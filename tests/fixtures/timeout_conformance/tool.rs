//! A procedural child call through the installed scaffold helper.

use crate::{
    AccessLevel, ChildAgentRequest, InvocationContext, ParamSpec, ResourceProfile, ResultSpec,
    SelfTestSpec, ToolError,
};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) const TOOL_NAME: &str = "timeout_bridge";

pub(crate) fn description() -> &'static str {
    "Invoke one child for the isolated timeout fixture."
}

pub(crate) fn params() -> BTreeMap<String, ParamSpec> {
    ["artifact", "profile", "child_mode"]
        .into_iter()
        .map(|name| {
            (
                name.to_string(),
                ParamSpec {
                    kind: "string".to_string(),
                    required: true,
                    description: name.to_string(),
                    default: None,
                },
            )
        })
        .collect()
}

pub(crate) fn result() -> ResultSpec {
    ResultSpec {
        kind: "string".to_string(),
        nullable: true,
        description: "Child completion.".to_string(),
    }
}

pub(crate) fn resource_profile() -> ResourceProfile {
    ResourceProfile {
        network: AccessLevel::None,
        filesystem_read: AccessLevel::Optional,
        filesystem_write: AccessLevel::Optional,
        subprocess: AccessLevel::Required,
        env_read: AccessLevel::Optional,
        credential_access: AccessLevel::None,
    }
}

pub(crate) fn self_test() -> SelfTestSpec {
    SelfTestSpec {
        supported: false,
        safe: false,
        description: "The process fixture supplies the isolated provider.".to_string(),
    }
}

pub(crate) fn minimal_example_params() -> BTreeMap<String, Value> {
    BTreeMap::from([
        (
            "artifact".to_string(),
            Value::String("./child.json".to_string()),
        ),
        ("profile".to_string(), Value::String(String::new())),
        ("child_mode".to_string(), Value::String("none".to_string())),
    ])
}

pub(crate) fn full_example_params() -> BTreeMap<String, Value> {
    minimal_example_params()
}

pub(crate) fn invoke(
    params: BTreeMap<String, Value>,
    context: InvocationContext,
) -> Result<Option<String>, ToolError> {
    let get = |name| {
        params
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::new(format!("Missing string parameter {name}")))
    };
    let mut request =
        ChildAgentRequest::new(get("artifact")?).add_run_var("mode", get("child_mode")?);
    if !get("profile")?.is_empty() {
        request = request.with_profile(get("profile")?);
    }
    context.invoke_agent(request)?;
    Ok(Some("child completed".to_string()))
}
