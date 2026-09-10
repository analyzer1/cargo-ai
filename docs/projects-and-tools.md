# Projects and local tools

[Documentation hub](./README.md) · [Cargo AI README](../README.md)

Cargo AI projects give agent definitions and source-backed tools one explicit local boundary. Use a project when an agent needs a reusable local capability behind a `kind: "tool"` step, or when you want to assemble agents, tools, and assets together later.

For exhaustive, version-matched assistant guidance, generate the offline bundle with `cargo ai add guidance` or read the repository sources under [`templates/guidance/`](../templates/guidance/). This page is the human workflow and safety guide.

## Create or initialize a project

Create a new project directory:

```bash
cargo ai new my-tool-project
cd my-tool-project
```

Or initialize the current directory:

```bash
cargo ai init
```

Add optional coding-assistant guidance with one or both discovery styles:

```bash
cargo ai add guidance --style codex
cargo ai add guidance --style claude
```

The project boundary is `.cargo-ai/project.toml`. Cargo AI discovers it from the current path and its ancestors. `cargo ai new` and `cargo ai init` create the project identity and tool policy; a combined manifest with optional runtime defaults can look like:

```toml
format_version = 1

[project]
name = "my_tool_project"
version = "0.1.0"

[tools]
allow_global_fallback = true

[runtime.defaults]
inference_timeout_in_sec = 600
max_runtime_in_sec = 600
max_agent_depth = 5
```

`cargo ai new` and `cargo ai init` write `allow_global_fallback = true`, which permits project commands to use machine-installed tools after project lookup. If you hand-author the manifest and omit the setting, Cargo AI treats the project as project-only. Set it explicitly to `false` when the project must refuse machine/global fallback:

```toml
[tools]
allow_global_fallback = false
```

Runtime defaults are optional. For repeated `run` workflows, precedence is:

- inference timeout: CLI override, project default, selected profile, built-in default;
- maximum runtime: CLI override, project default, built-in default;
- maximum agent depth: CLI override, project default, built-in default.

Maximum runtime and depth cascade through the invocation tree. Inference timeout stays local to an invocation unless a child explicitly selects a different profile or timeout.

See [Packages](./packages.md) for `[build.<profile>]`, package permissions, and hosted dependency declarations.

## Project-owned runtime data

New projects opt in to a fixed data directory in `.cargo-ai/project.toml`:

```toml
[runtime]
data_root = ".cargo-ai/data"
```

`cargo ai init` preserves existing behavior and does not add this setting. To adopt it in an existing project, review the tool paths and then add the setting explicitly. Other values are rejected. A project without the setting keeps its existing working-directory behavior. Initialization fails on malformed metadata before initializing Git or replacing files.

In an opted-in project, Cargo AI-controlled image outputs, generated child file/image paths and image references, and action-owned child usage logs resolve under the project boundary's `.cargo-ai/data/`. Tools run with that directory as their working directory. The directory is created only when a writing action or tool invocation needs it; scaffold, inspection, and path validation do not create it. Static inputs and caller-selected input/log paths retain their existing meaning.

Keep definitions, immutable assets, and tool sources outside runtime data. Build/package output retains the opt-in but excludes runtime data, including data nested inside copied assets or source tools. Explicit declarations and output destinations that overlap the reserved data root are rejected before output replacement. The managed Git ignore block excludes `/.cargo-ai/data/`; it does not untrack files already in your index or ignore an unrelated `data/` directory.

Controlled data paths reject absolute/drive/UNC forms, parent traversal, and existing symbolic links or reparse points. This is path validation, not an OS sandbox: tools and subprocesses retain their ambient permissions. Use trusted code and keep installation, authentication, and sensitive actions explicit.

New tool bridges keep sibling child artifacts at their original artifact location while the tool writes in data. Existing user-owned tool code is not automatically rewritten; review its bridge and relative paths before adopting the data setting. See the [tool-child guidance](../templates/guidance/tool-child-agents.md) for that compatibility boundary.

## Scaffold a Rust tool

When agent JSON alone is not enough and Cargo is available, prefer a generated Rust tool over an ad hoc Python, Node, or shell helper:

```bash
cargo ai add tool hello_tool
```

The scaffold separates author code from Cargo AI-managed protocol code:

```text
tools/hello_tool/
  Cargo.toml
  src/
    main.rs
    lib.rs
    agent_bridge.rs
    tool.rs
.cargo-ai/tools/hello_tool/tool.json
```

Cargo creates `Cargo.lock` later when it first resolves or builds the tool's dependencies.

Implement custom behavior in `tools/hello_tool/src/tool.rs`. Keep `src/main.rs` thin, and do not rewrite `src/lib.rs` or `src/agent_bridge.rs` unless you are deliberately changing the protocol adapter itself.

Define and validate:

- the tool description and examples;
- typed params and clear invalid-input errors;
- nullable-string result metadata;
- an accurate `resource_profile` for filesystem, network, subprocess, environment, credentials, UI, and background-process behavior;
- the `invoke` behavior and its failure modes.

Tool params may be `string`, `boolean`, `integer`, `number`, `array`, or `object`. Cargo AI validates only the top-level kind for arrays and objects; the tool owns deeper deserialization and shape validation. A step with `output_variable` requires the actual `invoke` result to be a non-null string even though the `describe` result schema is nullable.

## Build, inspect, and validate

Build the managed project artifact, then inspect both its contract and its integration with an agent:

```bash
cargo ai tools build hello_tool --target aarch64-apple-darwin
cargo ai tools describe hello_tool
cargo ai tools lint hello_tool
cargo ai tools check hello_tool
cargo ai tools check --config ./my_agent.json
cargo ai hatch my_agent --config ./my_agent.json --check
```

`cargo ai tools build` uses Cargo’s dev profile for the authoring loop. Final `cargo ai build` artifacts and tools materialized by package installation use the release profile. Managed tool paths separate the target and compile profile; existing tool manifests remain readable, and existing installations are rebuilt only through an explicit lifecycle operation. Configure optimization in the tool’s `Cargo.toml`, not in Cargo AI build-profile metadata.

Use the target triple for the platform you are building. `tools lint` statically checks Cargo AI metadata linkage and source/scaffold expectations for a project-local source-backed tool. Machine-only and binary-only tools are not lint targets. `tools check` exercises the tool contract, while `hatch --check` validates the agent scaffold and compile path without exporting a binary.

Wire the tool into agent JSON with a tool action:

```json
{
  "kind": "tool",
  "name": "hello_tool",
  "params": {
    "name": "Cargo AI"
  },
  "output_variable": "greeting"
}
```

By default, `run`, `hatch --check`, and `hatch` audit referenced tools against their `describe` contracts before execution or compilation. They resolve project tools first and consult Cargo AI Home only when the manifest permits global fallback. Use `--ignore-tools` only when you intentionally accept a later failure if execution reaches an unavailable or incompatible tool.

Ordinary `cargo ai hatch` exports the agent binary only; it does not copy project tool artifacts beside the binary. A hatched binary launched inside a Cargo AI project uses the same project-first lookup. Outside a project, it can use machine-installed tools but not project-only tools.

`cargo ai tools build <name>` materializes managed state inside the current project. Reusable machine-scope installation goes through the [package workflow](./packages.md), not direct promotion of a local tool artifact.

## Generated state belongs to Cargo AI

Treat `.cargo-ai/tools/` and `.cargo-ai/agents/` as Cargo AI-owned generated state. Do not manually copy, move, rename, symlink, or delete files there while diagnosing a build or runtime problem.

If managed state was changed manually, stop using that workspace as evidence of a Cargo AI artifact defect. Reproduce from a fresh workspace or freshly regenerated state so the result is trustworthy.

Project bootstrap may add `.gitignore` entries for managed build state when version control is enabled. The separate `cargo ai add guidance` command creates `AGENTS.md` and/or `CLAUDE.md` discovery entrypoints plus `.cargo-ai/guidance/`, a self-contained, version-matched authoring bundle, and manages the related ignore entries.

The generated guidance bundle is the exhaustive offline assistant contract. This human guide summarizes the workflow without replacing that bundle.

## Maintain assistant guidance

```bash
cargo ai guidance status
cargo ai guidance update
```

Status is read-only, including startup: it does not initialize Cargo AI Home, migrate credentials, check the network, acquire a new lock, recover an interrupted update or repair ignores. It reports missing, current, update available, locally modified, incomplete, malformed, or legacy/unmanaged guidance. Separate notices explain preserved instructions, missing ignore entries and tracked generated files.

An explicit update uses only the installed binary's bundle. Its manifest records format, producing version, bundle digest and managed file hashes. Only unchanged previously owned files may be replaced or retired; newly added bundle files are checked for conflicts first. Existing user-owned or modified `AGENTS.md` and `CLAUDE.md` remain byte-for-byte intact, with a loader snippet for the owner to review. Fresh add creates the same ownership manifest used by update. Identical legacy files without a manifest do not establish ownership.

Updates serialize writers, preflight the complete set, and stage a recoverable replacement including any new root loader and ignore-file repair. Failure before commit restores the prior set or leaves an explicitly incomplete transaction for verified recovery on a later update. After commit, recovery retains the verified new set and finishes cleanup. Status never performs recovery. A directory swap can briefly make guidance unavailable to concurrent readers; it does not promise uninterrupted reads. If recovery data or owned files changed unexpectedly, stop and reconcile them rather than deleting recovery state or inventing a manifest. Keep a copy of local edits before resolving a blocked bundle.

Add/update repairs only Cargo AI's marked ignore block, and only when valid project metadata selects Git and Git confirms that exact project boundary. It preserves unrelated entries and never changes the Git index. `vcs = "none"`, missing/malformed metadata, and an unrelated ancestor repository cause no automatic ignore mutation. Linked/reparse paths are rejected.

Build/package recursion and tool-source packaging exclude incidental `.cargo-ai/guidance/` bundles and manifest-owned root loaders. User-owned instruction files remain ordinary source. Deliberately listing a guidance path as an asset includes it; guidance locks and transaction/recovery state are always excluded. This does not relax runtime-data exclusion.

## Tools that call child agents

New tool scaffolds include a Cargo AI-owned child-agent helper in `src/agent_bridge.rs`. Use it through the `InvocationContext` passed to `src/tool.rs` instead of hand-building subprocess flags, depth propagation, runtime-budget forwarding, or usage-ledger metadata.

```rust
let request = ChildAgentRequest::new("./child_agent.json")
    .add_text_input("hello from the tool");
context.invoke_agent(request)?;
```

The helper supports same-project child targets with same-level paths such as `./child_agent` or `./child_agent.json`. The tool step itself does not consume an additional agent-depth hop; each child call consumes depth as if the parent agent called that child directly. Remaining runtime budget and enabled usage-ledger context propagate to the child.

Manual `describe` or `invoke` calls outside a parent Cargo AI tool step do not include child-agent bridge context. Keep deterministic splitting and iteration in the tool, and model-driven work in the child agent.

See the generated-guidance sources for the complete contracts:

- [Tool authoring](../templates/guidance/tool-authoring.md)
- [Tool contract](../templates/guidance/tool-contract.md)
- [Tool child agents](../templates/guidance/tool-child-agents.md)
- [Tool hardening](../templates/guidance/tool-hardening.md)

## Validation ladder

Prove the smallest deterministic layer before moving upward:

1. use `cargo test` for crate-local Rust behavior;
2. run `cargo ai tools lint`, `describe`, `build`, and `check`;
3. validate the agent/tool pairing and run `hatch --check`;
4. test the leaf tool or child agent with deterministic input;
5. test tool-to-child fan-out;
6. test the parent orchestration path;
7. add live URLs or provider behavior;
8. perform real side effects last.

Do not keep changing higher layers while a lower layer is still failing. For UI and background-process tools, separate artifact creation from UI launch where practical, provide a smoke control such as `open_window=false`, and make process lifetime explicit. Use process inspection or termination only to clean up a specific long-lived child left by your own live run.

## Dependency and resource safety

Treat every tool dependency as trusted executable code. Prefer the standard library or stable, focused, actively maintained crates; enable only required features; and avoid Git/path dependencies, unpublished forks, prereleases, unnecessary build scripts, and unnecessary native dependencies unless the tradeoff is intentional.

Keep `Cargo.lock`, review the enabled dependency tree, and run available safety checks:

```bash
cargo tree -e features
cargo audit
cargo deny check
```

Before calling a tool complete, review parameter validation, errors, path traversal and overwrite behavior, network URLs and timeouts, authentication and data exposure, environment and credential reads, subprocess/UI lifetime, cleanup, and partial-output failure modes. The declared `resource_profile` must match the real behavior.

For complete resource and dependency review criteria, see [Tool hardening](../templates/guidance/tool-hardening.md).

---

[Documentation hub](./README.md) · [Cargo AI README](../README.md)
