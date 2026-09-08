# Cargo AI Tool Authoring

Use this file when the user wants a project-local tool crate, not just agent JSON.
This file is the workflow overview and entrypoint for the more detailed tool guidance files.

## When To Reach For A Tool

Prefer normal agent JSON first.

Use a local Cargo AI tool when the user needs:
- deterministic local behavior behind a reusable `kind: "tool"` step
- typed params checked against a tool-owned `describe` contract
- a reusable local capability that multiple agents can call
- something more structured than an ad hoc `exec` wrapper

If plain `exec`, `email_me`, `agent`, or `generate_image` can solve the request cleanly, keep the workflow in JSON and do not invent a tool.

When executable code is needed and Cargo is available, prefer a Rust Cargo AI tool:
- scaffold it with `cargo ai add tool <tool_name>`
- implement behavior inside the generated Rust crate
- avoid creating ad hoc Python, Node, or shell helper scripts by default

Only use another language or standalone script when the user explicitly asks, Cargo is unavailable, or the task cannot reasonably fit the current `describe` / `invoke` tool contract.

## Project Preconditions

Tool authoring assumes the workspace is already a Cargo AI project.

If the project boundary is missing, bootstrap it first:

1. New folder:
   - `cargo ai new <path>`
2. Existing folder:
   - `cargo ai init`
3. If the user wants assistant guidance too:
   - `cargo ai add guidance --style codex`
   - `cargo ai add guidance --style claude`
   - repeat `--style` to install both discovery entrypoints

Current minimum project metadata:

```text
.cargo-ai/project.toml
```

with:

```toml
format_version = 1

[project]
name = "my-tool-project"
version = "0.1.0"

[tools]
allow_global_fallback = true
```

`cargo ai new/init` writes that default policy plus starter project identity so new projects can reuse machine-level tools when desired and already have a package/publish identity. If you hand-author `project.toml` and omit `allow_global_fallback`, Cargo AI treats that as project-only lookup.

## Default Local Tool Workflow

1. Scaffold the tool:
   - `cargo ai add tool <tool_name>`
2. Review the generated crate:
   - `tools/<tool_name>/Cargo.toml`
   - `tools/<tool_name>/src/main.rs`
   - `tools/<tool_name>/src/lib.rs`
   - `tools/<tool_name>/src/agent_bridge.rs`
   - `tools/<tool_name>/src/tool.rs`
   - `.cargo-ai/tools/<tool_name>/tool.json`
   - treat `.cargo-ai/tools/...` and `.cargo-ai/agents/...` as Cargo AI-owned generated state; do not manually replace, rename, symlink, or delete files there during validation
3. Implement the tool behavior in `src/tool.rs`:
   - description
   - params
   - result metadata
   - resource profile
   - examples
   - invoke behavior
4. Build the managed artifact:
   - `cargo ai tools build <tool_name> --target <triple>`
5. Inspect and validate it:
   - `cargo ai tools describe <tool_name>`
   - `cargo ai tools lint <tool_name>`
   - `cargo ai tools check <tool_name>`
6. Wire the tool into agent JSON:
   - `kind: "tool"`
   - `name`
   - `params`
7. Validate the agent/tool pairing:
   - `cargo ai tools check --config <agent.json>`
   - `cargo ai hatch <agent-name> --config <agent.json> --check`
8. For live behavior, prefer leaf-first runtime checks:
   - run the leaf tool or child agent in print/smoke mode first
   - run the parent orchestration path next
   - run real side effects such as email delivery last
   - if the workflow also depends on live URLs or provider behavior, prove the deterministic hardcoded-input path first

If a debugging session manually mutates `.cargo-ai/tools/...` or `.cargo-ai/agents/...`, stop trusting that workspace as a clean diagnostic surface. Move the repro to a fresh workspace or freshly regenerated managed state instead of continuing to patch around contaminated generated artifacts.

For troubleshooting, identify the first failing layer and work upward:
- leaf agent first
- tool-only deterministic input next
- tool-to-child fan-out next
- parent orchestration next
- live URLs/providers after that
- side effects last

Do not keep changing higher layers while a lower layer is still failing.

Runtime lookup stays project-first:
- `cargo ai run`, `cargo ai hatch --check`, and ordinary `cargo ai hatch` audit tools up front
- they resolve tools from the current Cargo AI project first and only use Cargo AI Home when `.cargo-ai/project.toml` allows global fallback
- ordinary `cargo ai hatch` exports only the binary; it does not copy tool artifacts next to the output
- a hatched binary run from inside a project uses that same project-first lookup, while a run outside any project can only rely on machine-installed tools

If a tool should ship inside an explicit project build root, list it under `.cargo-ai/project.toml` in `[build.<profile>].tools`. `cargo ai build` only packages project-attached tools named there; it does not infer tool dependencies from agents and it does not pull machine-only tools into the build automatically.

If a tool should ship inside a portable project source package, use that same `[build.<profile>].tools` list with `cargo ai package`. The package step reuses the build profile directly, copies the tool crate source plus project tool metadata, and leaves built tool binaries out of the portable package root.

For package install, publish, hosted install, update, rollback, provenance, and installed package data boundaries, switch to `package-workflow.md` after the local tool and agent definitions validate.

If `cargo ai init/new` was run with the default VCS mode, it will also initialize Git and create or update `.gitignore` for generated guidance and managed build state. If Git is unavailable, rerun with `--vcs none`.

## Guidance Map

Use the narrower guidance files for detailed rules:

- `tool-contract.md`
  - scaffold layout
  - `tool.json`
  - `describe` / `invoke`
  - nullable string result rules
  - lint versus check
  - testing ladder and process hygiene
  - runtime resolution and failure behavior

- `tool-child-agents.md`
  - `InvocationContext`
  - `ChildAgentRequest`
  - same-project child-agent rules
  - depth and runtime-budget pass-through

- `tool-hardening.md`
  - dependency discipline
  - hardening review
  - UI/background-process guidance

For the current MVP, assume:
- one logical tool
- one Cargo crate
- one primary binary target

## What To Tell The Authoring Assistant To Do

If the user wants a local tool, ask the authoring assistant to:
- scaffold or inspect the local tool crate
- implement tool metadata and behavior in `src/tool.rs`
- keep `src/main.rs` thin
- avoid rewriting the protocol adapter in `src/lib.rs` unless the tool contract itself must change
- build with `cargo ai tools build <tool_name> --target <triple>`
- validate with `cargo ai tools describe`, `cargo ai tools lint`, `cargo ai tools check`, and `cargo ai hatch --check`
- prefer leaf-first live testing before parent orchestration when runtime behavior depends on URLs, providers, or child-agent calls
- identify the first failing layer and prove the smallest deterministic layer first before changing higher layers
- use `tool-contract.md` for scaffold and protocol details
- use `tool-child-agents.md` when the tool needs to call child agents
- use `tool-hardening.md` for dependency selection and completion review

Do not invent a second tool runtime or manifest format beyond the current scaffold and `tool.json`.

## Runtime Data and Relative Paths

With `[runtime] data_root = ".cargo-ai/data"`, Cargo AI invokes tools with the project's `.cargo-ai/data/` as cwd, creating it at invocation time. Installed tools continue to use their alias `data/`. Without the project setting, existing cwd behavior remains. Use relative paths for mutable tool output and keep immutable assets with the project artifacts. This cwd choice is not a filesystem sandbox; accurately declare resources and obtain authorization for sensitive operations.

New bridges resolve sibling child artifacts independently of the tool's data cwd. See `tool-child-agents.md`. Updating Cargo AI does not overwrite existing tool sources or bridges. Before an existing project adopts the data setting, review/rebuild its tool code and test child calls; do not copy children into data or silently replace user code to compensate for an old bridge.
