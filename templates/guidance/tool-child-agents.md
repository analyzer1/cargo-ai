# Cargo AI Tool Child Agents

Use this file when a project-local tool needs to call one or more same-project child agents.

## When To Use The Helper

New scaffolded tools receive an `InvocationContext` argument plus a Cargo AI-owned helper in `src/agent_bridge.rs`.

Use that helper when the tool needs procedural control such as:
- splitting or iterating over input values
- calling one or more same-project child agents in sequence
- keeping iterator or fan-out logic in Rust instead of expanding the agent JSON or relying on the model to do deterministic string processing

## Current Helper Shape

Use it from `src/tool.rs`:

```rust
let request = ChildAgentRequest::new("./child_agent.json")
    .add_text_input("hello from the tool");
context.invoke_agent(request)?;
```

## First-Slice Rules

- same-project child-agent targets only; use same-level relative paths such as `./child_agent` or `./child_agent.json`
- tool execution itself does not consume an extra agent-depth hop
- a child-agent call from the tool consumes depth exactly as if the parent had called that child directly
- the helper carries the remaining runtime budget through to the child-agent invocation
- when the parent run enables `--usage-log` or `CARGO_AI_USAGE_LOG`, the helper carries the usage-ledger path, root run id, parent agent run id, and tool-launch metadata into child-agent invocations
- manual direct `describe` or `invoke` calls outside a parent Cargo AI tool step will not include child-agent bridge context
- keep custom orchestration in `src/tool.rs`; do not rewrite `src/agent_bridge.rs`

## Authoring Guidance

- keep the tool responsible for deterministic orchestration and splitting
- keep the child agent responsible for model-driven work
- prefer the helper over hand-rolled subprocess flags, depth propagation, or runtime-budget forwarding
- keep custom business logging inside tool code when needed; Cargo AI's built-in usage log records only usage/timing metadata and never tool arguments, stdout/stderr, or child-agent payloads

## Artifact Location and Data Location

For opted-in project runs, tools write relative to `.cargo-ai/data/`, while sibling child JSON/executables stay beside the original run artifacts. The runtime supplies an optional `artifact_root` in `runtime_context.agent_bridge`. The bridge resolves the existing single-sibling `./child` contract against that base, checks the artifact is a regular file rather than a link/reparse point, and invokes it from that artifact directory so immutable relative inputs keep their meaning. Installed runs derive the base from the verified entrypoint. Missing artifacts fail; the helper does not search elsewhere or copy children into mutable data.

If `artifact_root` is absent, the bridge retains its previous cwd-based contract. New scaffolds include this support. Cargo AI upgrades do not rewrite existing user tool bridges. Review an older bridge before opting an existing project into runtime data; adopting the setting alone does not repair its child lookup. An explicit source update and rebuild may be needed, with user changes preserved. This field grants no additional permission and does not change depth, profile, runtime-budget, or usage forwarding.
