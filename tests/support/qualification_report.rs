//! A completed isolated probe reports evidence; policy decides qualification.

use serde::Deserialize;
use serde_json::json;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct Event {
    event_type: String,
    root_run_id: String,
    usage_log_format_version: Option<String>,
    agent_run_id: Option<String>,
    parent_agent_run_id: Option<String>,
    depth: Option<u32>,
    status: Option<String>,
    provider: Option<Provider>,
    error: Option<UsageError>,
}

#[derive(Deserialize)]
struct Provider {
    server: String,
    profile: String,
    model: String,
    auth_mode: String,
}

#[derive(Deserialize)]
struct UsageError {
    kind: String,
}

pub(super) struct Context {
    pub path: PathBuf,
    pub candidate: String,
    pub run_id: String,
    pub run_attempt: String,
    pub probe_id: String,
}

fn hexadecimal(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn decimal(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('0') && value.bytes().all(|b| b.is_ascii_digit())
}

impl Context {
    pub fn from_environment() -> Result<Option<Self>, &'static str> {
        let path = std::env::var_os("CARGO_AI_QUALIFICATION_REPORT");
        let probe = std::env::var_os("CARGO_AI_QUALIFICATION_PROBE");
        if path.is_none() && probe.is_none() {
            return Ok(None);
        }
        let value = |name| std::env::var(name).map_err(|_| "missing qualification identity");
        let context = Self {
            path: path.ok_or("missing qualification destination")?.into(),
            candidate: value("CARGO_AI_SHA")?,
            run_id: value("GITHUB_RUN_ID")?,
            run_attempt: value("GITHUB_RUN_ATTEMPT")?,
            probe_id: value("CARGO_AI_QUALIFICATION_PROBE")?,
        };
        context.validate()?;
        Ok(Some(context))
    }

    fn validate(&self) -> Result<(), &'static str> {
        if !self.path.is_absolute()
            || !hexadecimal(&self.candidate, 40)
            || !hexadecimal(&self.probe_id, 32)
            || !decimal(&self.run_id)
            || !decimal(&self.run_attempt)
        {
            return Err("invalid qualification identity");
        }
        Ok(())
    }

    pub fn write(
        &self,
        provider: &str,
        model: &str,
        profile: &str,
        token: &str,
        exit: Option<i32>,
        usage: &Path,
    ) -> Result<&'static str, &'static str> {
        self.validate()?;
        if !["openai", "anthropic", "gemini", "xai", "mistral"].contains(&provider) {
            return Err("invalid qualification provider");
        }
        let metadata = fs::symlink_metadata(usage).map_err(|_| "missing isolated usage")?;
        if !metadata.is_file() || metadata.len() > 64 * 1024 {
            return Err("invalid isolated usage");
        }
        let raw = fs::read_to_string(usage).map_err(|_| "invalid isolated usage")?;
        if token.is_empty() || raw.contains(token) {
            return Err("invalid usage redaction");
        }
        let outcome = classify(&raw, provider, model, profile, exit)?;
        let bytes = serde_json::to_vec(&json!({
            "schema_version": 1, "candidate": self.candidate, "provider": provider,
            "run_id": self.run_id, "run_attempt": self.run_attempt,
            "probe_id": self.probe_id, "outcome": outcome,
        }))
        .map_err(|_| "qualification serialization failed")?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)
            .map_err(|_| "qualification destination must be new")?;
        file.write_all(&bytes)
            .map_err(|_| "qualification write failed")?;
        Ok(outcome)
    }
}

pub(super) fn classify(
    raw: &str,
    provider: &str,
    model: &str,
    profile: &str,
    exit: Option<i32>,
) -> Result<&'static str, &'static str> {
    let events: Vec<Event> = raw
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .map_err(|_| "malformed usage evidence")?;
    let expected = [
        "usage_log_started",
        "agent_run_started",
        "provider_request_completed",
        "agent_run_completed",
        "root_run_completed",
    ];
    if events.len() != expected.len()
        || events
            .iter()
            .zip(expected)
            .any(|(event, kind)| event.event_type != kind)
    {
        return Err("incomplete or duplicate probe events");
    }
    let root = &events[0].root_run_id;
    let agent = events[1]
        .agent_run_id
        .as_deref()
        .ok_or("missing agent identity")?;
    if root.is_empty()
        || agent.is_empty()
        || events[0].usage_log_format_version.as_deref() != Some("2026-06-10.r1")
        || events.iter().any(|e| &e.root_run_id != root)
        || events[1..4].iter().any(|e| {
            e.agent_run_id.as_deref() != Some(agent)
                || e.parent_agent_run_id.is_some()
                || e.depth != Some(0)
        })
    {
        return Err("uncorrelated probe events");
    }
    let request = &events[2];
    let identity = request
        .provider
        .as_ref()
        .ok_or("missing provider identity")?;
    if identity.server != provider
        || identity.profile != profile
        || identity.model != model
        || identity.auth_mode != "api_key"
    {
        return Err("unexpected provider identity");
    }
    let terminal = match exit {
        Some(0) => "success",
        Some(1) => "failed",
        _ => return Err("probe did not exit normally"),
    };
    if events[3..]
        .iter()
        .any(|e| e.status.as_deref() != Some(terminal))
    {
        return Err("inconsistent terminal evidence");
    }
    match request.status.as_deref() {
        Some("success") if request.error.is_none() => Ok(if terminal == "success" {
            "pass"
        } else {
            "failure"
        }),
        Some("failed") if terminal == "failed" => {
            let error = request
                .error
                .as_ref()
                .ok_or("missing failed request classification")?;
            Ok(if error.kind == "ratelimited" {
                "rate_limited"
            } else {
                "failure"
            })
        }
        _ => Err("inconsistent request evidence"),
    }
}
