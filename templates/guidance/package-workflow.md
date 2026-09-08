# Cargo AI Package Workflow

Use this file when a user wants to turn agents, tools, and assets into a reusable Cargo AI package, install a package locally, or use hosted package version management.

## Authoring Shape

A Cargo AI package is the versioned artifact. It can contain agent definitions, hatched-agent entrypoints, project-local tool source, and assets. Use `agent package` only as shorthand when the package mainly contains agents.

Project identity and build profiles live in `.cargo-ai/project.toml`:

```toml
format_version = 1

[project]
name = "data_integration"
version = "0.1.0"

[tools]
allow_global_fallback = true

# Request only when hosted package entrypoints need a declared tool or subprocess.
[package.permissions]
subprocess = "allowed"

# Bind every project-level alias reference to one hosted identity and version range.
[package_dependencies.data_integration]
hosted_source_id = "<opaque source id from cargo ai packages inspect>"
version = ">=1.2, <2.0"

[build.default]
agent_definitions = ["agents/lookup_account.json"]
hatched_agents = ["agents/daily_digest.json"]
tools = ["database_query"]
assets = ["assets/prompts/"]
```

Keep build lists explicit. `cargo ai package` and `cargo ai build` do not infer tools or agents from JSON references.

The package permission request is also explicit. Omit `[package.permissions]` when subprocess execution is not needed; Cargo AI keeps it blocked by default. Unsupported permission keys or values fail during packaging. A hosted install or version transition still requires the user to review and accept an `allowed` subprocess request. Treat that acceptance as a publisher-trust decision: hosted Rust tool materialization may execute build scripts, procedural macros, and related Cargo build-time code without an operating-system sandbox, using the current user's ambient filesystem, environment, and network authority.

## Build And Local Install

```bash
# Build a portable package root/archive from [build.default]
cargo ai package

# Install the current project package into Cargo AI Home
cargo ai packages install

# Install the current project package with an explicit alias
cargo ai packages install --as data_integration

# Install from a package root, archive, or cargo-ai-package.toml path
cargo ai packages install ./dist/data_integration --as data_integration

# Inspect, run, hatch, or uninstall the alias
cargo ai packages list
cargo ai packages inspect data_integration
cargo ai run data_integration::lookup_account
cargo ai hatch data_integration::daily_digest
cargo ai packages uninstall data_integration
```

Local alias behavior is version-aware: same version and hash is normally a no-op, newer semver upgrades, older semver requires `--downgrade`, and same-version content replacement or different package identity requires `--replace`. Reinstalling the same content repairs missing, corrupt, or wrong-target disposable runtime state while preserving `data/`.

Local-source installation builds declared source-backed tools with `cargo build --locked` for the current target. Compilation and artifact validation happen in the alias transaction: verified `package/` stays unchanged, only managed executable state enters disposable `runtime/`, and a failure leaves the previous alias plus `data/` recoverable. A compatible Rust toolchain and any uncached dependencies must be available.

Bare package names without `--account` are local-only. They must not trigger network lookup.

## Hosted Packages

```bash
# Hosted listings
cargo ai packages list --account
cargo ai packages list --account alice
cargo ai packages list --account --include-archived

# Publish the current project package
cargo ai packages publish

# Pull without machine install
cargo ai packages pull data_integration
cargo ai packages pull data_integration --owner-handle alice
cargo ai packages pull data_integration --owner-handle alice --version 1.2.3

# Install hosted packages into Cargo AI Home
cargo ai packages install data_integration --account --as data_integration
cargo ai packages install data_integration --account alice --as data_integration
cargo ai packages install data_integration --account alice --version 1.2.3 --as data_integration

# Accept a reviewed hosted subprocess permission when the package requests it
cargo ai packages install data_integration --account alice --as data_integration --accept-permissions

# Version management for installed hosted aliases
cargo ai packages update data_integration
cargo ai packages rollback data_integration --to 1.1.0
```

Hosted source identity is server/API-owned. Do not derive it from a public URL. `inspect` and hosted pull receipts expose the opaque hosted source id, hosted version id, optional owner handle, resolved version, and package hash.

Omitting `--version` resolves the latest eligible semver version at that moment and pins the exact resolved version locally. `update` moves forward only when a newer eligible version exists. `rollback` targets the exact `--to <version>`; it never means latest. Update and rollback resolve by the stored opaque source id, so an owner-handle change does not break an installed alias. Explicit handle installs and pulls verify the normalized owner returned by the service. `--include-archived` adds archived packages to the normal active listing.

Replacing an alias with a different source identity resets permission acceptance and requires both `--replace` and an explicit data choice: use `--keep-data` only after reviewing that the new publisher may read the old state, or `--delete-data` to start with an empty data directory.

Published versions for one hosted package identity must increase by semver. If a user needs to change same-version content, they should publish a higher version.

For hosted packages, Cargo AI compiles declared source tools only after the verified manifest requests `subprocess = "allowed"` and the user accepts that exact transition. The locked Cargo build is not sandboxed, so accept only trusted publisher code. A blocked hosted version can still run tool-free entrypoints; Cargo AI does not compile or execute its tools. Repeating `update` at the current latest version or `rollback --to` the current version repairs disposable runtime state when needed.

## Installed Layout And Permissions

Installed aliases use:

```text
$CARGO_AI_HOME/packages/<alias>/
  install.toml
  package/
  runtime/
    tools/
  data/
```

`install.toml` is Cargo AI-owned provenance. `package/` is the verified payload for the active exact version. `runtime/` is Cargo AI-owned, target-specific derived state for that version. Installed package tool resolution is confined to this version-bound runtime and never falls back to project or machine tools. `data/` is package-owned persistent local state.

Hosted update and rollback rematerialize `install.toml`, `package/`, and `runtime/`, then preserve `data/`. Install, update, rollback, and uninstall serialize each alias with an operating-system lock that is released automatically if the process exits. Publisher-authored migrations and total-refresh replacement are not supported.

Uninstall removes `install.toml`, `package/`, `runtime/`, and `data/`. When `data/` is nonempty, first back up or export it and then confirm permanent deletion explicitly:

```bash
cargo ai packages uninstall data_integration --delete-data
```

Installed package entrypoints resolve Cargo AI-controlled child `usage_log` paths under `data/`. Use this when a parent/observer agent needs a child-specific metadata-only JSONL file before a package tool imports it into SQLite or another package-owned store.

Default hosted package runtime boundaries:
- `package/` is readable verified payload.
- `runtime/` is disposable local materialization for the active package version and target; it is not publisher data and does not persist through version transitions.
- `data/` is the default writable package-owned root.
- project/workspace writes require an explicit grant and are not implied by install.
- Cargo AI-controlled file writes validate relative paths and reject traversal out of `data/`.
- unconstrained `exec` and tool subprocess steps are blocked for hosted packages unless the installed package permission profile explicitly allows them.
- first install or a version transition that newly requests subprocess execution requires `--accept-permissions` after review.
- project/workspace `read` and `read_write` requests are unsupported and fail even with `--accept-permissions`.
- installed JSON child agents must be declared package exports; hosted direct child executables require the accepted subprocess permission and resolve only from verified `package/`.
- definition-owned image/file inputs resolve inside verified `package/`; caller filesystem access requires an explicit runtime named-input override.
- package-generated relative child inputs resolve under `data/`; nested agents cannot reinterpret text or generated values as external filesystem paths.

Hatching a hosted alias exports trusted executable code outside the installed permission boundary. Review it and acknowledge that transition explicitly:

```bash
cargo ai hatch data_integration::daily_digest --allow-hosted-code
```

Hosted archives are checked before extraction. The client accepts at most 10 MiB compressed, 100 MiB expanded, 10,000 entries, and 1,024 bytes per normalized relative path.

## Cross-Package References

Declare the alias binding in the calling project's `.cargo-ai/project.toml`, then use the installed alias and exported entrypoint:

```toml
[package_dependencies.data_integration]
hosted_source_id = "<opaque hosted source id>"
version = "^1.2"
```

```json
{ "kind": "agent", "agent": "data_integration::lookup_account" }
```

Cargo AI verifies that a declared installed alias is hosted, has the declared source id, and matches the semver requirement before top-level run/hatch and child resolution. An undeclared local-source alias remains available for development, while a hosted declaration never binds a local alias. Package assembly preserves hosted declarations. A hosted package needs an accepted subprocess permission before it can invoke a cross-package child. A hatched binary resolves package children only while it is executed inside a Cargo AI project; it fails closed after being moved or launched without project context. It also requires `cargo ai` or `cargo-ai` on `PATH`, and that spawned Cargo AI process applies the full local/hosted identity and version policy.

Do not introduce unqualified global lookup by bare agent or tool name. Package internals remain private unless they are exported as entrypoints.

## Source Project Data

An opted-in source project declares `[runtime] data_root = ".cargo-ai/data"`. Assembly preserves that setting but excludes the data directory, even if tracked in Git or nested in an asset/tool-source tree. Explicit declarations overlapping runtime data and conflicting output roots fail before replacement. Keep immutable assets outside runtime data and never place credentials in distributable assets. Installed aliases still use their own `data/`; they do not inherit the producer's mutable files. Build roots acquire independent data only when a writing action needs it.
