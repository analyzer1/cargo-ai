# Cargo AI Start Here

Use this file when a user opens a blank folder and wants help creating their first Cargo AI agent that hatches into a CLI tool.

## First-Run Goal

Do not start by asking the user to choose between a single agent, an action agent, or a parent/child setup.

Start by asking:

1. What do you want this agent to do?
2. What inputs should it accept?
3. What output fields should it return?
4. Does it need files, local commands, email, or another agent?
5. Near the end: where should it run?
   - on your current machine
   - or portable across macOS, Windows, and Linux

If the runtime exposes the current OS, treat that as a hint for the local-machine option, but still confirm whether the user wants local-only behavior or cross-platform portability.

When files, URLs, or images are involved, also clarify:
- should this content be baked into the JSON as a fixed default?
- or should the caller supply it at runtime with flags such as `--input-file` or `--input-url`?
- for generated images, should any images be passed as `generate_image.reference_images` instead of only as model-facing input context?

When action behavior may vary by invocation, also clarify:
- should the caller gate the action or adjust a threshold at runtime?
- should the caller choose the image model at runtime?
- if yes, prefer top-level `runtime_vars` plus `--run-var` over asking the caller to edit JSON for each run

When the user wants token usage, runtime timing, provider timing, or embedding-friendly accounting:
- use `usage-ledger.md`
- keep usage logging opt-in with `--usage-log <path>` or `CARGO_AI_USAGE_LOG=<path>`
- do not design a custom logging backend unless the user explicitly wants business logs or a destination-specific tool

When selecting a model provider:
- `anthropic` uses Anthropic's native Messages API and requires an `api_key` profile or explicit token
- `gemini` uses Google's native Interactions API and requires an `api_key` profile or explicit token
- `mistral` uses Mistral's hosted Chat Completions API and requires an `api_key` profile or explicit token
- `openai` supports direct API-key profiles and the OpenAI-only account-session flow
- `ollama` supports local no-token operation and optional API-key-compatible endpoints
- `xai` uses xAI's Responses API for Grok models and requires an `api_key` profile or explicit token
- Anthropic supports text, URL-text, image input, structured output, and normalized usage; it does not support direct file input or `generate_image` in the current release
- Gemini supports text, URL-text, image input, structured output, and normalized usage; it sends `store = false` and does not support direct file input or `generate_image` in the current release
- Mistral supports text, URL-text, strict structured output, and normalized usage; it does not support image input, direct file input, or `generate_image` in the current release
- xAI supports text, URL-text, strict structured output, and normalized usage; it sends `store = false` and does not support image input, direct file input, or `generate_image` in the current release
- for real Anthropic credentials, create the profile first and pipe the key to `cargo ai profile set <name> --stdin`; never put a key in agent JSON or guidance
- for real Gemini credentials, create the profile first and pipe a Google AI Studio key to `cargo ai profile set <name> --stdin`; never put a key in agent JSON or guidance
- for real Mistral or xAI credentials, create the provider API profile first and pipe the key to `cargo ai profile set <name> --stdin`; never put a key in agent JSON or guidance
- a Claude.ai paid plan and Anthropic Console API billing are separate; do not imply that the consumer subscription supplies an API key or credits
- choose a current model available to the user's provider account instead of inventing a model ID or treating examples as a built-in allowlist
- preserve the authored output schema and fail closed if the provider rejects it or returns invalid JSON; never weaken constraints, switch models, or fall back to another provider silently
- a parent may use one provider while a `generate_image` or child-agent step selects another saved profile explicitly

When the user says they need a reusable local tool or native helper:
- confirm that they really need a project-local `kind: "tool"` capability rather than a plain `exec` step
- if they need new executable code and Cargo is available, prefer Rust inside a `cargo ai add tool <name>` scaffold instead of ad hoc Python, Node, or shell helper scripts
- if yes, switch to `.cargo-ai/guidance/tool-authoring.md`
- use the narrower tool files only as needed:
  - `tool-contract.md` for scaffold and protocol details
  - `tool-child-agents.md` when the tool must call child agents
  - `tool-hardening.md` for dependency and hardening review
- keep the tool workflow separate from the agent JSON workflow, then wire them back together with `cargo ai tools check --config <agent.json>`

When the user wants to share, install, update, or roll back reusable agents/tools as a package:
- use `package-workflow.md`
- keep the formal term `Cargo AI package`
- make sure the project has `.cargo-ai/project.toml` `[project]` identity and an explicit `[build.<profile>]`
- use `cargo ai package` for local package artifacts, `cargo ai packages install` for local machine installs, and `cargo ai packages publish|pull|list --account|update|rollback` for hosted package workflows

## How To Drive The Conversation

- Keep the questions in plain language.
- Ask only for information needed to draft the first JSON.
- Explain the inferred pattern in plain language after you have enough detail.
- Prefer examples over abstract explanations.

## What To Do Next

1. Use `pattern-selection.md` to infer the right Cargo AI shape.
2. If the request truly needs a local tool, use `tool-authoring.md`, keep the Rust tool crate small, and put custom behavior in `src/tool.rs` instead of rewriting the protocol adapter.
3. If the tool needs crates.io dependencies, apply `tool-hardening.md` dependency discipline before editing `Cargo.toml`.
4. Before presenting a tool as complete, perform the `tool-hardening.md` hardening review.
5. Copy the closest example from `examples/`.
6. Decide which inputs are baked into JSON, which are caller-supplied runtime inputs, and whether any action behavior belongs in `runtime_vars`.
7. If the caller needs usage or timing accounting, use `usage-ledger.md` for run commands and integration notes.
8. If the user wants reusable package distribution, switch to `package-workflow.md` after the agent/tool shape is valid.
9. Draft the JSON in canonical field order.
10. If the flow becomes complex, recommend a same-name sidecar Markdown file.
11. Validate with:
   - `cargo ai hatch <agent-name> --config <config.json> --check`
12. Fix reported errors before building. `hatch --check` uses the Cargo dev profile without exporting a binary; ordinary hatch exports a release-profile executable. Keep Cargo compile profiles separate from project build profiles that select package inputs.

## Behavioral Defaults

- Prefer the most minimal portable approach that satisfies the user's goal.
- Use platform-specific steps only when the user explicitly wants local-machine behavior or the task cannot be met portably.
- Remember that runtime input flags replace the full baked `inputs` array unless the caller sets `--input-mode append` or `--input-mode prepend`. If a run still needs text instructions plus a runtime file, URL, or image in replace mode, supply both kinds of runtime inputs.
- Prefer boxed ASCII diagrams for explanations by default.
- If Mermaid rendering is clearly supported, offer it as an option and ask whether the user wants it.

## Local Data Ownership

Inspect `.cargo-ai/project.toml` before choosing output paths. `cargo ai new` sets `[runtime] data_root = ".cargo-ai/data"`; `cargo ai init` leaves adoption explicit for existing work. Do not add or remove it silently. In an opted-in project, writing tools and Cargo AI-controlled outputs use the fixed project data root. Keep definitions, immutable inputs, and source code outside it. Read `tool-authoring.md`, `tool-child-agents.md`, and `package-workflow.md` before adapting older tool paths or shipping a project.
