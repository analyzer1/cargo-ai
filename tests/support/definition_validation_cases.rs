//! Shared exact-boundary cases for the portable definition contract.
//!
//! Cases are delivered immediately so consumers need not retain several large
//! definitions. Limits are explicit expectations, independent of production
//! constants; changing the contract therefore requires updating its proof.

#![allow(dead_code)]

use serde_json::{json, Map, Value};

const STRING_BYTES: usize = 256 * 1024;
const KEY_BYTES: usize = 256;
const TEXT_BYTES: usize = 1024 * 1024;
const NODES: usize = 65_536;
const DEPTH: usize = 32;
const COLLECTION: usize = 4_096;
const DECLARATIONS: usize = 256;
const PARTS: usize = 1_024;
const DATA_PATH: &str = "$.actions[0].run[0].params.data";
const STEP_PATH: &str = "$.actions[0].run[0]";

/// Visit valid exact-limit definitions and definitions exceeding one limit.
///
/// A rejected case supplies its expected resource name and canonical JSON path.
/// Consumers should additionally require the `limit_exceeded` error code.
pub fn visit_limit_cases(mut visit: impl FnMut(&str, &Value, Option<(&str, &str)>)) {
    count_boundary(
        &mut visit,
        "string_bytes",
        STRING_BYTES,
        "string_bytes",
        DATA_PATH,
        |count| with_payload(Value::String(utf8_bytes(count))),
    );

    for extra in 0..=1 {
        let key = utf8_bytes(KEY_BYTES + extra);
        let definition = with_payload(Value::Object(Map::from_iter([(
            key.clone(),
            Value::Bool(true),
        )])));
        let path = field_path(DATA_PATH, &key);
        visit(
            &case_name("key_bytes", extra),
            &definition,
            (extra != 0).then_some(("key_bytes", path.as_str())),
        );
    }

    for extra in 0..=1 {
        let definition = definition_with_text_bytes(TEXT_BYTES + extra);
        // The final nonempty string crosses the aggregate ceiling by one byte.
        let path = last_string_path(&definition, "$")
            .expect("the definition contains nonempty string values");
        visit(
            &case_name("decoded_text_bytes", extra),
            &definition,
            (extra != 0).then_some(("decoded_text_bytes", path.as_str())),
        );
    }

    count_boundary(
        &mut visit,
        "object_members",
        COLLECTION,
        "object_members",
        DATA_PATH,
        |count| with_payload(named_map("key", count, |_| Value::Null)),
    );
    count_boundary(
        &mut visit,
        "array_entries",
        COLLECTION,
        "array_entries",
        DATA_PATH,
        |count| with_payload(Value::Array(vec![Value::Null; count])),
    );

    for extra in 0..=1 {
        let definition = definition_with_nodes(NODES + extra);
        // With exactly one extra value, only the final visited node is over.
        let path = last_node_path(&definition, "$");
        visit(
            &case_name("json_nodes", extra),
            &definition,
            (extra != 0).then_some(("json_nodes", path.as_str())),
        );
    }

    // The open tool payload is six edges below the definition root.
    let payload_depth = 6;
    for extra in 0..=1 {
        let wrappers = DEPTH - payload_depth + extra;
        let mut payload = Value::Bool(true);
        for _ in 0..wrappers {
            payload = Value::Array(vec![payload]);
        }
        let definition = with_payload(payload);
        let path = format!("{DATA_PATH}{}", "[0]".repeat(wrappers));
        visit(
            &case_name("nesting_depth", extra),
            &definition,
            (extra != 0).then_some(("nesting_depth", path.as_str())),
        );
    }

    count_boundary(
        &mut visit,
        "root_schema_properties",
        DECLARATIONS,
        "schema_properties",
        "$.agent_schema.properties",
        |count| {
            let mut definition = model_definition();
            definition["agent_schema"]["properties"] = schema_properties(count);
            definition
        },
    );
    count_boundary(
        &mut visit,
        "object_schema_properties",
        DECLARATIONS,
        "schema_properties",
        "$.agent_schema.properties.payload.properties",
        |count| {
            let mut definition = model_definition();
            definition["agent_schema"]["properties"] = json!({
                "payload": {"type": "object", "properties": schema_properties(count)}
            });
            definition
        },
    );
    count_boundary(
        &mut visit,
        "array_object_schema_properties",
        DECLARATIONS,
        "schema_properties",
        "$.agent_schema.properties.payload.items.properties",
        |count| {
            let mut definition = model_definition();
            definition["agent_schema"]["properties"] = json!({
                "payload": {
                    "type": "array",
                    "items": {"type": "object", "properties": schema_properties(count)}
                }
            });
            definition
        },
    );
    count_boundary(
        &mut visit,
        "inputs",
        DECLARATIONS,
        "inputs",
        "$.inputs",
        |count| {
            let mut definition = model_definition();
            definition["inputs"] = Value::Array(
                (0..count)
                    .map(|index| {
                        json!({"name": format!("input{index:04}"), "type": "text", "text": "x"})
                    })
                    .collect(),
            );
            definition
        },
    );
    count_boundary(
        &mut visit,
        "runtime_variables",
        DECLARATIONS,
        "runtime_variables",
        "$.runtime_vars",
        |count| {
            let mut definition = model_definition();
            definition["runtime_vars"] = named_map(
                "variable",
                count,
                |_| json!({"type": "boolean", "default": false}),
            );
            definition
        },
    );
    count_boundary(
        &mut visit,
        "actions",
        DECLARATIONS,
        "actions",
        "$.actions",
        |count| {
            let mut definition = model_definition();
            definition["actions"] = Value::Array(
                (0..count)
                    .map(|index| action(index, vec![exec_step()]))
                    .collect(),
            );
            definition
        },
    );
    count_boundary(
        &mut visit,
        "steps_per_action",
        DECLARATIONS,
        "steps_per_action",
        "$.actions[0].run",
        |count| {
            let mut definition = model_definition();
            definition["actions"] = json!([action(0, vec![exec_step(); count])]);
            definition
        },
    );
    count_boundary(
        &mut visit,
        "total_steps",
        COLLECTION,
        "total_steps",
        "$.actions[16].run",
        |count| {
            let mut definition = model_definition();
            let mut actions = Vec::new();
            let mut remaining = count;
            while remaining != 0 {
                let steps = remaining.min(DECLARATIONS);
                actions.push(action(actions.len(), vec![exec_step(); steps]));
                remaining -= steps;
            }
            definition["actions"] = Value::Array(actions);
            definition
        },
    );
    count_boundary(
        &mut visit,
        "tool_params",
        DECLARATIONS,
        "tool_params",
        &format!("{STEP_PATH}.params"),
        |count| {
            with_step(json!({
                "kind": "tool", "name": "boundary_tool",
                "params": named_map("param", count, |_| Value::Bool(true))
            }))
        },
    );
    count_boundary(
        &mut visit,
        "child_inputs",
        DECLARATIONS,
        "child_inputs",
        &format!("{STEP_PATH}.inputs"),
        |count| {
            with_step(json!({
                "kind": "agent", "artifact": "./child.json",
                "inputs": vec![json!({"type": "text", "text": "x"}); count]
            }))
        },
    );
    for (field, limit) in [
        ("run_vars", "child_run_variables"),
        ("input_overrides", "child_input_overrides"),
    ] {
        count_boundary(
            &mut visit,
            limit,
            DECLARATIONS,
            limit,
            &format!("{STEP_PATH}.{field}"),
            |count| {
                let mut step = json!({"kind": "agent", "artifact": "./child.json"});
                step[field] = named_map("binding", count, |_| Value::String("x".into()));
                with_step(step)
            },
        );
    }
    count_boundary(
        &mut visit,
        "reference_images",
        DECLARATIONS,
        "reference_images",
        &format!("{STEP_PATH}.reference_images"),
        |count| {
            with_step(json!({
                "kind": "generate_image", "prompt": "An image.", "path": "./image.png",
                "reference_images": vec![json!({"path": "./reference.png"}); count]
            }))
        },
    );
    count_boundary(
        &mut visit,
        "enum_entries",
        PARTS,
        "enum_entries",
        "$.agent_schema.properties.flag.enum",
        |count| {
            let mut definition = model_definition();
            definition["agent_schema"]["properties"]["flag"] = json!({
                "type": "string",
                "enum": (0..count).map(|index| format!("entry{index:04}")).collect::<Vec<_>>()
            });
            definition
        },
    );
    count_boundary(
        &mut visit,
        "exec_string_parts",
        PARTS,
        "string_parts",
        &format!("{STEP_PATH}.args"),
        |count| {
            let mut step = exec_step();
            step["args"] = json!(vec!["x"; count]);
            with_step(step)
        },
    );
    count_boundary(
        &mut visit,
        "interpolated_string_parts",
        PARTS,
        "string_parts",
        &format!("{STEP_PATH}.text"),
        |count| {
            with_step(json!({
                "kind": "email_me", "subject": "Boundary", "text": vec!["x"; count]
            }))
        },
    );
}

/// Visit raw structured-output cases at and above the evaluation-work limit.
///
/// The expected failure uses the same resource-name/path pair as definitions.
pub fn visit_output_limit_cases(mut visit: impl FnMut(&str, &Value, &Value, Option<(&str, &str)>)) {
    let schema = json!({
        "type": "object",
        "properties": {
            "values": {
                "type": "array",
                "items": {
                    "type": "string",
                    "enum": (0..1022).map(|index| format!("e{index}")).collect::<Vec<_>>()
                }
            }
        }
    });
    for extra in 0..=1 {
        let mut values = vec![Value::String("e1021".into()); 256];
        for value in values.iter_mut().take(4) {
            *value = Value::String("e1020".into());
        }
        if extra != 0 {
            values[0] = Value::String("e1021".into());
        }
        let output = json!({"values": values});
        // 258 data nodes + 258 output visits + 4*1021 + 252*1022
        // comparisons = 262,144. One extra comparison fails at the final item.
        visit(
            &case_name("output_evaluation_work", extra),
            &output,
            &schema,
            (extra != 0).then_some(("evaluation_work", "$.values[255]")),
        );
    }
}

fn count_boundary(
    visit: &mut impl FnMut(&str, &Value, Option<(&str, &str)>),
    name: &str,
    maximum: usize,
    limit: &str,
    path: &str,
    build: impl Fn(usize) -> Value,
) {
    for extra in 0..=1 {
        let definition = build(maximum + extra);
        visit(
            &case_name(name, extra),
            &definition,
            (extra != 0).then_some((limit, path)),
        );
    }
}

fn case_name(name: &str, extra: usize) -> String {
    format!(
        "{name}_{}",
        if extra == 0 { "at_limit" } else { "over_limit" }
    )
}

fn model_definition() -> Value {
    json!({
        "agent_definition_schema_version": "2026-09-09.r1",
        "agent_schema": {"type": "object", "properties": {"flag": {"type": "boolean"}}},
        "actions": []
    })
}

fn action(index: usize, steps: Vec<Value>) -> Value {
    json!({"name": format!("action{index:04}"), "logic": {"==": [1, 1]}, "run": steps})
}

fn exec_step() -> Value {
    json!({"kind": "exec", "program": "echo", "args": []})
}

fn with_step(step: Value) -> Value {
    let mut definition = model_definition();
    definition["actions"] = json!([action(0, vec![step])]);
    definition
}

fn with_payload(payload: Value) -> Value {
    with_step(json!({"kind": "tool", "name": "boundary_tool", "params": {"data": payload}}))
}

fn named_map(prefix: &str, count: usize, value: impl Fn(usize) -> Value) -> Value {
    Value::Object(
        (0..count)
            .map(|index| (format!("{prefix}{index:04}"), value(index)))
            .collect(),
    )
}

fn schema_properties(count: usize) -> Value {
    named_map("field", count, |_| json!({"type": "boolean"}))
}

fn utf8_bytes(count: usize) -> String {
    let mut value = "é".repeat(count / 2);
    if count % 2 != 0 {
        value.push('x');
    }
    value
}

fn definition_with_text_bytes(target: usize) -> Value {
    let mut definition = with_payload(json!(["", "", "", ""]));
    let overhead = text_bytes(&definition);
    let remainder = target
        .checked_sub(overhead + 3 * STRING_BYTES)
        .expect("wrapper leaves room for four bounded text chunks");
    definition["actions"][0]["run"][0]["params"]["data"] = json!([
        utf8_bytes(STRING_BYTES),
        utf8_bytes(STRING_BYTES),
        utf8_bytes(STRING_BYTES),
        utf8_bytes(remainder)
    ]);
    definition
}

fn definition_with_nodes(target: usize) -> Value {
    const ROWS: usize = 16;
    let mut definition = with_payload(json!([]));
    let mut leaves = target
        .checked_sub(node_count(&definition) + ROWS)
        .expect("wrapper leaves room for sixteen bounded array rows");
    let mut rows = Vec::with_capacity(ROWS);
    for _ in 0..ROWS {
        let count = leaves.min(COLLECTION);
        rows.push(Value::Array(vec![Value::Null; count]));
        leaves -= count;
    }
    definition["actions"][0]["run"][0]["params"]["data"] = Value::Array(rows);
    definition
}

fn text_bytes(value: &Value) -> usize {
    match value {
        Value::String(value) => value.len(),
        Value::Array(values) => values.iter().map(text_bytes).sum(),
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| key.len() + text_bytes(value))
            .sum(),
        _ => 0,
    }
}

fn node_count(value: &Value) -> usize {
    1 + match value {
        Value::Array(values) => values.iter().map(node_count).sum::<usize>(),
        Value::Object(values) => values.values().map(node_count).sum::<usize>(),
        _ => 0,
    }
}

fn field_path(parent: &str, key: &str) -> String {
    let mut chars = key.chars();
    let simple = chars
        .next()
        .is_some_and(|value| value == '_' || value.is_ascii_alphabetic())
        && chars.all(|value| value == '_' || value.is_ascii_alphanumeric());
    if simple {
        format!("{parent}.{key}")
    } else {
        format!("{parent}[{}]", json!(key))
    }
}

fn last_node_path(value: &Value, path: &str) -> String {
    match value {
        Value::Array(values) if !values.is_empty() => {
            let index = values.len() - 1;
            last_node_path(&values[index], &format!("{path}[{index}]"))
        }
        Value::Object(values) if !values.is_empty() => {
            let (key, value) = values
                .iter()
                .max_by_key(|(key, _)| *key)
                .expect("nonempty object");
            last_node_path(value, &field_path(path, key))
        }
        _ => path.to_string(),
    }
}

fn last_string_path(value: &Value, path: &str) -> Option<String> {
    match value {
        Value::String(value) if !value.is_empty() => Some(path.to_string()),
        Value::Array(values) => values
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, value)| last_string_path(value, &format!("{path}[{index}]"))),
        Value::Object(values) => {
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|left, right| right.0.cmp(left.0));
            entries
                .into_iter()
                .find_map(|(key, value)| last_string_path(value, &field_path(path, key)))
        }
        _ => None,
    }
}
