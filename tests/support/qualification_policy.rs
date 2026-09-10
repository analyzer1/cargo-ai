//! Typed, sanitized evidence shared by probe execution and qualification summaries.

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, &'static str>;
pub const PROVIDERS: [&str; 5] = ["openai", "anthropic", "gemini", "xai", "mistral"];

pub fn hexadecimal(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn positive(value: &str) -> Result<u64> {
    if value.is_empty() || value.starts_with('0') || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid workflow identity");
    }
    value
        .parse()
        .map_err(|_| "workflow identity exceeds supported bounds")
}

pub fn required(provider: &str) -> bool {
    matches!(provider, "openai" | "anthropic")
}

pub fn label(provider: &str) -> &'static str {
    match provider {
        "openai" => "OpenAI",
        "anthropic" => "Anthropic",
        "gemini" => "Gemini",
        "xai" => "xAI",
        "mistral" => "Mistral",
        _ => "Unknown",
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pass,
    RateLimited,
    Failure,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub schema_version: u32,
    pub candidate: String,
    pub provider: String,
    pub run_id: String,
    pub run_attempt: String,
    pub probe_id: String,
    pub outcome: Outcome,
}

impl Record {
    pub fn parse(raw: &[u8]) -> Result<Self> {
        if raw.len() > 2048 {
            return Err("oversized probe evidence");
        }
        // Deserialize directly into a struct: duplicate and unknown fields must fail.
        let record: Self = serde_json::from_slice(raw).map_err(|_| "invalid probe evidence")?;
        if record.schema_version != 1
            || !hexadecimal(&record.candidate, 40)
            || !hexadecimal(&record.probe_id, 32)
            || !PROVIDERS.contains(&record.provider.as_str())
        {
            return Err("invalid probe identity");
        }
        positive(&record.run_id)?;
        positive(&record.run_attempt)?;
        Ok(record)
    }
}

pub struct Identity<'a> {
    pub candidate: &'a str,
    pub run_id: &'a str,
    pub run_attempt: &'a str,
    pub probe_id: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Cancelled,
    Skipped,
    NotConfigured,
    Missing,
    Unverified,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "✅ pass",
            Self::Fail => "❌ fail",
            Self::Cancelled => "🚫 cancelled",
            Self::Skipped => "⏭️ skipped",
            Self::NotConfigured => "⚪ not configured",
            Self::Missing => "❓ missing",
            Self::Unverified => "⚠️ not verified — rate limited",
        }
    }

    pub fn job_result(self) -> &'static str {
        match self {
            Self::Pass => "success",
            Self::Fail => "failure",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
            _ => "missing",
        }
    }
}

pub struct Decision {
    pub status: Status,
    pub accepted: bool,
}

pub fn evaluate(
    provider: &str,
    raw: &[u8],
    identity: &Identity<'_>,
    job: &str,
    enrollment: &str,
) -> Result<Decision> {
    if !PROVIDERS.contains(&provider) || !hexadecimal(identity.candidate, 40) {
        return Err("invalid expected identity");
    }
    positive(identity.run_id)?;
    positive(identity.run_attempt)?;
    if !required(provider) {
        match enrollment {
            "" | "false" if job == "skipped" && raw.is_empty() => {
                return Ok(Decision {
                    status: Status::NotConfigured,
                    accepted: true,
                });
            }
            "true" => {}
            _ => return Err("invalid enrollment or unexpected provider execution"),
        }
    }
    if job != "success" {
        return Err("provider job did not complete successfully");
    }
    let record = Record::parse(raw)?;
    if record.provider != provider
        || record.candidate != identity.candidate
        || record.run_id != identity.run_id
        || record.run_attempt != identity.run_attempt
        || identity.probe_id.is_some_and(|id| id != record.probe_id)
    {
        return Err("probe identity mismatch");
    }
    Ok(Decision {
        status: match record.outcome {
            Outcome::Pass => Status::Pass,
            Outcome::RateLimited => Status::Unverified,
            Outcome::Failure => Status::Fail,
        },
        accepted: record.outcome == Outcome::Pass
            || (!required(provider) && record.outcome == Outcome::RateLimited),
    })
}
