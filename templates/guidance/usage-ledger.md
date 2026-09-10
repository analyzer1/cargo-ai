# Cargo AI Usage Ledger

Use this file when a user wants token usage, provider timing, run timing, or embedding-friendly accounting for Cargo AI runs.

## What It Is

Cargo AI usage logging is an opt-in usage ledger. It records Cargo AI-owned runtime metadata so a caller can understand how much provider usage a run consumed and where time was spent.

It is not a general logging backend. Keep business logs, prompts, model outputs, decision traces, custom diagnostics, and writes to Datadog, Snowflake, S3, Postgres, or other systems inside explicit tools when the user asks for them.

## Enable It

For interpreted JSON definitions:

```bash
cargo ai run ./my_agent.json --usage-log ./usage.ndjson
```

For embedding products or shell wrappers:

```bash
CARGO_AI_USAGE_LOG=./usage.jsonl cargo ai run ./my_agent.json
```

For hatched agents:

```bash
./my_agent --usage-log ./usage.ndjson
CARGO_AI_USAGE_LOG=./usage.jsonl ./my_agent
```

If both `--usage-log <path>` and `CARGO_AI_USAGE_LOG=<path>` are present, the explicit CLI flag wins for that process.

## File Format

- `.ndjson` and `.jsonl` are both newline-delimited JSON: one complete JSON object per line.
- Cargo AI accepts any writable path, but prefer `.ndjson` or `.jsonl` for clarity.
- The file is append-friendly. A partial file is still useful if a run fails.
- Consumers should parse each line independently and ignore unknown fields for forward compatibility.

## Event Types

Common events:

- `usage_log_started`
- `agent_run_started`
- `provider_request_completed`
- `tool_run_started`
- `tool_run_completed`
- `agent_run_completed`
- `root_run_completed`

Provider requests use `provider_request_completed` for both success and failure. Successful provider requests include normalized `usage` when the provider reports counters. Failed provider requests include safe status/timing/error metadata when Cargo AI can measure it.

## Tree Reconstruction

Use these fields to rebuild the run tree:

- `root_run_id`: one Cargo AI-generated id for the whole root run, prefixed with `cai_run_`.
- `agent_run_id`: one Cargo AI-generated id for one execution of one agent, prefixed with `cai_agent_run_`.
- `parent_agent_run_id`: null for the root agent, otherwise the parent agent execution id.
- `depth`: display/filtering convenience for nested runs.
- `launched_by`: present when a child run was launched by an agent step or tool bridge.

Do not rely on `depth` alone. Sibling agents share a depth, repeated agents can run more than once, and recursive agents can have the same artifact/name at multiple depths.

## Agent Identity

Usage events include best-available agent metadata:

- `agent.source`: examples include `local_path`, `registry`, `inline_json`, `stdin_json`, and `hatched_agent`.
- `agent.artifact`: local path or generated binary path when known.
- `agent.name`: derived or authored display name when known.
- `agent.project_root`: project root for interpreted local runs when known.
- `agent.definition_sha256`: canonical definition hash for interpreted JSON definitions when Cargo AI has the source JSON.
- `agent.generated`: true for hatched/generated binaries.

For interpreted local JSON runs, expect `source: "local_path"`, an `artifact` path such as `./my_agent.json`, a derived `name`, and `definition_sha256`.

For hatched agents, expect `source: "hatched_agent"`, `generated: true`, the executable `artifact`, and the binary name. A hatched run may not expose the same interpreted-definition hash shape because the definition is embedded in generated code.

## Provider And Usage Fields

Provider events include:

- `provider.server`
- `provider.profile`
- `provider.auth_mode`
- `provider.model`
- `step.kind`, such as `agent_inference` or `generate_image`
- `duration_ms`
- `timing.provider_round_trip_ms` when measurable
- `status`
- `usage.input_tokens`
- `usage.output_tokens`
- `usage.total_tokens`

Anthropic, Gemini, Mistral, Ollama, OpenAI, and xAI usage counters are normalized into the same `input_tokens`, `output_tokens`, and `total_tokens` shape when reported. Compatible wire formats do not collapse identity: Mistral events use `provider.server = "mistral"`, and xAI Responses events use `provider.server = "xai"`. If a provider does not return counters, Cargo AI leaves `usage` null or omitted instead of estimating.

## Metadata Boundary

Usage logs must stay metadata-only. They should not include:

- prompts
- model output text
- generated image bytes or file contents
- tool arguments
- tool stdout or stderr
- tool return payloads
- profile tokens
- access tokens
- refresh tokens
- raw provider response bodies

When the user wants those details, create explicit project-local tools for that logging behavior and make the data handling visible in the tool contract.

## Quick Inspection

After a run, inspect the tree fields:

```bash
jq -c 'select(.agent) | {event_type, depth, agent_run_id, parent_agent_run_id, agent}' usage.ndjson
```

Inspect provider usage totals:

```bash
jq -c 'select(.event_type == "provider_request_completed") | {agent: .agent.name, model: .provider.model, status, usage, duration_ms}' usage.ndjson
```

## Project-owned Child Logs

When a project opts in with `[runtime] data_root = ".cargo-ai/data"`, an action's child `usage_log` path resolves under that fixed directory. Installed aliases continue to use their own `data/`. These controlled relative paths reject traversal and links. An explicit top-level `--usage-log` or `CARGO_AI_USAGE_LOG` destination keeps its caller-selected meaning; the project setting does not redirect it. Legacy projects without the setting retain their previous path behavior.
