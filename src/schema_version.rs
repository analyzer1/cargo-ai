//! Canonical local schema-version helpers for agent definitions.
//!
//! Format contract: `YYYY-MM-DD.rN` (example: `2026-03-03.r1`).

use serde_json::Value;

const ROOT_AGENTCFG: &str = include_str!("../.agentcfg");

pub const SCHEMA_VERSION_EXAMPLE: &str = crate::definition_validation::STRICT_SCHEMA_VERSION;
pub const AGENT_DEFINITION_SCHEMA_VERSION_KEY: &str = "agent_definition_schema_version";

pub fn current_schema_version() -> String {
    extract_schema_version_from_agentcfg(ROOT_AGENTCFG)
        .unwrap_or_else(|| SCHEMA_VERSION_EXAMPLE.to_string())
}

pub fn extract_schema_version_from_agentcfg(agentcfg_contents: &str) -> Option<String> {
    serde_json::from_str::<Value>(agentcfg_contents)
        .ok()
        .and_then(|json| {
            json.get(AGENT_DEFINITION_SCHEMA_VERSION_KEY)
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
        })
        .filter(|value| is_valid_schema_version(value))
}

pub fn is_valid_schema_version(value: &str) -> bool {
    crate::definition_validation::parse_schema_version(value).is_some()
}

#[cfg(test)]
mod tests {
    use super::{
        current_schema_version, extract_schema_version_from_agentcfg, is_valid_schema_version,
        SCHEMA_VERSION_EXAMPLE,
    };

    #[test]
    fn accepts_expected_schema_version_format() {
        assert!(is_valid_schema_version("2026-03-03.r1"));
        assert!(is_valid_schema_version("2024-02-29.r3"));
        assert!(is_valid_schema_version("2030-12-31.r12"));
    }

    #[test]
    fn rejects_legacy_or_invalid_schema_version_values() {
        assert!(!is_valid_schema_version("0.0.10"));
        assert!(!is_valid_schema_version("2026-03-03"));
        assert!(!is_valid_schema_version("2026-13-03.r1"));
        assert!(!is_valid_schema_version("2025-02-29.r1"));
        assert!(!is_valid_schema_version("2026-03-03.r0"));
        assert!(!is_valid_schema_version("2026-03-03.rX"));
    }

    #[test]
    fn extracts_only_valid_schema_versions_from_agentcfg() {
        let valid = extract_schema_version_from_agentcfg(
            r#"{"agent_definition_schema_version":"2026-03-03.r2","inputs":[{"type":"text","text":"x"}],"agent_schema":{"type":"object","properties":{}},"actions":[]}"#,
        );
        assert_eq!(valid.as_deref(), Some("2026-03-03.r2"));

        let invalid = extract_schema_version_from_agentcfg(
            r#"{"agent_definition_schema_version":"0.0.10","inputs":[{"type":"text","text":"x"}],"agent_schema":{"type":"object","properties":{}},"actions":[]}"#,
        );
        assert!(invalid.is_none());

        let legacy = extract_schema_version_from_agentcfg(
            r#"{"version":"2026-03-03.r2","inputs":[{"type":"text","text":"x"}],"agent_schema":{"type":"object","properties":{}},"actions":[]}"#,
        );
        assert!(legacy.is_none());
    }

    #[test]
    fn current_schema_version_is_valid_date_revision_value() {
        let current = current_schema_version();
        assert!(is_valid_schema_version(&current));
        assert_eq!(current, SCHEMA_VERSION_EXAMPLE);
    }
}
