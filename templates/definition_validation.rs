//! Portable, versioned authoring and structured-output validation.
//!
//! This module deliberately uses only serde_json and the standard library so the
//! CLI, generated build scripts and hosted input boundary share one contract.

#![allow(dead_code)]

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const STRICT_SCHEMA_VERSION: &str = "2026-09-09.r1";
pub const VERSION_KEY: &str = "agent_definition_schema_version";
pub const MAX_STRING_BYTES: usize = 256 * 1024;
pub const MAX_KEY_BYTES: usize = 256;
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;
pub const MAX_NODES: usize = 65_536;
pub const MAX_DEPTH: usize = 32;
pub const MAX_COLLECTION: usize = 4_096;
pub const MAX_DECLARATIONS: usize = 256;
pub const MAX_PARTS: usize = 1_024;
pub const MAX_WORK: usize = 262_144;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefinitionRevision {
    Legacy,
    Strict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DefinitionValidationError {
    pub code: &'static str,
    pub path: String,
    pub message: String,
    pub expected_keys: Vec<String>,
    pub corrective_action: String,
    pub limit: Option<&'static str>,
    pub maximum: Option<usize>,
    pub observed: Option<usize>,
}

impl DefinitionValidationError {
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "code": self.code,
            "path": self.path,
            "message": self.message,
            "expected_keys": self.expected_keys,
            "corrective_action": self.corrective_action,
        });
        if let Some(limit) = self.limit {
            value["limit"] = json!(limit);
            value["maximum"] = json!(self.maximum);
            value["observed"] = json!(self.observed);
        }
        value
    }
}

impl fmt::Display for DefinitionValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at {}: {}", self.code, self.path, self.message)?;
        if !self.expected_keys.is_empty() {
            write!(f, " Expected keys: {}.", self.expected_keys.join(", "))?;
        }
        write!(f, " {}", self.corrective_action)
    }
}
impl std::error::Error for DefinitionValidationError {}
type Result<T> = std::result::Result<T, DefinitionValidationError>;

fn error(
    code: &'static str,
    path: &str,
    message: impl Into<String>,
    correction: &str,
) -> DefinitionValidationError {
    DefinitionValidationError {
        code,
        path: path.into(),
        message: message.into(),
        expected_keys: vec![],
        corrective_action: correction.into(),
        limit: None,
        maximum: None,
        observed: None,
    }
}
fn invalid(path: &str, message: impl Into<String>) -> DefinitionValidationError {
    error(
        "invalid_value",
        path,
        message,
        "Use a value supported by this definition contract.",
    )
}
fn wrong_type(path: &str, expected: &str) -> DefinitionValidationError {
    error(
        "invalid_type",
        path,
        format!("Expected {expected}."),
        "Replace the value with the expected JSON type.",
    )
}
fn limit(path: &str, name: &'static str, observed: usize, maximum: usize) -> Result<()> {
    if observed <= maximum {
        return Ok(());
    }
    let mut e = error(
        "limit_exceeded",
        path,
        format!("{name} is {observed}; maximum is {maximum}."),
        "Reduce this value or split the application data into smaller declared inputs.",
    );
    e.limit = Some(name);
    e.maximum = Some(maximum);
    e.observed = Some(observed);
    Err(e)
}
pub fn field_path(parent: &str, key: &str) -> String {
    let mut chars = key.chars();
    let simple = chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric());
    if simple {
        format!("{parent}.{key}")
    } else {
        format!(
            "{parent}[{}]",
            serde_json::to_string(key).expect("string serialization")
        )
    }
}
fn index_path(parent: &str, index: usize) -> String {
    format!("{parent}[{index}]")
}
fn object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| wrong_type(path, "an object"))
}
fn ordered_entries(map: &Map<String, Value>) -> impl DoubleEndedIterator<Item = (&String, &Value)> {
    // serde_json may preserve insertion order in one consumer and sort in another.
    let mut entries = map.iter().collect::<Vec<_>>();
    entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
    entries.into_iter()
}
fn array<'a>(value: &'a Value, path: &str) -> Result<&'a [Value]> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| wrong_type(path, "an array"))
}
fn string<'a>(value: &'a Value, path: &str) -> Result<&'a str> {
    value.as_str().ok_or_else(|| wrong_type(path, "a string"))
}
fn nonempty<'a>(value: &'a Value, path: &str) -> Result<&'a str> {
    let s = string(value, path)?;
    if s.trim().is_empty() {
        return Err(invalid(path, "Expected a non-empty string."));
    }
    Ok(s)
}
fn required<'a>(map: &'a Map<String, Value>, key: &str, path: &str) -> Result<&'a Value> {
    map.get(key).ok_or_else(|| {
        let mut e = error(
            "missing_required_field",
            &field_path(path, key),
            "missing required field.",
            "Add the required field using its documented type.",
        );
        e.expected_keys.push(key.into());
        e
    })
}
fn keys(map: &Map<String, Value>, allowed: &[&str], path: &str, schema: bool) -> Result<()> {
    for (key, _) in ordered_entries(map) {
        if !allowed.contains(&key.as_str()) {
            let code = if schema
                && (key.starts_with('$')
                    || matches!(
                        key.as_str(),
                        "format"
                            | "allOf"
                            | "anyOf"
                            | "oneOf"
                            | "not"
                            | "if"
                            | "then"
                            | "else"
                            | "required"
                            | "additionalProperties"
                            | "pattern"
                            | "minLength"
                            | "maxLength"
                            | "minItems"
                            | "maxItems"
                    )) {
                "unsupported_schema_keyword"
            } else {
                "unknown_field"
            };
            let mut e = error(
                code,
                &field_path(path, key),
                "This field is not supported here.",
                "Remove or correct the field; use a supported schema revision for a different contract.",
            );
            e.expected_keys = allowed.iter().map(|s| s.to_string()).collect();
            e.expected_keys.sort();
            return Err(e);
        }
    }
    Ok(())
}

/// Syntax parsing is independent from the supported-revision policy.
pub fn parse_schema_version(value: &str) -> Option<(u32, u32, u32, u32)> {
    let (date, revision) = value.split_once(".r")?;
    let b = date.as_bytes();
    if b.len() != 10
        || b[4] != b'-'
        || b[7] != b'-'
        || !b
            .iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
    {
        return None;
    }
    if revision.is_empty() || !revision.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (year, month, day, revision) = (
        date[0..4].parse().ok()?,
        date[5..7].parse().ok()?,
        date[8..10].parse().ok()?,
        revision.parse().ok()?,
    );
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if day == 0 || day > days || revision == 0 {
        return None;
    }
    Some((year, month, day, revision))
}
pub fn definition_revision(root: &Value) -> Result<DefinitionRevision> {
    let root = object(root, "$")?;
    if root.contains_key("version") {
        return Err(error(
            "legacy_schema_key",
            "$.version",
            "The legacy schema key `version` is no longer supported.",
            "rename it to `agent_definition_schema_version` and keep its existing schema-version value.",
        ));
    }
    let path = "$.agent_definition_schema_version";
    let value = string(required(root, VERSION_KEY, "$")?, path)?;
    let parsed = parse_schema_version(value).ok_or_else(|| {
        invalid(
            path,
            "Expected schema version format YYYY-MM-DD.rN with a valid date and positive revision.",
        )
    })?;
    let strict = (2026, 9, 9, 1);
    if parsed < strict {
        Ok(DefinitionRevision::Legacy)
    } else if value == STRICT_SCHEMA_VERSION {
        Ok(DefinitionRevision::Strict)
    } else {
        Err(error(
            "unsupported_schema_version",
            path,
            "This Cargo AI build does not support the requested definition revision.",
            "Upgrade Cargo AI or review and migrate the definition to a supported contract; do not guess or blindly replace its version.",
        ))
    }
}
pub fn parse_definition(raw: &str) -> Result<(Value, DefinitionRevision)> {
    let value = serde_json::from_str(raw).map_err(|cause| {
        error(
            "invalid_json",
            "$",
            &format!("The definition is not valid JSON: {cause}"),
            "Correct the JSON syntax before validating again.",
        )
    })?;
    let revision = validate_definition(&value)?;
    Ok((value, revision))
}

#[derive(Default)]
struct Budget {
    work: usize,
}
impl Budget {
    fn charge(&mut self, path: &str, amount: usize) -> Result<()> {
        self.work = self.work.saturating_add(amount);
        limit(path, "evaluation_work", self.work, MAX_WORK)
    }
}
fn data_budget(value: &Value, path: &str) -> Result<Budget> {
    let mut stack = vec![(value, path.to_string(), 0usize)];
    let mut nodes = 0usize;
    let mut bytes = 0usize;
    let mut budget = Budget::default();
    while let Some((value, path, depth)) = stack.pop() {
        nodes += 1;
        limit(&path, "json_nodes", nodes, MAX_NODES)?;
        limit(&path, "nesting_depth", depth, MAX_DEPTH)?;
        budget.charge(&path, 1)?;
        match value {
            Value::String(s) => {
                limit(&path, "string_bytes", s.len(), MAX_STRING_BYTES)?;
                bytes = bytes.saturating_add(s.len());
            }
            Value::Array(items) => {
                limit(&path, "array_entries", items.len(), MAX_COLLECTION)?;
                for (i, item) in items.iter().enumerate().rev() {
                    stack.push((item, index_path(&path, i), depth + 1));
                }
            }
            Value::Object(map) => {
                limit(&path, "object_members", map.len(), MAX_COLLECTION)?;
                for (key, value) in ordered_entries(map).rev() {
                    let p = field_path(&path, key);
                    limit(&p, "key_bytes", key.len(), MAX_KEY_BYTES)?;
                    bytes = bytes.saturating_add(key.len());
                    stack.push((value, p, depth + 1));
                }
            }
            _ => {}
        }
        limit(&path, "decoded_text_bytes", bytes, MAX_TEXT_BYTES)?;
    }
    Ok(budget)
}
pub fn validate_data_limits(value: &Value, path: &str) -> Result<()> {
    data_budget(value, path).map(|_| ())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    String,
    Boolean,
    Integer,
    Number,
    Array,
    Object,
}
impl Kind {
    fn structured(self) -> bool {
        matches!(self, Self::Array | Self::Object)
    }
    fn numeric(self) -> bool {
        matches!(self, Self::Integer | Self::Number)
    }
    fn matches(self, value: &Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Boolean => value.is_boolean(),
            Self::Integer => value.as_i64().is_some(),
            Self::Number => value.as_f64().is_some_and(f64::is_finite),
            Self::Array => value.is_array(),
            Self::Object => value.is_object(),
        }
    }
}
fn kind(value: &str, path: &str) -> Result<Kind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "string" => Ok(Kind::String),
        "boolean" => Ok(Kind::Boolean),
        "integer" => Ok(Kind::Integer),
        "number" => Ok(Kind::Number),
        "array" => Ok(Kind::Array),
        "object" => Ok(Kind::Object),
        _ => Err(invalid(path, "Unsupported schema type.")),
    }
}
fn schema_type(
    map: &Map<String, Value>,
    path: &str,
    nullable_allowed: bool,
) -> Result<(Kind, bool)> {
    let p = field_path(path, "type");
    let value = required(map, "type", path)?;
    if let Some(s) = value.as_str() {
        return Ok((kind(s, &p)?, false));
    }
    let entries = array(value, &p)?;
    if !nullable_allowed || entries.len() != 2 {
        return Err(invalid(
            &p,
            "Only a scalar and null union inside an object property is supported.",
        ));
    }
    let a = string(&entries[0], &index_path(&p, 0))?
        .trim()
        .to_ascii_lowercase();
    let b = string(&entries[1], &index_path(&p, 1))?
        .trim()
        .to_ascii_lowercase();
    let scalar = if a == "null" {
        b
    } else if b == "null" {
        a
    } else {
        return Err(invalid(
            &p,
            "A nullable type must contain null and one scalar type.",
        ));
    };
    let k = kind(&scalar, &p)?;
    if k.structured() {
        return Err(invalid(
            &p,
            "Only scalar object properties may be nullable.",
        ));
    }
    Ok((k, true))
}
fn identifier(name: &str, path: &str, rust: bool) -> Result<()> {
    if name.is_empty()
        || name != name.trim()
        || name.chars().any(char::is_whitespace)
        || name.contains('.')
    {
        return Err(invalid(
            path,
            "Names must be nonempty and flat, without whitespace or dot separators.",
        ));
    }
    if name == "runtime" {
        return Err(invalid(
            path,
            "The name runtime is reserved for invocation-scoped variables.",
        ));
    }
    if rust {
        let mut chars = name.chars();
        if !chars
            .next()
            .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
            || !chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
            || matches!(
                name,
                "_" | "as"
                    | "break"
                    | "const"
                    | "continue"
                    | "crate"
                    | "else"
                    | "enum"
                    | "extern"
                    | "false"
                    | "fn"
                    | "for"
                    | "if"
                    | "impl"
                    | "in"
                    | "let"
                    | "loop"
                    | "match"
                    | "mod"
                    | "move"
                    | "mut"
                    | "pub"
                    | "ref"
                    | "return"
                    | "self"
                    | "Self"
                    | "static"
                    | "struct"
                    | "super"
                    | "trait"
                    | "true"
                    | "type"
                    | "unsafe"
                    | "use"
                    | "where"
                    | "while"
                    | "async"
                    | "await"
                    | "dyn"
                    | "union"
                    | "abstract"
                    | "become"
                    | "box"
                    | "do"
                    | "final"
                    | "macro"
                    | "override"
                    | "priv"
                    | "typeof"
                    | "unsized"
                    | "virtual"
                    | "yield"
                    | "try"
            )
        {
            return Err(invalid(path,"Output property names must be portable Rust identifiers and cannot be reserved keywords."));
        }
    }
    Ok(())
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum SchemaContext {
    TopLevel,
    ArrayItem,
    ObjectProperty,
}

fn descriptor(
    value: &Value,
    path: &str,
    context: SchemaContext,
    budget: &mut Budget,
) -> Result<Kind> {
    budget.charge(path, 1)?;
    let map = object(value, path)?;
    let (k, _) = schema_type(map, path, context == SchemaContext::ObjectProperty)?;
    let allowed: &[&str] = match k {
        Kind::String => &["type", "description", "enum"],
        Kind::Boolean => &["type", "description"],
        Kind::Integer | Kind::Number => &[
            "type",
            "description",
            "minimum",
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
        ],
        Kind::Array => &["type", "description", "items"],
        Kind::Object => &["type", "description", "properties"],
    };
    keys(map, allowed, path, true)?;
    if let Some(value) = map.get("description") {
        nonempty(value, &field_path(path, "description"))?;
    }
    if context == SchemaContext::ObjectProperty && k.structured() {
        return Err(invalid(
            &field_path(path, "type"),
            "Object properties inside structured fields must be scalar.",
        ));
    }
    if context == SchemaContext::ArrayItem && k == Kind::Array {
        return Err(invalid(
            &field_path(path, "type"),
            "Nested arrays are not supported.",
        ));
    }
    if let Some(values) = map.get("enum") {
        let p = field_path(path, "enum");
        let values = array(values, &p)?;
        if values.is_empty() {
            return Err(invalid(&p, "Enum must contain at least one string."));
        }
        limit(&p, "enum_entries", values.len(), MAX_PARTS)?;
        let mut seen = BTreeSet::new();
        for (i, v) in values.iter().enumerate() {
            let value_path = index_path(&p, i);
            budget.charge(&value_path, 1)?;
            let value = nonempty(v, &value_path)?;
            if !seen.insert(value) {
                return Err(invalid(&value_path, "Enum values must be unique."));
            }
        }
    }
    if k.numeric() {
        numeric_schema(map, path, k, budget)?;
    }
    if k == Kind::Array {
        descriptor(
            required(map, "items", path)?,
            &field_path(path, "items"),
            SchemaContext::ArrayItem,
            budget,
        )?;
    }
    if k == Kind::Object {
        let p = field_path(path, "properties");
        let props = object(required(map, "properties", path)?, &p)?;
        if props.is_empty() {
            return Err(invalid(
                &p,
                "An object field must declare at least one property.",
            ));
        }
        limit(&p, "schema_properties", props.len(), MAX_DECLARATIONS)?;
        for (name, prop) in ordered_entries(props) {
            descriptor(
                prop,
                &field_path(&p, name),
                SchemaContext::ObjectProperty,
                budget,
            )?;
        }
    }
    Ok(k)
}
fn numeric_schema(
    map: &Map<String, Value>,
    path: &str,
    k: Kind,
    budget: &mut Budget,
) -> Result<()> {
    for key in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
        if let Some(v) = map.get(key) {
            budget.charge(path, 1)?;
            if !k.matches(v) {
                return Err(wrong_type(
                    &field_path(path, key),
                    if k == Kind::Integer {
                        "an integer bound"
                    } else {
                        "a finite numeric bound"
                    },
                ));
            }
        }
    }
    if map.contains_key("minimum") && map.contains_key("exclusiveMinimum") {
        return Err(invalid(
            &field_path(path, "exclusiveMinimum"),
            "Choose minimum or exclusiveMinimum, not both.",
        ));
    }
    if map.contains_key("maximum") && map.contains_key("exclusiveMaximum") {
        return Err(invalid(
            &field_path(path, "exclusiveMaximum"),
            "Choose maximum or exclusiveMaximum, not both.",
        ));
    }
    let lower = map.get("minimum").or_else(|| map.get("exclusiveMinimum"));
    let upper = map.get("maximum").or_else(|| map.get("exclusiveMaximum"));
    if let (Some(a), Some(b)) = (lower, upper) {
        let exclusive =
            map.contains_key("exclusiveMinimum") || map.contains_key("exclusiveMaximum");
        let cmp = if k == Kind::Integer {
            a.as_i64().unwrap().cmp(&b.as_i64().unwrap())
        } else {
            a.as_f64()
                .unwrap()
                .partial_cmp(&b.as_f64().unwrap())
                .unwrap()
        };
        if cmp.is_gt() || (exclusive && cmp.is_eq()) {
            return Err(invalid(path, "Numeric bounds leave no permitted value."));
        }
    }
    Ok(())
}
fn owned_path(raw: &str, path: &str, file: bool) -> Result<()> {
    let normalized = raw.trim();
    if normalized.is_empty()
        || normalized.starts_with(['/', '\\'])
        || normalized.as_bytes().get(1) == Some(&b':')
        || normalized.contains('\0')
        || normalized.split(['/', '\\']).any(|part| part == "..")
    {
        return Err(invalid(
            path,
            "Use a relative owned path without absolute prefixes or parent traversal.",
        ));
    }
    if file {
        let ext = path_extension(raw);
        if ![
            "pdf", "docx", "csv", "xla", "xlb", "xlc", "xlm", "xls", "xlsx", "xlt", "xlw", "tsv",
            "iif", "doc", "dot", "odt", "rtf", "pot", "ppa", "pps", "ppt", "pptx", "pwz", "wiz",
        ]
        .contains(&ext.as_str())
        {
            return Err(invalid(path, "Unsupported file input extension."));
        }
    }
    Ok(())
}
fn path_extension(raw: &str) -> String {
    raw.rsplit(['/', '\\'])
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .filter(|(stem, extension)| !stem.is_empty() && !extension.is_empty())
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default()
}
fn tool_identifier(name: &str, path: &str) -> Result<()> {
    if name.trim().is_empty() || name != name.trim() || name.chars().any(char::is_whitespace) {
        return Err(invalid(
            path,
            "Tool and parameter names must be nonempty without whitespace.",
        ));
    }
    Ok(())
}
fn capture_identifier(name: &str, path: &str) -> Result<()> {
    if name.is_empty() || name.contains('.') || name == "runtime" {
        return Err(invalid(
            path,
            "Capture names must be nonempty and flat; runtime is reserved.",
        ));
    }
    Ok(())
}
fn child_path(raw: &str, path: &str) -> Result<()> {
    fn package_id(s: &str) -> bool {
        s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }
    if let Some((a, b)) = raw.split_once("::") {
        if package_id(a) && package_id(b) {
            return Ok(());
        }
    } else if let Some(s) = raw.strip_prefix("./") {
        if !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', ':', '\0']) {
            return Ok(());
        }
    }
    Err(invalid(
        path,
        "Use an explicit same-level ./child artifact or declared alias::entrypoint.",
    ))
}

pub fn validate_definition(value: &Value) -> Result<DefinitionRevision> {
    let revision = definition_revision(value)?;
    if revision == DefinitionRevision::Legacy {
        return Ok(revision);
    }
    let mut budget = data_budget(value, "$")?;
    let root = object(value, "$")?;
    keys(
        root,
        &[
            VERSION_KEY,
            "agent_schema",
            "actions",
            "inputs",
            "runtime_vars",
            "action_execution",
        ],
        "$",
        false,
    )?;
    if let Some(execution) = root.get("action_execution") {
        choice(execution, "$.action_execution", &["sequential", "parallel"])?;
    }
    let schema = object(required(root, "agent_schema", "$")?, "$.agent_schema")?;
    keys(schema, &["type", "properties"], "$.agent_schema", true)?;
    if string(
        required(schema, "type", "$.agent_schema")?,
        "$.agent_schema.type",
    )? != "object"
    {
        return Err(invalid(
            "$.agent_schema.type",
            "The output schema must be an object.",
        ));
    }
    let props = object(
        required(schema, "properties", "$.agent_schema")?,
        "$.agent_schema.properties",
    )?;
    limit(
        "$.agent_schema.properties",
        "schema_properties",
        props.len(),
        MAX_DECLARATIONS,
    )?;
    let mut fields = BTreeMap::new();
    for (name, prop) in ordered_entries(props) {
        let p = field_path("$.agent_schema.properties", name);
        identifier(name, &p, true)?;
        fields.insert(
            name.clone(),
            descriptor(prop, &p, SchemaContext::TopLevel, &mut budget)?,
        );
    }
    if let Some(vars) = root.get("runtime_vars") {
        let vars = object(vars, "$.runtime_vars")?;
        limit(
            "$.runtime_vars",
            "runtime_variables",
            vars.len(),
            MAX_DECLARATIONS,
        )?;
        for (name, value) in ordered_entries(vars) {
            let p = field_path("$.runtime_vars", name);
            identifier(name, &p, false)?;
            let map = object(value, &p)?;
            keys(map, &["type", "default"], &p, false)?;
            let kp = field_path(&p, "type");
            let raw = required(map, "type", &p)?;
            exact_choice(raw, &kp, &["string", "boolean", "number", "integer"])?;
            let k = kind(string(raw, &kp)?, &kp)?;
            if k.structured() {
                return Err(invalid(&kp, "Runtime variables must be scalar."));
            }
            if let Some(v) = map.get("default") {
                if !k.matches(v) {
                    return Err(wrong_type(
                        &field_path(&p, "default"),
                        "the declared scalar type",
                    ));
                }
            }
            fields.insert(format!("runtime.{name}"), k);
        }
    }
    let mut inputs = BTreeMap::new();
    if let Some(values) = root.get("inputs") {
        let values = array(values, "$.inputs")?;
        if values.is_empty() {
            return Err(invalid(
                "$.inputs",
                "Inputs must contain at least one entry when present.",
            ));
        }
        limit("$.inputs", "inputs", values.len(), MAX_DECLARATIONS)?;
        for (i, value) in values.iter().enumerate() {
            let p = index_path("$.inputs", i);
            let (name, k) = top_input(value, &p, props.is_empty())?;
            if let Some(name) = name {
                if inputs.insert(name, k).is_some() {
                    return Err(error(
                        "duplicate_name",
                        &field_path(&p, "name"),
                        "Named inputs must be unique.",
                        "Choose a distinct named input.",
                    ));
                }
            }
        }
    }
    let actions = array(required(root, "actions", "$")?, "$.actions")?;
    limit("$.actions", "actions", actions.len(), MAX_DECLARATIONS)?;
    let mut total_steps = 0;
    for (i, value) in actions.iter().enumerate() {
        let p = index_path("$.actions", i);
        let action = object(value, &p)?;
        keys(action, &["name", "logic", "run"], &p, false)?;
        nonempty(required(action, "name", &p)?, &field_path(&p, "name"))?;
        logic(
            required(action, "logic", &p)?,
            &field_path(&p, "logic"),
            &fields,
            &mut budget,
        )?;
        let rp = field_path(&p, "run");
        let steps = array(required(action, "run", &p)?, &rp)?;
        if steps.is_empty() {
            return Err(invalid(
                &rp,
                "Each action must contain at least one run step.",
            ));
        }
        limit(&rp, "steps_per_action", steps.len(), MAX_DECLARATIONS)?;
        total_steps += steps.len();
        limit(&rp, "total_steps", total_steps, MAX_COLLECTION)?;
        let mut available = fields.clone();
        let mut captures = BTreeSet::new();
        for (j, step) in steps.iter().enumerate() {
            run_step(
                step,
                &index_path(&rp, j),
                &fields,
                &mut available,
                &mut captures,
                &inputs,
                &mut budget,
            )?;
        }
    }
    Ok(revision)
}
fn choice<'a>(value: &'a Value, path: &str, allowed: &[&str]) -> Result<&'a str> {
    let raw = string(value, path)?;
    if allowed.contains(&raw.trim().to_ascii_lowercase().as_str()) {
        Ok(raw)
    } else {
        Err(invalid(
            path,
            format!("Expected one of: {}.", allowed.join(", ")),
        ))
    }
}
fn exact_choice(value: &Value, path: &str, allowed: &[&str]) -> Result<()> {
    let raw = string(value, path)?;
    if allowed.contains(&raw.trim()) {
        Ok(())
    } else {
        Err(invalid(
            path,
            format!("Expected one of: {}.", allowed.join(", ")),
        ))
    }
}
fn top_input<'a>(
    value: &'a Value,
    path: &str,
    named_required: bool,
) -> Result<(Option<String>, String)> {
    let map = object(value, path)?;
    let tp = field_path(path, "type");
    let k = choice(
        required(map, "type", path)?,
        &tp,
        &["text", "url", "image", "file"],
    )?
    .trim()
    .to_ascii_lowercase();
    let value_key = match k.as_str() {
        "text" => "text",
        "url" => "url",
        _ => "path",
    };
    keys(map, &["type", "name", value_key], path, false)?;
    let name = map
        .get("name")
        .map(|v| {
            let p = field_path(path, "name");
            let name = nonempty(v, &p)?;
            identifier(name, &p, false)?;
            Ok(name.to_string())
        })
        .transpose()?;
    if named_required && name.is_none() {
        required(map, "name", path)?;
    }
    if let Some(v) = map.get(value_key) {
        let p = field_path(path, value_key);
        let s = string(v, &p)?;
        if value_key == "path" {
            owned_path(s.trim(), &p, k == "file")?;
        }
    } else if name.is_none() {
        required(map, value_key, path)?;
    }
    Ok((name, k))
}
fn reference(
    value: &Value,
    path: &str,
    fields: &BTreeMap<String, Kind>,
    scalar: bool,
) -> Result<Kind> {
    reference_name(nonempty(value, path)?.trim(), path, fields, scalar)
}
fn reference_name(
    name: &str,
    path: &str,
    fields: &BTreeMap<String, Kind>,
    scalar: bool,
) -> Result<Kind> {
    let k = fields.get(name).copied().ok_or_else(|| {
        error(
            "invalid_reference",
            path,
            "The variable is not available at this point.",
            "Use a declared output/runtime variable or an earlier capture in this action.",
        )
    })?;
    if scalar && k.structured() {
        return Err(error(
            "invalid_reference",
            path,
            "Structured output fields may only flow into tool parameters.",
            "Pass the complete structured value to a declared tool parameter.",
        ));
    }
    Ok(k)
}
fn parts(
    value: &Value,
    path: &str,
    fields: &BTreeMap<String, Kind>,
    budget: &mut Budget,
) -> Result<Option<String>> {
    if let Some(s) = value.as_str() {
        if s.trim().is_empty() {
            return Err(invalid(path, "Expected a non-empty string."));
        }
        return Ok(Some(s.into()));
    }
    let values = array(value, path)?;
    if values.is_empty() {
        return Err(invalid(path, "String parts must not be empty."));
    }
    limit(path, "string_parts", values.len(), MAX_PARTS)?;
    let mut literal = Some(String::new());
    for (i, value) in values.iter().enumerate() {
        let p = index_path(path, i);
        budget.charge(&p, 1)?;
        if let Some(s) = value.as_str() {
            if let Some(text) = literal.as_mut() {
                text.push_str(s);
            }
        } else {
            let map = object(value, &p)?;
            keys(map, &["var"], &p, false)?;
            reference(
                required(map, "var", &p)?,
                &field_path(&p, "var"),
                fields,
                true,
            )?;
            literal = None;
        }
    }
    Ok(literal)
}
fn scalar_or_reference(
    value: &Value,
    path: &str,
    fields: &BTreeMap<String, Kind>,
    string_only: bool,
) -> Result<()> {
    if let Value::Object(map) = value {
        keys(map, &["var"], path, false)?;
        let p = field_path(path, "var");
        let k = reference(required(map, "var", path)?, &p, fields, true)?;
        if string_only && k != Kind::String {
            return Err(wrong_type(&p, "a string variable"));
        }
    } else if string_only {
        nonempty(value, path)?;
    } else if !(value.is_string() || value.is_boolean() || value.is_number()) {
        return Err(wrong_type(
            path,
            "a string, boolean or number, or a var reference",
        ));
    }
    Ok(())
}
const OPERATORS: &[&str] = &[
    "==",
    "===",
    "!=",
    "!==",
    "var",
    "!",
    "!!",
    "if",
    "or",
    "and",
    "<",
    "<=",
    ">",
    ">=",
    "missing",
    "missing_some",
    "min",
    "max",
    "+",
    "-",
    "*",
    "/",
    "%",
    "in",
    "cat",
    "substr",
    "log",
    "merge",
    "map",
    "filter",
    "reduce",
    "all",
    "some",
    "none",
];
fn logic(
    value: &Value,
    path: &str,
    fields: &BTreeMap<String, Kind>,
    budget: &mut Budget,
) -> Result<Kind> {
    budget.charge(path, 1)?;
    let map = object(value, path)?;
    if map.len() != 1 {
        return Err(invalid(
            path,
            "A logic expression must have exactly one operator.",
        ));
    }
    let (op, args) = map.iter().next().unwrap();
    let p = field_path(path, op);
    if !OPERATORS.contains(&op.as_str()) {
        let mut e = error(
            "unsupported_operator",
            &p,
            "This operator is not supported by the runtime.",
            "Use a supported runtime operator; for an unconditional action use {\"==\":[1,1]}.",
        );
        e.expected_keys = OPERATORS.iter().map(|s| s.to_string()).collect();
        e.expected_keys.sort();
        return Err(e);
    }
    if op == "var" {
        let arg = if let Some(items) = args.as_array() {
            items
                .first()
                .ok_or_else(|| invalid(&p, "A var reference must include a name."))?
        } else {
            args
        };
        if let Some(items) = args.as_array() {
            for (index, default) in items.iter().enumerate().skip(1) {
                if default.is_object() {
                    logic(default, &index_path(&p, index), fields, budget)?;
                }
            }
        }
        return reference_name(nonempty(arg, &p)?, &p, fields, true);
    }
    let operands: Vec<&Value> = if let Some(items) = args.as_array() {
        items.iter().collect()
    } else {
        vec![args]
    };
    for (i, arg) in operands.iter().enumerate() {
        if arg.is_object() {
            let argument_path = if args.is_array() {
                index_path(&p, i)
            } else {
                p.clone()
            };
            logic(arg, &argument_path, fields, budget)?;
        }
    }
    if matches!(op.as_str(), "==" | "!=" | ">" | ">=" | "<" | "<=") {
        if !args.is_array() || operands.len() != 2 {
            return Err(invalid(
                &p,
                "This comparison requires exactly two operands.",
            ));
        }
        let a = logic_kind(operands[0], &index_path(&p, 0), fields, budget)?;
        let b = logic_kind(operands[1], &index_path(&p, 1), fields, budget)?;
        if matches!(op.as_str(), ">" | ">=" | "<" | "<=") && (!a.numeric() || !b.numeric()) {
            return Err(invalid(&p, "Ordered comparison operands must be numeric."));
        }
        if matches!(op.as_str(), "==" | "!=") && a != b && !(a.numeric() && b.numeric()) {
            return Err(invalid(
                &p,
                "Comparison operands must have compatible types.",
            ));
        }
        return Ok(Kind::Boolean);
    }
    Ok(if matches!(op.as_str(), "and" | "or" | "!") {
        Kind::Boolean
    } else {
        Kind::String
    })
}
fn logic_kind(
    value: &Value,
    path: &str,
    fields: &BTreeMap<String, Kind>,
    budget: &mut Budget,
) -> Result<Kind> {
    match value {
        Value::String(_) => Ok(Kind::String),
        Value::Null => Err(invalid(path, "Null comparison operands are not supported.")),
        Value::Bool(_) => Ok(Kind::Boolean),
        Value::Number(n) => Ok(if n.is_i64() {
            Kind::Integer
        } else {
            Kind::Number
        }),
        Value::Array(_) => Ok(Kind::Array),
        Value::Object(_) => logic(value, path, fields, budget),
    }
}

fn run_step(
    value: &Value,
    path: &str,
    fields: &BTreeMap<String, Kind>,
    available: &mut BTreeMap<String, Kind>,
    captures: &mut BTreeSet<String>,
    inputs: &BTreeMap<String, String>,
    budget: &mut Budget,
) -> Result<()> {
    budget.charge(path, 1)?;
    let map = object(value, path)?;
    let kp = field_path(path, "kind");
    let k = string(required(map, "kind", path)?, &kp)?;
    let mut allowed = vec![
        "kind",
        "platform",
        "when",
        "failure_mode",
        "status_variable",
        "error_variable",
    ];
    allowed.extend_from_slice(match k {
        "exec" => &["program", "args", "output_variable"][..],
        "email_me" => &["subject", "text"],
        "agent" => &[
            "artifact",
            "agent",
            "profile",
            "usage_log",
            "inputs",
            "input_mode",
            "input_overrides",
            "run_vars",
            "ignore_tools",
        ],
        "tool" => &["name", "params", "output_variable"],
        "generate_image" => &["prompt", "path", "model", "profile", "reference_images"],
        _ => {
            return Err(invalid(
                &kp,
                "Supported kinds are exec, email_me, agent, tool and generate_image.",
            ))
        }
    });
    keys(map, &allowed, path, false)?;
    if let Some(v) = map.get("when") {
        logic(v, &field_path(path, "when"), available, budget)?;
    }
    if let Some(v) = map.get("failure_mode") {
        exact_choice(
            v,
            &field_path(path, "failure_mode"),
            &["continue", "stop", "abort"],
        )?;
    }
    if let Some(v) = map.get("platform") {
        let p = field_path(path, "platform");
        let entries = if let Some(a) = v.as_array() {
            if a.is_empty() {
                return Err(invalid(&p, "At least one platform is required."));
            }
            a.iter().collect::<Vec<_>>()
        } else {
            vec![v]
        };
        let mut seen = BTreeSet::new();
        for (i, v) in entries.iter().enumerate() {
            let ip = if v.is_string() && !map["platform"].is_array() {
                p.clone()
            } else {
                index_path(&p, i)
            };
            let platform = choice(v, &ip, &["macos", "linux", "windows"])?
                .trim()
                .to_ascii_lowercase();
            if !seen.insert(platform) {
                return Err(invalid(&ip, "Platform entries must be unique."));
            }
        }
    }
    match k {
        "exec" => {
            nonempty(
                required(map, "program", path)?,
                &field_path(path, "program"),
            )?;
            let p = field_path(path, "args");
            let args = array(required(map, "args", path)?, &p)?;
            limit(&p, "string_parts", args.len(), MAX_PARTS)?;
            for (i, arg) in args.iter().enumerate() {
                if !arg.is_string() {
                    let ap = index_path(&p, i);
                    let obj = object(arg, &ap)?;
                    keys(obj, &["var"], &ap, false)?;
                    reference(
                        required(obj, "var", &ap)?,
                        &field_path(&ap, "var"),
                        available,
                        true,
                    )?;
                }
            }
        }
        "email_me" => {
            for name in ["subject", "text"] {
                parts(
                    required(map, name, path)?,
                    &field_path(path, name),
                    available,
                    budget,
                )?;
            }
        }
        "agent" => {
            if map.contains_key("artifact") && map.contains_key("agent") {
                return Err(invalid(path, "Specify exactly one of artifact or agent."));
            }
            let key = if map.contains_key("agent") {
                "agent"
            } else {
                "artifact"
            };
            let p = field_path(path, key);
            child_path(nonempty(required(map, key, path)?, &p)?.trim(), &p)?;
            if let Some(v) = map.get("profile") {
                scalar_or_reference(v, &field_path(path, "profile"), fields, true)?;
            }
            if let Some(v) = map.get("usage_log") {
                let p = field_path(path, "usage_log");
                owned_path(nonempty(v, &p)?, &p, false)?;
            }
            if let Some(v) = map.get("ignore_tools") {
                if !v.is_boolean() {
                    return Err(wrong_type(&field_path(path, "ignore_tools"), "a boolean"));
                }
            }
            if let Some(v) = map.get("input_mode") {
                let p = field_path(path, "input_mode");
                exact_choice(v, &p, &["replace", "append", "prepend"])?;
                if !map.contains_key("inputs") {
                    return Err(invalid(&p, "input_mode requires inputs."));
                }
            }
            if let Some(v) = map.get("inputs") {
                let p = field_path(path, "inputs");
                let a = array(v, &p)?;
                nonempty_collection(a.len(), &p, "child_inputs")?;
                for (i, v) in a.iter().enumerate() {
                    child_input(v, &index_path(&p, i), available, inputs, budget)?;
                }
            }
            for name in ["run_vars", "input_overrides"] {
                if let Some(v) = map.get(name) {
                    let p = field_path(path, name);
                    let entries = object(v, &p)?;
                    nonempty_collection(
                        entries.len(),
                        &p,
                        if name == "run_vars" {
                            "child_run_variables"
                        } else {
                            "child_input_overrides"
                        },
                    )?;
                    for (name, v) in ordered_entries(entries) {
                        let ip = field_path(&p, name);
                        identifier(name, &ip, false)?;
                        if p.ends_with(".input_overrides") {
                            if let Some(obj) = v.as_object() {
                                if obj.contains_key("input") {
                                    keys(obj, &["input"], &ip, false)?;
                                    named_input(
                                        required(obj, "input", &ip)?,
                                        &field_path(&ip, "input"),
                                        inputs,
                                        false,
                                    )?;
                                    continue;
                                }
                            }
                            if v.is_object() {
                                scalar_or_reference(v, &ip, available, false)?;
                            } else {
                                string(v, &ip)?;
                            }
                        } else {
                            scalar_or_reference(v, &ip, available, false)?;
                        }
                    }
                }
            }
        }
        "tool" => {
            let p = field_path(path, "name");
            let name = nonempty(required(map, "name", path)?, &p)?;
            tool_identifier(name, &p)?;
            if let Some(v) = map.get("params") {
                let p = field_path(path, "params");
                let params = object(v, &p)?;
                limit(&p, "tool_params", params.len(), MAX_DECLARATIONS)?;
                for (name, v) in ordered_entries(params) {
                    let ip = field_path(&p, name);
                    tool_identifier(name, &ip)?;
                    if v.is_null() {
                        return Err(wrong_type(
                            &ip,
                            "a JSON literal other than null, or a var reference",
                        ));
                    }
                    if let Some(obj) = v.as_object() {
                        if obj.len() == 1 && obj.contains_key("var") {
                            reference(&obj["var"], &field_path(&ip, "var"), available, false)?;
                        }
                    }
                }
            }
        }
        "generate_image" => {
            parts(
                required(map, "prompt", path)?,
                &field_path(path, "prompt"),
                available,
                budget,
            )?;
            let p = field_path(path, "path");
            if let Some(s) = parts(required(map, "path", path)?, &p, available, budget)? {
                owned_path(&s, &p, false)?;
                let ext = path_extension(&s);
                if !["png", "jpg", "jpeg", "webp"].contains(&ext.as_str()) {
                    return Err(invalid(
                        &p,
                        "Generated image paths require png, jpg, jpeg or webp extension.",
                    ));
                }
            }
            for name in ["profile", "model"] {
                if let Some(v) = map.get(name) {
                    scalar_or_reference(v, &field_path(path, name), fields, true)?;
                }
            }
            if let Some(v) = map.get("reference_images") {
                let p = field_path(path, "reference_images");
                let entries = array(v, &p)?;
                nonempty_collection(entries.len(), &p, "reference_images")?;
                for (i, v) in entries.iter().enumerate() {
                    let ip = index_path(&p, i);
                    let obj = object(v, &ip)?;
                    if obj.contains_key("input") {
                        keys(obj, &["input"], &ip, false)?;
                        named_input(&obj["input"], &field_path(&ip, "input"), inputs, true)?;
                    } else {
                        keys(obj, &["path"], &ip, false)?;
                        let pp = field_path(&ip, "path");
                        if let Some(s) = parts(required(obj, "path", &ip)?, &pp, available, budget)?
                        {
                            owned_path(&s, &pp, false)?;
                        }
                    }
                }
            }
        }
        _ => unreachable!(),
    }
    let mut added = Vec::new();
    for name in ["output_variable", "status_variable", "error_variable"] {
        if let Some(v) = map.get(name) {
            let p = field_path(path, name);
            let raw = nonempty(v, &p)?.trim();
            capture_identifier(raw, &p)?;
            if fields.contains_key(raw)
                || captures.contains(raw)
                || added.iter().any(|s: &String| s == raw)
            {
                return Err(error(
                    "duplicate_name",
                    &p,
                    "Capture names must not collide with declared fields or another capture in this action.",
                    "Choose an unused flat capture name.",
                ));
            }
            added.push(raw.to_string());
        }
    }
    for name in added {
        captures.insert(name.clone());
        available.insert(name, Kind::String);
    }
    Ok(())
}
fn nonempty_collection(count: usize, path: &str, name: &'static str) -> Result<()> {
    if count == 0 {
        return Err(invalid(
            path,
            "This collection must not be empty when present.",
        ));
    }
    limit(path, name, count, MAX_DECLARATIONS)
}
fn named_input(
    value: &Value,
    path: &str,
    inputs: &BTreeMap<String, String>,
    image: bool,
) -> Result<()> {
    let name = nonempty(value, path)?;
    let k = inputs.get(name).ok_or_else(|| {
        error(
            "invalid_reference",
            path,
            "Unknown named parent input.",
            "Reference a declared named input.",
        )
    })?;
    if image && k != "image" {
        return Err(error(
            "invalid_reference",
            path,
            "Reference images must use image inputs.",
            "Reference a declared image input.",
        ));
    }
    Ok(())
}
fn child_input(
    value: &Value,
    path: &str,
    fields: &BTreeMap<String, Kind>,
    inputs: &BTreeMap<String, String>,
    budget: &mut Budget,
) -> Result<()> {
    let map = object(value, path)?;
    if map.contains_key("input") {
        keys(map, &["input"], path, false)?;
        return named_input(&map["input"], &field_path(path, "input"), inputs, false);
    }
    let tp = field_path(path, "type");
    let k = choice(
        required(map, "type", path)?,
        &tp,
        &["text", "url", "image", "file"],
    )?
    .trim()
    .to_ascii_lowercase();
    let key = match k.as_str() {
        "text" => "text",
        "url" => "url",
        _ => "path",
    };
    keys(map, &["type", key], path, false)?;
    let p = field_path(path, key);
    if let Some(raw) = parts(required(map, key, path)?, &p, fields, budget)? {
        if key == "path" {
            owned_path(&raw, &p, k == "file")?;
        }
    }
    Ok(())
}

/// Validate raw JSON before typed deserialization can discard fields.
pub fn validate_model_output(value: &Value, schema: &Value) -> Result<()> {
    let mut budget = data_budget(value, "$")?;
    output_value(value, schema, "$", &mut budget)
}
fn output_value(value: &Value, schema: &Value, path: &str, budget: &mut Budget) -> Result<()> {
    budget.charge(path, 1)?;
    let schema = object(schema, path)?;
    let (k, nullable) = schema_type(schema, path, true)?;
    if value.is_null() && nullable {
        return Ok(());
    }
    if !k.matches(value) {
        return Err(wrong_type(path, "the declared output type"));
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        let mut found = false;
        for expected in values {
            budget.charge(path, 1)?;
            if value == expected {
                found = true;
                break;
            }
        }
        if !found {
            return Err(invalid(
                path,
                "Output does not match an allowed enum value.",
            ));
        }
    }
    if k.numeric() {
        for (name, exclusive, lower) in [
            ("minimum", false, true),
            ("maximum", false, false),
            ("exclusiveMinimum", true, true),
            ("exclusiveMaximum", true, false),
        ] {
            if let Some(bound) = schema.get(name) {
                budget.charge(path, 1)?;
                let order = if k == Kind::Integer {
                    value.as_i64().unwrap().cmp(
                        &bound
                            .as_i64()
                            .ok_or_else(|| invalid(path, "Invalid integer schema bound."))?,
                    )
                } else {
                    value
                        .as_f64()
                        .unwrap()
                        .partial_cmp(
                            &bound
                                .as_f64()
                                .ok_or_else(|| invalid(path, "Invalid numeric schema bound."))?,
                        )
                        .ok_or_else(|| invalid(path, "Invalid finite numeric schema bound."))?
                };
                if (lower && order.is_lt())
                    || (!lower && order.is_gt())
                    || (exclusive && order.is_eq())
                {
                    return Err(invalid(path, "Output violates its declared numeric bound."));
                }
            }
        }
    }
    if k == Kind::Array {
        let items = required(schema, "items", path)?;
        for (i, v) in value.as_array().unwrap().iter().enumerate() {
            output_value(v, items, &index_path(path, i), budget)?;
        }
    }
    if k == Kind::Object {
        let props = object(required(schema, "properties", path)?, path)?;
        let values = value.as_object().unwrap();
        let allowed = props.keys().map(String::as_str).collect::<Vec<_>>();
        keys(values, &allowed, path, false)?;
        for (name, prop) in ordered_entries(props) {
            let p = field_path(path, name);
            let v = values.get(name).ok_or_else(|| {
                error(
                    "missing_required_field",
                    &p,
                    "Missing required output field.",
                    "Return every declared output property.",
                )
            })?;
            output_value(v, prop, &p, budget)?;
        }
    }
    Ok(())
}
