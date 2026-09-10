//! Focused schema/codegen hardening tests for `templates/build_support.rs`.

#[allow(dead_code)]
#[path = "../templates/build_support.rs"]
mod build_support;

use sha2::{Digest, Sha256};

#[path = "support/definition_validation_cases.rs"]
mod boundary_cases;

fn generate_with_strict_parity(raw: &str) -> Result<String, build_support::BuildError> {
    let legacy = build_support::generate_agent_model_from_str(raw);
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Some(root) = value.as_object_mut() {
            root.insert(
                "agent_definition_schema_version".into(),
                serde_json::json!("2026-09-09.r1"),
            );
            let strict = build_support::generate_agent_model_from_str(&value.to_string());
            assert_eq!(
                legacy.is_ok(),
                strict.is_ok(),
                "legacy/strict disposition drift: {strict:?}\n{raw}"
            );
        }
    }
    legacy
}

fn absolute_test_path(file_name: &str) -> String {
    let path = std::env::temp_dir().join(file_name);
    assert!(path.is_absolute());
    path.to_string_lossy().into_owned()
}

/// Constructs a minimal `.agentcfg` JSON document with caller-provided schema
/// properties and actions sections.
fn config_with(properties: &str, actions: &str) -> String {
    config_with_runtime_vars(properties, "", actions)
}

fn config_with_runtime_vars(properties: &str, runtime_vars: &str, actions: &str) -> String {
    config_with_optional_inputs(
        properties,
        runtime_vars,
        actions,
        Some(
            r#"[
        { "type": "text", "text": "Test prompt" }
    ]"#,
        ),
    )
}

fn config_with_optional_inputs(
    properties: &str,
    runtime_vars: &str,
    actions: &str,
    inputs: Option<&str>,
) -> String {
    config_with_optional_inputs_and_action_execution(
        properties,
        runtime_vars,
        actions,
        inputs,
        None,
    )
}

fn config_with_optional_inputs_and_action_execution(
    properties: &str,
    runtime_vars: &str,
    actions: &str,
    inputs: Option<&str>,
    action_execution: Option<&str>,
) -> String {
    let inputs_block = if let Some(inputs) = inputs {
        format!(
            r#",
    "inputs": {inputs}"#
        )
    } else {
        String::new()
    };
    let runtime_vars_block = if runtime_vars.trim().is_empty() {
        String::new()
    } else {
        format!(
            r#",
    "runtime_vars": {{
        {runtime_vars}
    }}"#
        )
    };
    let action_execution_block = if let Some(action_execution) = action_execution {
        format!(
            r#",
    "action_execution": "{action_execution}""#
        )
    } else {
        String::new()
    };

    format!(
        r#"{{
    "agent_definition_schema_version": "2026-03-03.r1"{inputs_block}{action_execution_block},
    "agent_schema": {{
        "type": "object",
        "properties": {{
            {properties}
        }}
    }}{runtime_vars_block},
    "actions": {actions}
}}"#
    )
}

#[test]
fn accepts_schema_backed_agents_without_baked_inputs() {
    let cfg = config_with_optional_inputs(r#""answer": { "type": "integer" }"#, "", "[]", None);

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains("pub fn inputs() -> Vec<Input> {\n    vec![]\n}"));
    assert!(generated.contains("pub fn has_output_schema_properties() -> bool {\n    true\n}"));
    assert!(
        generated.contains(
            "pub fn action_execution() -> ActionExecutionMode {\n    ActionExecutionMode::Sequential\n}"
        )
    );
}

#[test]
fn accepts_structural_action_only_agents_without_inputs() {
    let cfg = config_with_optional_inputs(
        "",
        r#""generate_images": { "type": "boolean", "default": true }"#,
        "[]",
        None,
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains("pub fn inputs() -> Vec<Input> {\n    vec![]\n}"));
    assert!(generated.contains("pub fn has_output_schema_properties() -> bool {\n    false\n}"));
}

#[test]
fn emits_parallel_action_execution_when_declared() {
    let cfg = config_with_optional_inputs_and_action_execution(
        r#""answer": { "type": "integer" }"#,
        "",
        "[]",
        None,
        Some("parallel"),
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains(
        "pub fn action_execution() -> ActionExecutionMode {\n    ActionExecutionMode::Parallel\n}"
    ));
}

#[test]
fn rejects_invalid_action_execution_values() {
    let cfg = config_with_optional_inputs_and_action_execution(
        r#""answer": { "type": "integer" }"#,
        "",
        "[]",
        None,
        Some("fanout"),
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.action_execution"));
    assert!(err.contains("expected `sequential` or `parallel`"));
}

#[test]
fn accepts_named_inputs_when_schema_properties_are_empty() {
    let cfg = config_with_optional_inputs(
        "",
        "",
        r#"[
          {
            "name": "child_agent",
            "logic": { "==": [true, true] },
            "run": [
              {
                "kind": "agent",
                "agent": "./summary_agent",
                "inputs": [
                  { "input": "menu_image" },
                  { "input": "menu_note" }
                ]
              }
            ]
          }
        ]"#,
        Some(
            r#"[
        { "name": "menu_image", "type": "image", "path": "./artifacts/menu.png" },
        { "name": "menu_note", "type": "text" }
    ]"#,
        ),
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains(
        "Input { name: Some(\"menu_image\".to_string()), kind: InputKind::Image, value: Some(\"./artifacts/menu.png\".to_string()) }"
    ));
    assert!(generated.contains(
        "Input { name: Some(\"menu_note\".to_string()), kind: InputKind::Text, value: None }"
    ));
    assert!(generated.contains("ActionInput::Named { input: \"menu_image\".to_string() }"));
    assert!(generated.contains("ActionInput::Named { input: \"menu_note\".to_string() }"));
}

#[test]
fn rejects_unnamed_inputs_when_schema_properties_are_empty() {
    let cfg = config_with_optional_inputs(
        "",
        "",
        "[]",
        Some(
            r#"[
        { "type": "text", "text": "Test prompt" }
    ]"#,
        ),
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.inputs[0].name"));
    assert!(err.contains("must declare `name`"));
}

#[test]
fn rejects_unknown_named_child_input_reference() {
    let cfg = config_with_optional_inputs(
        "",
        "",
        r#"[
          {
            "name": "child_agent",
            "logic": { "==": [true, true] },
            "run": [
              {
                "kind": "agent",
                "agent": "./summary_agent",
                "inputs": [
                  { "input": "missing_input" }
                ]
              }
            ]
          }
        ]"#,
        Some(
            r#"[
        { "name": "menu_image", "type": "image", "path": "./artifacts/menu.png" }
    ]"#,
        ),
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].inputs[0].input"));
    assert!(err.contains("unknown named top-level input `missing_input`"));
}

#[test]
fn accepts_top_level_array_fields_and_arrays_of_objects() {
    let cfg = config_with(
        r#""rows": {
          "type": "array",
          "items": {
            "type": "object",
            "properties": {
              "customer": { "type": "string" },
              "amount": { "type": "number" }
            }
          }
        }"#,
        "[]",
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains("pub rows: serde_json::Value,"));
    assert!(generated.contains(r#"apply_property_schema_override("#));
    assert!(generated.contains(r#""rows""#));
    assert!(generated.contains(r#"\"customer\":{\"type\":\"string\"}"#));
}

#[test]
fn accepts_nullable_scalar_object_properties_inside_structured_fields() {
    let cfg = config_with(
        r#""rows": {
          "type": "array",
          "items": {
            "type": "object",
            "properties": {
              "customer": { "type": "string" },
              "discount": { "type": ["number", "null"] }
            }
          }
        }"#,
        "[]",
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains(r#"\"discount\":{\"type\":[\"number\",\"null\"]}"#));
}

#[test]
fn rejects_nested_arrays_with_actionable_path() {
    let cfg = config_with(
        r#""matrix": {
          "type": "array",
          "items": { "type": "array", "items": { "type": "integer" } }
        }"#,
        "[]",
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.agent_schema.properties.matrix.items.type"));
    assert!(err.contains("nested arrays are not supported in this story"));
}

#[test]
fn rejects_nested_object_properties_inside_structured_fields() {
    let cfg = config_with(
        r#""payload": {
          "type": "object",
          "properties": {
            "nested": {
              "type": "object",
              "properties": {
                "value": { "type": "string" }
              }
            }
          }
        }"#,
        "[]",
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.agent_schema.properties.payload.properties.nested.type"));
    assert!(err.contains(
        "nested object fields are not supported beyond one declared object layer in this story"
    ));
}

#[test]
fn preserves_description_in_generated_schema_metadata() {
    let cfg = config_with(
        r#""answer": { "type": "integer", "description": "The numeric answer." }"#,
        "[]",
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains(r#"apply_property_schema_override("#));
    assert!(generated.contains(r#"\"description\":\"The numeric answer.\""#));
}

#[test]
fn preserves_string_enum_in_generated_schema_and_runtime_validation() {
    let cfg = config_with(r#""unit": { "type": "string", "enum": ["F", "C"] }"#, "[]");

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains(r#"validate_schema_field("#));
    assert!(generated.contains(r#"\"enum\":[\"F\",\"C\"]"#));
}

#[test]
fn rejects_non_string_enum_fields() {
    let cfg = config_with(
        r#""score": { "type": "number", "enum": ["high", "low"] }"#,
        "[]",
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.agent_schema.properties.score.enum"));
    assert!(err.contains("`enum` is supported only for `type: \"string\"` fields"));
}

#[test]
fn preserves_numeric_bounds_in_generated_schema_and_runtime_validation() {
    let cfg = config_with(
        r#""confidence": {
          "type": "number",
          "minimum": 0,
          "exclusiveMaximum": 1
        }"#,
        "[]",
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains(r#"\"minimum\":0"#));
    assert!(generated.contains(r#"\"exclusiveMaximum\":1"#));
    assert!(generated.contains("fn validate_number_constraints("));
}

#[test]
fn preserves_integer_bounds_in_generated_schema_and_runtime_validation() {
    let cfg = config_with(
        r#""attempts": {
          "type": "integer",
          "minimum": 1,
          "maximum": 3
        }"#,
        "[]",
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains(r#"\"minimum\":1"#));
    assert!(generated.contains(r#"\"maximum\":3"#));
    assert!(generated.contains("fn validate_integer_constraints("));
}

#[test]
fn rejects_conflicting_numeric_lower_bounds() {
    let cfg = config_with(
        r#""confidence": {
          "type": "number",
          "minimum": 0,
          "exclusiveMinimum": 0
        }"#,
        "[]",
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.agent_schema.properties.confidence.exclusiveMinimum"));
    assert!(err.contains("cannot be combined with `minimum`"));
}

#[test]
fn rejects_top_level_nullable_fields() {
    let cfg = config_with(r#""value": { "type": ["string", "null"] }"#, "[]");

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.agent_schema.properties.value.type"));
    assert!(err.contains("union schema types are not supported yet"));
}

#[test]
fn rejects_union_types_with_actionable_path() {
    let cfg = config_with(r#""value": { "type": ["string", "integer"] }"#, "[]");

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.agent_schema.properties.value.type"));
    assert!(err.contains("union schema types are not supported yet"));
}

#[test]
fn rejects_invalid_field_identifiers() {
    let cfg = config_with(r#""bad-name": { "type": "string" }"#, "[]");

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.agent_schema.properties.bad-name"));
    assert!(err.contains("must contain only ASCII letters, digits, or underscores"));
}

#[test]
fn rejects_reserved_runtime_top_level_output_field_name() {
    let cfg = config_with(r#""runtime": { "type": "string" }"#, "[]");

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.agent_schema.properties.runtime"));
    assert!(err.contains("`runtime` is reserved"));
}

#[test]
fn emits_runtime_var_specs_for_declared_runtime_vars() {
    let cfg = config_with_runtime_vars(
        r#""answer": { "type": "integer" }"#,
        r#"
        "subject": { "type": "string", "default": "Test" },
        "generate_images": { "type": "boolean", "default": false },
        "retry_count": { "type": "integer" },
        "score_threshold": { "type": "number", "default": 0.75 }
        "#,
        "[]",
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains("pub enum RuntimeVarType"));
    assert!(generated.contains("pub struct RuntimeVarSpec"));
    assert!(generated.contains("pub fn runtime_var_specs() -> Vec<RuntimeVarSpec>"));
    assert!(generated.contains("name: \"subject\".to_string()"));
    assert!(generated.contains("field_type: RuntimeVarType::Boolean"));
    assert!(generated.contains("generated runtime-var default must be valid JSON"));
}

#[test]
fn rejects_runtime_var_default_type_mismatch() {
    let cfg = config_with_runtime_vars(
        r#""answer": { "type": "integer" }"#,
        r#""generate_images": { "type": "boolean", "default": "true" }"#,
        "[]",
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.runtime_vars.generate_images.default"));
    assert!(err.contains("default must be a boolean"));
}

#[test]
fn accepts_runtime_vars_in_logic_when_and_substitution_surfaces() {
    let cfg = config_with_runtime_vars(
        r#""answer": { "type": "integer" }"#,
        r#"
        "generate_images": { "type": "boolean", "default": false },
        "report_suffix": { "type": "string", "default": "nightly" }
        "#,
        r#"[
          {
            "name": "demo",
            "logic": { "==": [ { "var": "runtime.generate_images" }, true ] },
            "run": [
              {
                "kind": "email_me",
                "when": { "==": [ { "var": "runtime.generate_images" }, true ] },
                "subject": ["Report ", { "var": "runtime.report_suffix" }],
                "text": ["Answer=", { "var": "answer" }]
              }
            ]
          }
        ]"#,
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains(r#"RunArg::Variable("runtime.report_suffix".to_string())"#));
    assert!(generated.contains(r#"generated step `when` must be valid JSON"#));
}

#[test]
fn rejects_reserved_keyword_field_identifiers() {
    let cfg = config_with(r#""union": { "type": "string" }"#, "[]");

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.agent_schema.properties.union"));
    assert!(err.contains("reserved Rust keyword"));
}

#[test]
fn rejects_unsupported_action_kind_with_actionable_path() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "unsupported",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              { "kind": "http", "program": "echo", "args": [] }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].kind"));
    assert!(err.contains("supported: `exec`, `email_me`, `agent`, `tool`, `generate_image`"));
}

#[test]
fn accepts_mixed_literal_and_variable_action_args() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "mixed_args",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": ["value=", { "var": "value" }]
              }
            ]
          }
        ]"#,
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains("RunArg::Literal(\"value=\".to_string())"));
    assert!(generated.contains("RunArg::Variable(\"value\".to_string())"));
}

#[test]
fn accepts_email_me_string_and_variable_parts() {
    let cfg = config_with(
        r#""city": { "type": "string" }, "raining": { "type": "boolean" }"#,
        r#"[
          {
            "name": "email_me",
            "logic": { "==": [ { "var": "raining" }, true ] },
            "run": [
              {
                "kind": "email_me",
                "subject": ["Weather alert for ", { "var": "city" }],
                "text": ["Bring an umbrella because raining=", { "var": "raining" }]
              }
            ]
          }
        ]"#,
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains("kind: \"email_me\".to_string()"));
    assert!(generated.contains("subject: Some(vec![RunArg::Literal(\"Weather alert for \".to_string()), RunArg::Variable(\"city\".to_string())])"));
    assert!(generated.contains("text: Some(vec![RunArg::Literal(\"Bring an umbrella because raining=\".to_string()), RunArg::Variable(\"raining\".to_string())])"));
}

#[test]
fn accepts_generate_image_step_with_prompt_and_path_parts() {
    let cfg = config_with(
        r#""product_name": { "type": "string" }, "product_slug": { "type": "string" }"#,
        r#"[
          {
            "name": "generate_image",
            "logic": { "==": [ { "var": "product_name" }, "Widget" ] },
            "run": [
              {
                "kind": "generate_image",
                "model": "gpt-image-1",
                "prompt": ["Create a render for ", { "var": "product_name" }],
                "path": ["./artifacts/", { "var": "product_slug" }, ".png"]
              }
            ]
          }
        ]"#,
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains("kind: \"generate_image\".to_string()"));
    assert!(generated.contains("model: Some(RunArg::Literal(\"gpt-image-1\".to_string()))"));
    assert!(generated.contains(
        "prompt: Some(vec![RunArg::Literal(\"Create a render for \".to_string()), RunArg::Variable(\"product_name\".to_string())])"
    ));
    assert!(generated.contains(
        "path: Some(vec![RunArg::Literal(\"./artifacts/\".to_string()), RunArg::Variable(\"product_slug\".to_string()), RunArg::Literal(\".png\".to_string())])"
    ));
}

#[test]
fn rejects_generate_image_output_variable() {
    let cfg = config_with(
        r#""product_name": { "type": "string" }"#,
        r#"[
          {
            "name": "generate_image",
            "logic": { "==": [ { "var": "product_name" }, "Widget" ] },
            "run": [
              {
                "kind": "generate_image",
                "model": "gpt-image-1",
                "prompt": "Create a render",
                "path": "./artifacts/widget.png",
                "output_variable": "image_path"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].output_variable"));
    assert!(err.contains("`output_variable` is only supported for `exec` and `tool` actions"));
}

#[test]
fn rejects_generate_image_unsupported_output_extension() {
    let cfg = config_with(
        r#""product_name": { "type": "string" }"#,
        r#"[
          {
            "name": "generate_image",
            "logic": { "==": [ { "var": "product_name" }, "Widget" ] },
            "run": [
              {
                "kind": "generate_image",
                "model": "gpt-image-1",
                "prompt": "Create a render",
                "path": "./artifacts/widget.gif"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].path"));
    assert!(err.contains("supported extension"));
}

#[test]
fn accepts_generate_image_model_from_runtime_or_schema_string_variables() {
    let cfg = config_with_runtime_vars(
        r#""image_model": { "type": "string" }, "product_name": { "type": "string" }"#,
        r#""runtime_image_model": { "type": "string" }"#,
        r#"[
          {
            "name": "generate_image",
            "logic": { "==": [ { "var": "product_name" }, "Widget" ] },
            "run": [
              {
                "kind": "generate_image",
                "model": { "var": "runtime.runtime_image_model" },
                "prompt": "Create a render",
                "path": "./artifacts/widget.png"
              },
              {
                "kind": "generate_image",
                "model": { "var": "image_model" },
                "prompt": "Create another render",
                "path": "./artifacts/widget2.png"
              }
            ]
          }
        ]"#,
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated
        .contains("model: Some(RunArg::Variable(\"runtime.runtime_image_model\".to_string()))"));
    assert!(generated.contains("model: Some(RunArg::Variable(\"image_model\".to_string()))"));
}

#[test]
fn rejects_generate_image_model_from_non_string_variable() {
    let cfg = config_with_runtime_vars(
        r#""product_name": { "type": "string" }"#,
        r#""runtime_image_model": { "type": "boolean" }"#,
        r#"[
          {
            "name": "generate_image",
            "logic": { "==": [ { "var": "product_name" }, "Widget" ] },
            "run": [
              {
                "kind": "generate_image",
                "model": { "var": "runtime.runtime_image_model" },
                "prompt": "Create a render",
                "path": "./artifacts/widget.png"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].model"));
    assert!(err.contains("must resolve from string fields"));
}

#[test]
fn rejects_generate_image_model_from_captured_variable() {
    let cfg = config_with(
        r#""product_name": { "type": "string" }"#,
        r#"[
          {
            "name": "generate_image",
            "logic": { "==": [ { "var": "product_name" }, "Widget" ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": ["gpt-image-1"],
                "output_variable": "image_model"
              },
              {
                "kind": "generate_image",
                "model": { "var": "image_model" },
                "prompt": "Create a render",
                "path": "./artifacts/widget.png"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[1].model"));
    assert!(err.contains("unknown variable `image_model`"));
}

#[test]
fn rejects_email_me_program_field() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "bad_email_me",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "email_me",
                "program": "echo",
                "subject": "Alert",
                "text": "Body"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].program"));
    assert!(err.contains("not supported for `email_me`"));
}

#[test]
fn rejects_empty_email_me_subject_string() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "bad_email_subject",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "email_me",
                "subject": "   ",
                "text": "Body"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].subject"));
    assert!(err.contains("must be a non-empty string"));
}

#[test]
fn accepts_agent_step_with_relative_path_and_inputs() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "child_agent",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "agent",
                "agent": "./summary_agent",
                "input_mode": "append",
                "inputs": [
                  { "type": "text", "text": "Summarize this." },
                  { "type": "url", "url": "https://example.com" }
                ]
              }
            ]
          }
        ]"#,
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains("kind: \"agent\".to_string()"));
    assert!(generated.contains("agent: Some(\"./summary_agent\".to_string())"));
    assert!(generated.contains("input_mode: Some(ActionInputMode::Append)"));
    assert!(generated.contains(
        "inputs: Some(vec![ActionInput::Text { text: vec![RunArg::Literal(\"Summarize this.\".to_string())] }, ActionInput::Url { url: vec![RunArg::Literal(\"https://example.com\".to_string())] }])"
    ));
}

#[test]
fn accepts_agent_step_with_input_overrides() {
    let cfg = config_with_optional_inputs(
        r#""value": { "type": "integer" }"#,
        "",
        r#"[
          {
            "name": "child_agent",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "agent",
                "agent": "./summary_agent",
                "run_vars": {
                  "year": 2026,
                  "month": { "var": "value" }
                },
                "input_overrides": {
                  "menu_image": { "input": "menu_image" },
                  "menu_note": "Spring menu"
                }
              }
            ]
          }
        ]"#,
        Some(
            r#"[
        { "name": "menu_image", "type": "image", "path": "./artifacts/menu.png" }
    ]"#,
        ),
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains("run_vars: Some(vec!["));
    assert!(generated
        .contains("ActionRunVar { name: \"year\".to_string(), value: ActionRunVarValue::Literal("));
    assert!(generated.contains(
        "ActionRunVar { name: \"month\".to_string(), value: ActionRunVarValue::Variable(\"value\".to_string()) }"
    ));
    assert!(generated.contains("input_overrides: Some(vec!["));
    assert!(generated.contains(
        "ActionInputOverride { name: \"menu_image\".to_string(), value: ActionInputOverrideValue::NamedInput { input: \"menu_image\".to_string() } }"
    ));
    assert!(generated.contains(
        "ActionInputOverride { name: \"menu_note\".to_string(), value: ActionInputOverrideValue::Literal(\"Spring menu\".to_string()) }"
    ));
}

#[test]
fn rejects_non_object_input_overrides() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "child_agent",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "agent",
                "agent": "./summary_agent",
                "input_overrides": []
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].input_overrides"));
    assert!(err.contains("expected `input_overrides` to be an object"));
}

#[test]
fn rejects_input_overrides_on_exec_steps() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "exec_with_overrides",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": ["hello"],
                "input_overrides": {
                  "menu_note": "Spring menu"
                }
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].input_overrides"));
    assert!(err.contains("only supported for `agent` actions"));
}

#[test]
fn rejects_agent_input_mode_without_inputs() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "child_agent",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "agent",
                "agent": "./summary_agent",
                "input_mode": "append"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].input_mode"));
    assert!(err.contains("requires `inputs`"));
}

#[test]
fn rejects_input_mode_on_exec_steps() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "exec_with_mode",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": ["hello"],
                "input_mode": "append"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].input_mode"));
    assert!(err.contains("only supported for `agent` actions"));
}

#[test]
fn rejects_input_mode_on_email_steps() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "email_with_mode",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "email_me",
                "subject": "Hello",
                "text": "World",
                "input_mode": "prepend"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].input_mode"));
    assert!(err.contains("only supported for `agent` actions"));
}

#[test]
fn accepts_dynamic_child_agent_input_parts() {
    let cfg = config_with(
        r#""report_filename": { "type": "string" }, "customer": { "type": "string" }"#,
        r#"[
          {
            "name": "child_agent",
            "logic": { "==": [ { "var": "customer" }, "acme" ] },
            "run": [
              {
                "kind": "agent",
                "agent": "./summary_agent",
                "inputs": [
                  {
                    "type": "text",
                    "text": ["Summarize for ", { "var": "customer" }]
                  },
                  {
                    "type": "file",
                    "path": ["./reports/", { "var": "report_filename" }]
                  }
                ]
              }
            ]
          }
        ]"#,
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains(
        "ActionInput::Text { text: vec![RunArg::Literal(\"Summarize for \".to_string()), RunArg::Variable(\"customer\".to_string())] }"
    ));
    assert!(generated.contains(
        "ActionInput::File { path: vec![RunArg::Literal(\"./reports/\".to_string()), RunArg::Variable(\"report_filename\".to_string())] }"
    ));
}

#[test]
fn accepts_exec_output_variable_for_later_action_inputs() {
    let cfg = config_with(
        r#""customer": { "type": "string" }"#,
        r#"[
          {
            "name": "child_agent",
            "logic": { "==": [ { "var": "customer" }, "Acme" ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": ["./reports/q1.pdf"],
                "output_variable": "report_path"
              },
              {
                "kind": "agent",
                "agent": "./summary_agent",
                "inputs": [
                  {
                    "type": "text",
                    "text": ["Customer=", { "var": "customer" }, "\nPath=", { "var": "report_path" }]
                  }
                ]
              }
            ]
          }
        ]"#,
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains("output_variable: Some(\"report_path\".to_string())"));
    assert!(generated.contains(
        "ActionInput::Text { text: vec![RunArg::Literal(\"Customer=\".to_string()), RunArg::Variable(\"customer\".to_string()), RunArg::Literal(\"\\nPath=\".to_string()), RunArg::Variable(\"report_path\".to_string())] }"
    ));
}

#[test]
fn accepts_step_control_fields_for_later_steps() {
    let cfg = config_with(
        r#""answer": { "type": "integer" }"#,
        r#"[
          {
            "name": "child_agent",
            "logic": { "==": [ { "var": "answer" }, 4 ] },
            "run": [
              {
                "kind": "agent",
                "agent": "./summary_agent",
                "failure_mode": "continue",
                "status_variable": "child_status",
                "error_variable": "child_error"
              },
              {
                "kind": "email_me",
                "when": { "==": [ { "var": "child_status" }, "failed" ] },
                "subject": "Child failed",
                "text": ["Failure: ", { "var": "child_error" }]
              }
            ]
          }
        ]"#,
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains("status_variable: Some(\"child_status\".to_string())"));
    assert!(generated.contains("error_variable: Some(\"child_error\".to_string())"));
    assert!(generated.contains("failure_mode: Some(FailureMode::Continue)"));
    assert!(generated.contains(
        "when: Some(serde_json::from_str(\"{\\\"==\\\":[{\\\"var\\\":\\\"child_status\\\"},\\\"failed\\\"]}\")"
    ));
}

#[test]
fn accepts_abort_failure_mode_for_run_steps() {
    let cfg = config_with(
        r#""answer": { "type": "integer" }"#,
        r#"[
          {
            "name": "abort_action",
            "logic": { "==": [ { "var": "answer" }, 4 ] },
            "run": [
              {
                "kind": "exec",
                "program": "/bin/false",
                "args": [],
                "failure_mode": "abort"
              }
            ]
          }
        ]"#,
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains("failure_mode: Some(FailureMode::Abort)"));
}

#[test]
fn rejects_duplicate_output_variable_names_within_one_action() {
    let cfg = config_with(
        r#""customer": { "type": "string" }"#,
        r#"[
          {
            "name": "child_agent",
            "logic": { "==": [ { "var": "customer" }, "Acme" ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": ["first"],
                "output_variable": "report_listing"
              },
              {
                "kind": "exec",
                "program": "echo",
                "args": ["second"],
                "output_variable": "report_listing"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[1].output_variable"));
    assert!(err.contains("duplicate captured variable name `report_listing`"));
}

#[test]
fn rejects_output_variable_name_collisions_with_agent_output_fields() {
    let cfg = config_with(
        r#""customer": { "type": "string" }"#,
        r#"[
          {
            "name": "child_agent",
            "logic": { "==": [ { "var": "customer" }, "Acme" ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": ["value"],
                "output_variable": "customer"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].output_variable"));
    assert!(err.contains("collides with an agent output field"));
}

#[test]
fn rejects_duplicate_status_variable_names_within_one_action() {
    let cfg = config_with(
        r#""customer": { "type": "string" }"#,
        r#"[
          {
            "name": "child_agent",
            "logic": { "==": [ { "var": "customer" }, "Acme" ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": ["first"],
                "status_variable": "step_status"
              },
              {
                "kind": "agent",
                "agent": "./summary_agent",
                "status_variable": "step_status"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[1].status_variable"));
    assert!(err.contains("duplicate captured variable name `step_status`"));
}

#[test]
fn rejects_status_variable_name_collisions_with_agent_output_fields() {
    let cfg = config_with(
        r#""customer": { "type": "string" }"#,
        r#"[
          {
            "name": "child_agent",
            "logic": { "==": [ { "var": "customer" }, "Acme" ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": ["value"],
                "status_variable": "customer"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].status_variable"));
    assert!(err.contains("collides with an agent output field"));
}

#[test]
fn rejects_reserved_runtime_capture_variable_name() {
    let cfg = config_with(
        r#""answer": { "type": "integer" }"#,
        r#"[
          {
            "name": "child_agent",
            "logic": { "==": [ { "var": "answer" }, 4 ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": ["ok"],
                "status_variable": "runtime"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].status_variable"));
    assert!(err.contains("`runtime` is reserved"));
}

#[test]
fn allows_reusing_output_variable_names_in_different_actions() {
    let cfg = config_with(
        r#""customer": { "type": "string" }"#,
        r#"[
          {
            "name": "first_action",
            "logic": { "==": [ { "var": "customer" }, "Acme" ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": ["first"],
                "output_variable": "report_listing"
              }
            ]
          },
          {
            "name": "second_action",
            "logic": { "==": [ { "var": "customer" }, "Acme" ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": ["second"],
                "output_variable": "report_listing"
              }
            ]
          }
        ]"#,
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert_eq!(
        generated
            .matches("output_variable: Some(\"report_listing\".to_string())")
            .count(),
        2
    );
}

#[test]
fn allows_reusing_status_variable_names_in_different_actions() {
    let cfg = config_with(
        r#""customer": { "type": "string" }"#,
        r#"[
          {
            "name": "first_action",
            "logic": { "==": [ { "var": "customer" }, "Acme" ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": ["first"],
                "status_variable": "step_status"
              }
            ]
          },
          {
            "name": "second_action",
            "logic": { "==": [ { "var": "customer" }, "Acme" ] },
            "run": [
              {
                "kind": "agent",
                "agent": "./summary_agent",
                "status_variable": "step_status"
              }
            ]
          }
        ]"#,
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert_eq!(
        generated
            .matches("status_variable: Some(\"step_status\".to_string())")
            .count(),
        2
    );
}

#[test]
fn rejects_cross_action_reference_to_captured_output_variable() {
    let cfg = config_with(
        r#""customer": { "type": "string" }"#,
        r#"[
          {
            "name": "first_action",
            "logic": { "==": [ { "var": "customer" }, "Acme" ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": ["first"],
                "output_variable": "report_listing"
              }
            ]
          },
          {
            "name": "second_action",
            "logic": { "==": [ { "var": "customer" }, "Acme" ] },
            "run": [
              {
                "kind": "agent",
                "agent": "./summary_agent",
                "inputs": [
                  {
                    "type": "text",
                    "text": ["Listing=", { "var": "report_listing" }]
                  }
                ]
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[1].run[0].inputs[0].text[1].var"));
    assert!(err.contains("unknown variable `report_listing`"));
}

#[test]
fn rejects_cross_action_reference_to_status_variable() {
    let cfg = config_with(
        r#""customer": { "type": "string" }"#,
        r#"[
          {
            "name": "first_action",
            "logic": { "==": [ { "var": "customer" }, "Acme" ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": ["first"],
                "status_variable": "step_status"
              }
            ]
          },
          {
            "name": "second_action",
            "logic": { "==": [ { "var": "customer" }, "Acme" ] },
            "run": [
              {
                "kind": "email_me",
                "when": { "==": [ { "var": "step_status" }, "failed" ] },
                "subject": "Failed",
                "text": "Body"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[1].run[0].when.==[0].var"));
    assert!(err.contains("unknown variable `step_status`"));
}

#[test]
fn accepts_pdf_file_inputs() {
    let cfg = r#"{
    "agent_definition_schema_version": "2026-03-03.r1",
    "inputs": [
        { "type": "file", "path": "./reports/q1.pdf" }
    ],
    "agent_schema": {
        "type": "object",
        "properties": {
            "summary": { "type": "string" }
        }
    },
    "actions": []
}"#;

    let generated = generate_with_strict_parity(cfg).unwrap();

    assert!(generated.contains(
        "Input { name: None, kind: InputKind::File, value: Some(\"./reports/q1.pdf\".to_string()) }"
    ));
}

#[test]
fn accepts_docx_file_inputs() {
    let cfg = r#"{
    "agent_definition_schema_version": "2026-03-03.r1",
    "inputs": [
        { "type": "file", "path": "./reports/q1.docx" }
    ],
    "agent_schema": {
        "type": "object",
        "properties": {
            "summary": { "type": "string" }
        }
    },
    "actions": []
}"#;

    let generated = generate_with_strict_parity(cfg).unwrap();

    assert!(generated.contains(
        "Input { name: None, kind: InputKind::File, value: Some(\"./reports/q1.docx\".to_string()) }"
    ));
}

#[test]
fn accepts_csv_file_inputs() {
    let cfg = r#"{
    "agent_definition_schema_version": "2026-03-03.r1",
    "inputs": [
        { "type": "file", "path": "./reports/q1.csv" }
    ],
    "agent_schema": {
        "type": "object",
        "properties": {
            "summary": { "type": "string" }
        }
    },
    "actions": []
}"#;

    let generated = generate_with_strict_parity(cfg).unwrap();

    assert!(generated.contains(
        "Input { name: None, kind: InputKind::File, value: Some(\"./reports/q1.csv\".to_string()) }"
    ));
}

#[test]
fn accepts_phase_three_file_inputs() {
    let extensions = [
        "xla", "xlb", "xlc", "xlm", "xls", "xlsx", "xlt", "xlw", "tsv", "iif", "doc", "dot", "odt",
        "rtf", "pot", "ppa", "pps", "ppt", "pptx", "pwz", "wiz",
    ];

    for extension in extensions {
        let cfg = format!(
            r#"{{
    "agent_definition_schema_version": "2026-03-03.r1",
    "inputs": [
        {{ "type": "file", "path": "./reports/q1.{extension}" }}
    ],
    "agent_schema": {{
        "type": "object",
        "properties": {{
            "summary": {{ "type": "string" }}
        }}
    }},
    "actions": []
}}"#
        );

        let generated = generate_with_strict_parity(&cfg)
            .unwrap_or_else(|err| panic!("expected {extension} to be accepted: {err}"));

        assert!(
            generated.contains(&format!(
                "Input {{ name: None, kind: InputKind::File, value: Some(\"./reports/q1.{extension}\".to_string()) }}"
            )),
            "generated code should preserve the {extension} path"
        );
    }
}

#[test]
fn rejects_unsupported_file_inputs() {
    let cfg = r#"{
    "agent_definition_schema_version": "2026-03-03.r1",
    "inputs": [
        { "type": "file", "path": "./reports/q1.txt" }
    ],
    "agent_schema": {
        "type": "object",
        "properties": {
            "summary": { "type": "string" }
        }
    },
    "actions": []
}"#;

    let err = generate_with_strict_parity(cfg).unwrap_err().to_string();

    assert!(err.contains("$.inputs[0].path"));
    assert!(err.contains("supported extension"));
    assert!(err.contains("`.docx`"));
    assert!(err.contains("`.csv`"));
    assert!(err.contains("`.pptx`"));
}

#[test]
fn rejects_agent_absolute_path() {
    let actions = serde_json::json!([
        {
            "name": "bad_agent",
            "logic": { "==": [{ "var": "value" }, 1] },
            "run": [
                {
                    "kind": "agent",
                    "agent": absolute_test_path("summary_agent")
                }
            ]
        }
    ])
    .to_string();
    let cfg = config_with(r#""value": { "type": "integer" }"#, &actions);

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].agent"));
    assert!(err.contains("absolute paths are not allowed"));
}

#[test]
fn rejects_agent_bare_executable_name() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "bad_agent",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "agent",
                "agent": "child_agent"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].agent"));
    assert!(err.contains("bare child-agent names are not allowed"));
}

#[test]
fn rejects_agent_parent_traversal_path() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "bad_agent",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "agent",
                "agent": "./../child_agent"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].agent"));
    assert!(err.contains("parent traversal"));
}

#[test]
fn rejects_image_input_parent_traversal_path() {
    let cfg = r#"{
    "agent_definition_schema_version": "2026-03-03.r1",
    "inputs": [
        { "type": "image", "path": "./../4.png" }
    ],
    "agent_schema": {
        "type": "object",
        "properties": {
            "value": { "type": "integer" }
        }
    },
    "actions": []
}"#;

    let err = generate_with_strict_parity(cfg).unwrap_err().to_string();

    assert!(err.contains("$.inputs[0].path"));
    assert!(err.contains("parent traversal"));
}

#[test]
fn rejects_image_input_absolute_path() {
    let cfg = serde_json::json!({
        "agent_definition_schema_version": "2026-03-03.r1",
        "inputs": [
            { "type": "image", "path": absolute_test_path("4.png") }
        ],
        "agent_schema": {
            "type": "object",
            "properties": {
                "value": { "type": "integer" }
            }
        },
        "actions": []
    })
    .to_string();

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.inputs[0].path"));
    assert!(err.contains("current level or below"));
}

#[test]
fn rejects_agent_program_field() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "bad_agent",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "agent",
                "agent": "./agents/summary_agent",
                "program": "echo"
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].program"));
    assert!(err.contains("not supported for `agent`"));
}

#[test]
fn normalizes_platform_string_and_array_values() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "platforms",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "exec",
                "platform": "MacOS",
                "program": "echo",
                "args": ["one"]
              },
              {
                "kind": "exec",
                "platform": ["LINUX", "windows"],
                "program": "echo",
                "args": ["two"]
              }
            ]
          }
        ]"#,
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains("platforms: Some(vec![\"macos\".to_string()])"));
    assert!(
        generated.contains("platforms: Some(vec![\"linux\".to_string(), \"windows\".to_string()])")
    );
}

#[test]
fn rejects_unknown_platform_with_actionable_path() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "bad_platform",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "exec",
                "platform": "freebsd",
                "program": "echo",
                "args": ["ok"]
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].platform"));
    assert!(err.contains("supported: `macos`, `linux`, `windows`"));
}

#[test]
fn rejects_empty_platform_array_with_actionable_path() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "empty_platforms",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "exec",
                "platform": [],
                "program": "echo",
                "args": ["ok"]
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].platform"));
    assert!(err.contains("expected at least one platform entry"));
}

#[test]
fn rejects_duplicate_platforms_after_normalization() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "duplicate_platforms",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "exec",
                "platform": ["macos", "MacOS"],
                "program": "echo",
                "args": ["ok"]
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].platform[1]"));
    assert!(err.contains("duplicate platform `macos`"));
}

#[test]
fn rejects_non_string_platform_entries_with_actionable_path() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "bad_platform_entry",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "exec",
                "platform": ["macos", 1],
                "program": "echo",
                "args": ["ok"]
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].platform[1]"));
    assert!(err.contains("expected a string platform value"));
}

#[test]
fn rejects_invalid_action_arg_object_shape_with_actionable_path() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "bad_arg_object",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": [{ "field": "value" }]
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].args[0]"));
    assert!(err.contains("supported: `var`"));
}

#[test]
fn rejects_non_string_var_name_with_actionable_path() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "bad_var_name",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": [{ "var": 1 }]
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].args[0].var"));
    assert!(err.contains("expected `var` to be a string field name"));
}

#[test]
fn rejects_unknown_action_arg_variable_with_actionable_path() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "unknown_arg_var",
            "logic": { "==": [ { "var": "value" }, 1 ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": [{ "var": "missing_field" }]
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].args[0].var"));
    assert!(err.contains("unknown variable `missing_field`"));
}

#[test]
fn rejects_structured_action_arg_variables_after_array_fields_are_enabled() {
    let cfg = config_with(
        r#""numbers": { "type": "array", "items": { "type": "integer" } }"#,
        r#"[
          {
            "name": "array_arg_var",
            "logic": { "literal": true },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": [{ "var": "numbers" }]
              }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].run[0].args[0].var"));
    assert!(err.contains(
        "structured field `numbers` cannot be used as an action arg variable in this story"
    ));
}

#[test]
fn rejects_non_object_action_logic_with_actionable_path() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "bad_logic_root",
            "logic": true,
            "run": [
              { "kind": "exec", "program": "echo", "args": [] }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].logic"));
    assert!(err.contains("expected a JSON Logic object expression"));
}

#[test]
fn rejects_multi_operator_action_logic_object_with_actionable_path() {
    let cfg = config_with(
        r#""value": { "type": "integer" }"#,
        r#"[
          {
            "name": "bad_logic_shape",
            "logic": { "and": [true], "or": [false] },
            "run": [
              { "kind": "exec", "program": "echo", "args": [] }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].logic"));
    assert!(err.contains("exactly one operator key"));
}

#[test]
fn rejects_unknown_logic_var_with_actionable_path() {
    let cfg = config_with(
        r#""answer": { "type": "integer" }"#,
        r#"[
          {
            "name": "unknown_var",
            "logic": { "==": [ { "var": "missing_field" }, 4 ] },
            "run": [
              { "kind": "exec", "program": "echo", "args": [] }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].logic.==[0].var"));
    assert!(err.contains("unknown variable `missing_field`"));
}

#[test]
fn rejects_logic_type_mismatch_with_actionable_path() {
    let cfg = config_with(
        r#""answer": { "type": "integer" }"#,
        r#"[
          {
            "name": "bad_type_match",
            "logic": { "==": [ { "var": "answer" }, "4" ] },
            "run": [
              { "kind": "exec", "program": "echo", "args": [] }
            ]
          }
        ]"#,
    );

    let err = generate_with_strict_parity(&cfg).unwrap_err().to_string();

    assert!(err.contains("$.actions[0].logic.=="));
    assert!(err.contains("incompatible operand types"));
}

#[test]
fn escapes_action_literals_and_logic_payload_safely() {
    let cfg = config_with(
        r#""value": { "type": "string" }"#,
        r#"[
          {
            "name": "special \"name\"\nline",
            "logic": { "==": [ { "var": "value" }, "v#\"x\nz" ] },
            "run": [
              {
                "kind": "exec",
                "program": "echo",
                "args": ["quote \" arg", "line\nbreak", "hash#\"marker"]
              }
            ]
          }
        ]"#,
    );

    let generated = generate_with_strict_parity(&cfg).unwrap();

    assert!(generated.contains("logic: serde_json::from_str(\""));
    assert!(!generated.contains("serde_json::from_str(r#\""));
    assert!(generated.contains("name: "));
    assert!(generated.contains("\\\"name\\\""));
    assert!(generated.contains("\\nline"));
    assert!(generated.contains("\"quote \\\" arg\".to_string()"));
    assert!(generated.contains("\"line\\nbreak\".to_string()"));
    assert!(generated.contains("\"hash#\\\"marker\".to_string()"));
}

#[test]
fn generates_canonical_build_provenance_constants() {
    let cfg = r#"{
      "inputs": [
        { "type": "text", "text": "Return a numeric answer." }
      ],
      "actions": [],
      "agent_definition_schema_version": "2026-03-03.r1",
      "agent_schema": {
        "properties": {
          "answer": {
            "type": "integer"
          }
        },
        "type": "object"
      }
    }"#;

    let generated = build_support::generate_agent_build_provenance_source_with_values(
        cfg,
        "aarch64-apple-darwin",
        "11111111-2222-4333-8444-555555555555",
        "2026-03-05T23:14:29Z",
    )
    .expect("build provenance source should be generated");

    let expected_definition = r#"{"actions":[],"agent_definition_schema_version":"2026-03-03.r1","agent_schema":{"properties":{"answer":{"type":"integer"}},"type":"object"},"inputs":[{"text":"Return a numeric answer.","type":"text"}]}"#;
    let mut hasher = Sha256::new();
    hasher.update(expected_definition.as_bytes());
    let expected_hash = format!("{:x}", hasher.finalize());

    assert!(generated
        .contains(r#"const AGENT_BUILD_ID: &str = "11111111-2222-4333-8444-555555555555";"#));
    assert!(generated.contains(r#"const AGENT_TARGET_TRIPLE: &str = "aarch64-apple-darwin";"#));
    assert!(
        generated.contains(r#"const AGENT_BUILD_TIMESTAMP_UTC: &str = "2026-03-05T23:14:29Z";"#)
    );
    assert!(generated.contains(&format!(
        r#"const AGENT_DEFINITION_SHA256: &str = "{}";"#,
        expected_hash
    )));
    assert!(generated.contains(&format!(
        r#"const AGENT_EMBEDDED_DEFINITION_JSON: &str = "{}";"#,
        expected_definition.replace('"', "\\\"")
    )));
}

#[test]
fn canonical_authoring_corpus_matches_codegen_contract() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/definition_validation");
    let mut paths = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    assert!(
        paths.len() >= 50,
        "the canonical corpus must not silently disappear"
    );
    for path in paths {
        let case: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let actual = build_support::generate_agent_model_from_str(&case["definition"].to_string());
        let expected = &case["expected"];
        if expected["accepted"] == true {
            assert!(actual.is_ok(), "{}: {actual:?}", path.display());
        } else {
            match actual.expect_err(&path.display().to_string()) {
                build_support::BuildError::Definition(error) => {
                    assert_eq!(
                        serde_json::json!(error.code),
                        expected["code"],
                        "{}",
                        path.display()
                    );
                    assert_eq!(
                        serde_json::json!(error.path),
                        expected["path"],
                        "{}",
                        path.display()
                    );
                    assert!(!error.corrective_action.is_empty());
                }
                other => panic!("{}: missing shared contract error: {other}", path.display()),
            }
        }
    }
}

#[test]
fn installed_authoring_examples_use_valid_strict_contracts() {
    for directory in ["templates/guidance/examples", "templates/shared/examples"] {
        for entry in
            std::fs::read_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(directory))
                .unwrap()
        {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let raw = std::fs::read_to_string(&path).unwrap();
            let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(
                value["agent_definition_schema_version"],
                "2026-09-09.r1",
                "{}",
                path.display()
            );
            build_support::generate_agent_model_from_str(&raw)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        }
    }
}

#[test]
fn canonical_resource_boundaries_match_codegen_contract() {
    let mut count = 0;
    boundary_cases::visit_limit_cases(|name, value, expected| {
        count += 1;
        let actual = build_support::generate_agent_model_from_str(&value.to_string());
        if let Some((limit, path)) = expected {
            match actual.expect_err(name) {
                build_support::BuildError::Definition(error) => {
                    assert_eq!(error.code, "limit_exceeded", "{name}: {error}");
                    assert_eq!(error.limit, Some(limit), "{name}: {error}");
                    assert_eq!(error.path, path, "{name}: {error}");
                    assert!(error.observed.unwrap() > error.maximum.unwrap());
                }
                error => panic!("{name}: missing shared resource error: {error}"),
            }
        } else {
            assert!(actual.is_ok(), "{name}: {actual:?}");
        }
    });
    assert_eq!(count, 46);
}

#[test]
fn generated_raw_output_contract_has_bounded_evaluation() {
    boundary_cases::visit_output_limit_cases(|name, value, schema, expected| {
        let definition = serde_json::json!({"agent_definition_schema_version":"2026-09-09.r1","agent_schema":schema,"actions":[]});
        let generated =
            build_support::generate_agent_model_from_str(&definition.to_string()).unwrap();
        assert!(generated.contains("fn validate_raw_response"));
        assert!(generated.contains("deny_unknown_fields"));
        let actual = build_support::definition_validation::validate_model_output(value, schema);
        if let Some((limit, path)) = expected {
            let error = actual.expect_err(name);
            assert_eq!(error.code, "limit_exceeded", "{name}");
            assert_eq!(error.limit, Some(limit), "{name}");
            assert_eq!(error.path, path, "{name}");
        } else {
            assert!(actual.is_ok(), "{name}: {actual:?}");
        }
    });
}

#[test]
fn malformed_json_uses_shared_codegen_error() {
    for raw in ["{", "not json", "[1,]", "{\"x\":NaN}"] {
        match build_support::generate_agent_model_from_str(raw)
            .expect_err("malformed source must fail")
        {
            build_support::BuildError::Definition(error) => {
                assert_eq!(error.code, "invalid_json");
                assert_eq!(error.path, "$");
                assert!(!error.corrective_action.is_empty());
            }
            error => panic!("missing shared parse failure: {error}"),
        }
    }
}
