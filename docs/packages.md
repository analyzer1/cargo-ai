# Build and package workflows

[Documentation hub](./README.md) · [Cargo AI README](../README.md)

A Cargo AI package is a versioned unit that can contain agent definitions, hatchable entrypoints, project-local tool source, assets, and hosted package bindings. Build roots are target-specific runnable outputs; packages are source-portable inputs for local install, sharing, and hosted publication.

The generated [package workflow](../templates/guidance/package-workflow.md) is the exhaustive version-matched assistant contract. This page explains the same lifecycle for people operating a project or reviewing a package.

## Select package contents explicitly

Project identity, permissions, hosted dependencies, and build profiles live in `.cargo-ai/project.toml`:

```toml
format_version = 1

[project]
name = "data_integration"
version = "1.2.0"

[tools]
allow_global_fallback = true

# Request only when hosted entrypoints need a declared tool or subprocess.
[package.permissions]
subprocess = "allowed"

# Bind each hosted alias reference to one source identity and version range.
[package_dependencies.reports]
hosted_source_id = "<opaque source id from cargo ai packages inspect>"
version = ">=1.2, <2.0"

[build.default]
agent_definitions = ["agents/lookup_account.json"]
hatched_agents = ["agents/daily_digest.json"]
tools = ["database_query"]
assets = ["assets/prompts/"]
```

Keep every build list explicit:

- `agent_definitions` copies JSON/config definitions into a target build or package;
- `hatched_agents` produces target-specific binaries in a build, but copies the JSON source definition into a portable package;
- `tools` rebuilds named project-attached tools for a build and copies their source plus metadata into a package;
- `assets` copies listed project-relative files or directories.

Cargo AI does not infer agents or tools from JSON references. The same agent path may appear in both agent lists when a runnable build should contain both the source definition and its compiled binary.

Only project-attached, source-backed tools named in the selected profile are eligible. A machine-only tool is never pulled into a build or package automatically. Attach it to the project first.

## Choose a build root or a source package

Create a target-specific runnable build root:

```bash
cargo ai build
cargo ai build release --target aarch64-apple-darwin
```

The positional profile selects `[build.<profile>]` inputs and defaults to `default`; even a profile named `release` is an input selection. Final agents and tools always compile with Cargo’s release profile. The generated build manifest records this separately as `cargo_compile_profile = "release"`. Unless `--output-dir` is set, output goes to `target/cargo-ai/build/<profile>/<target>/release/`. The assembled root contains generated project metadata, copied definitions and assets, managed tool state, and root-level hatched binaries. Its generated tool policy is project-only so it does not silently depend on unrelated machine tools.

Create a source-portable package from the same selection:

```bash
cargo ai package
cargo ai package release
```

Unless `--output-dir` is set, package output goes to `target/cargo-ai/package/<profile>/`. It includes JSON definitions, source-backed tool crates and metadata, assets, generated `.cargo-ai/project.toml`, and `cargo-ai-package.toml`; it does not include target binaries. Project name and version are copied into the generated manifests.

Both commands accept `--output-dir`; replacing an existing explicit destination requires `--force`. Run `cargo ai package` and inspect the reported package, archive, and request sizes before publishing asset-heavy work. The current hosted path accepts an **Estimated request** of at most `5,500,000` bytes. Because that serialized request includes base64 and JSON overhead, the archive itself must be materially smaller.

## Mutable data stays local

Build and package assembly retain `[runtime] data_root = ".cargo-ai/data"` when the source project adopts it. They exclude runtime data independently of Git tracking, including nested `.cargo-ai/data/` directories in copied assets and tool sources. An explicit input declaration that includes the reserved root, or an output destination that overlaps it, fails before replacing existing output. Declare immutable assets individually or under an unrelated asset directory; never use the data root to distribute credentials or live state.

A runnable build gets its own project data on first writing execution. An installed alias continues to use its separate `data/` directory, regardless of the source project's setting. Different projects and aliases do not share those files. See [project-owned runtime data](./projects-and-tools.md#project-owned-runtime-data) for adoption, path checks, and older-tool compatibility.

## Permission requests and publisher trust

Hosted packages are default-deny for subprocess execution. Cargo AI does not infer permission from a declared tool. Omit `[package.permissions]` when no tool or subprocess is needed; unsupported permission keys or values fail packaging.

When a package needs a declared tool, direct executable child, or another subprocess after hosted install, it must request:

```toml
[package.permissions]
subprocess = "allowed"
```

A first hosted install or version transition that newly enables this permission still requires the user to review the printed profile and pass `--accept-permissions`. Project/workspace `read` and `read_write` permission requests are unsupported and fail even with acceptance.

> [!WARNING]
> Hosted source-tool compilation is locked but not sandboxed. Package build scripts, procedural macros, and related build-time code run with your ambient filesystem, environment, and network authority. Accept subprocess-enabled packages only from a publisher you trust.

A hosted version whose permission has not been accepted may still run tool-free entrypoints. Cargo AI neither compiles nor executes that version's tools.

## Install local packages

Install the current project with its default profile, choose another profile, or install an existing package root, archive, or manifest:

```bash
cargo ai packages install
cargo ai packages install --profile release --as data_integration
cargo ai packages install ./dist/data_integration --as data_integration
cargo ai packages list
cargo ai packages inspect data_integration
```

Without `--account`, a bare package name is local-only and never triggers network lookup.

Local aliases are version-aware:

- identical version and content is normally a no-op;
- newer semver upgrades the same package identity;
- older semver requires `--downgrade`;
- same-version content replacement or a different identity requires `--replace`.

Reinstalling identical content repairs missing, corrupt, or wrong-target disposable runtime state while preserving package data.

A local-source install builds declared tools with `cargo build --locked --release` for the current target. Explicit approved hosted tool materialization uses the same compile profile. Cargo-native profile settings remain authoritative; existing installed artifacts are not silently rebuilt when Cargo AI changes. Compilation and validation are transactional: the verified source payload remains unchanged, only managed executable state enters `runtime/`, and failure leaves the previous alias and `data/` recoverable. The machine needs a compatible Rust toolchain and access to dependencies not already cached.

## Publish and pull hosted packages

Account-backed package commands use a separate surface from account agents:

```bash
# Hosted listings
cargo ai packages list --account
cargo ai packages list --account alice
cargo ai packages list --account --include-archived

# Package the current project, then publish its archive
cargo ai packages publish

# Restore a hosted package as a project without installing it
cargo ai packages pull data_integration
cargo ai packages pull data_integration --owner-handle alice
cargo ai packages pull data_integration --owner-handle alice --version 1.2.3
```

Publishing uses `[project].name` and `[project].version`; versions for one hosted package identity must increase by semver. Publish a higher version rather than changing hosted content at the same version.

`pull` defaults to the latest published version and restores a project-shaped directory. `.cargo-ai/project.toml` remains the operative project manifest. The immutable package manifest and hosted receipt are retained as provenance under:

```text
.cargo-ai/origin/cargo-ai-package.toml
.cargo-ai/origin/cargo-ai-package-receipt.toml
```

Pulled tools remain source-backed. Materialize one with `cargo ai tools build <tool-name>`, or create the complete runnable root with `cargo ai build`.

Visibility and archive state are explicit management operations:

```bash
cargo ai packages visibility --name data_integration --public
cargo ai packages visibility --name data_integration --private
cargo ai packages archive --name data_integration --archive
cargo ai packages archive --name data_integration --unarchive
```

Archived packages are omitted from the normal hosted listing. `list --account --include-archived` includes them; archive state does not create a different local package identity.

## Install and manage hosted versions

Install from your account or another public owner, optionally pinning an exact version:

```bash
cargo ai packages install data_integration --account --as data_integration
cargo ai packages install data_integration --account alice --as data_integration
cargo ai packages install data_integration --account alice --version 1.2.3 --as data_integration
```

If the verified manifest requests subprocess execution, accept it only after review:

```bash
cargo ai packages install data_integration --account alice \
  --as data_integration \
  --accept-permissions
```

Omitting `--version` resolves the latest eligible version at install time and pins that exact version in `install.toml`. Move an installed alias deliberately:

```bash
cargo ai packages update data_integration
cargo ai packages rollback data_integration --to 1.1.0
```

`update` moves forward only when a newer eligible semver exists. `rollback` selects exactly `--to`; it never means latest. Repeating either operation at the current requested version can repair disposable runtime state.

Hosted source identity is opaque and owned by the service, not derived from a URL or handle. Install, update, and rollback verify the returned identity. They resolve through the stored source id, so changing the publisher's handle does not strand an installed alias. Explicit handle installs and pulls verify normalized owner provenance.

Install, update, rollback, and uninstall hold an operating-system lock from the first alias read until mutation completes. The process releases the lock automatically when it exits.

## Installed layout and transactional state

Each alias lives under [Cargo AI Home](./cargo-ai-home.md):

```text
$CARGO_AI_HOME/packages/<alias>/
  install.toml
  package/
  runtime/
    tools/
  data/
```

- `install.toml` is Cargo AI-owned identity, version, hash, provenance, and accepted-permission state;
- `package/` is the verified payload for the active exact version;
- `runtime/` is Cargo AI-owned, target-specific disposable materialization;
- `data/` is package-owned persistent local state.

Hosted update and rollback transactionally rematerialize `install.toml`, `package/`, and `runtime/`, then preserve `data/`. Publisher-authored migrations and total-refresh replacement are not supported. Installed tool lookup is confined to the version-bound `runtime/` and never falls back to project or machine tools.

Replacing an alias with a different hosted source resets permission acceptance and never silently transfers state. Choose deliberately:

```bash
# Let the new publisher read the old package data only after explicit review.
cargo ai packages install <new-source> --account --as data_integration \
  --replace --keep-data

# Replace the source and start with empty package data.
cargo ai packages install <new-source> --account --as data_integration \
  --replace --delete-data
```

## Runtime filesystem boundaries

Installed hosted packages operate within explicit roots:

- verified publisher content and definition-owned image/file inputs resolve inside `package/`;
- generated relative child inputs and Cargo AI-controlled child `usage_log` writes resolve under `data/`;
- only an explicit runtime named-input override grants access to a caller-selected file path;
- Cargo AI-controlled writes reject traversal out of `data/`;
- direct child executables resolve only from verified `package/` and require accepted subprocess permission;
- installed JSON child agents must be declared package exports;
- nested agents cannot reinterpret text or generated values as arbitrary external filesystem paths.

Hosted archives are rejected before extraction when they exceed 10 MiB compressed, 100 MiB expanded, 10,000 entries, or 1,024 bytes in a normalized relative entry path. Absolute paths, parent traversal, drive-relative paths, UNC paths, device roots, symbolic links, and Windows reparse points are rejected.

## Run, hatch, inspect, and uninstall

Package entrypoints use `alias::entrypoint`:

```bash
cargo ai run data_integration::lookup_account
cargo ai hatch data_integration::daily_digest --allow-hosted-code
cargo ai packages inspect data_integration
```

Hatching exports executable code beyond the installed permission boundary, so a hosted alias requires the explicit `--allow-hosted-code` acknowledgement. `inspect` reports the opaque hosted source/version ids, optional owner handle, package hash, exports, and accepted permissions.

Uninstall is fail-safe around persistent data:

```bash
cargo ai packages uninstall data_integration
cargo ai packages uninstall data_integration --delete-data
```

If `data/` is nonempty, the first command refuses to proceed. Back up or export needed state before using `--delete-data`, which permanently removes the package payload, runtime, metadata, and data directory.

## Cross-package bindings

Bind each hosted alias used by a project to one source identity and semver range:

```toml
[package_dependencies.data_integration]
hosted_source_id = "<opaque hosted source id>"
version = "^1.2"
```

Then reference an exported entrypoint:

```json
{
  "kind": "agent",
  "artifact": "data_integration::lookup_account"
}
```

Before top-level run or hatch and before child resolution, Cargo AI verifies that the alias is an installed hosted package with the declared source id and a version satisfying the requirement. A hosted declaration never binds a local-source alias. Undeclared local aliases remain available for project development, and package assembly preserves hosted bindings.

A hosted package needs accepted subprocess permission to invoke a cross-package child. A hatched binary fails closed for package-child references unless launched inside a Cargo AI project. It also needs `cargo ai` or `cargo-ai` on `PATH`; that subprocess enforces the complete installed source, version, and permission policy.

Package internals remain private unless exported as entrypoints. Do not introduce unqualified global lookup by bare agent or tool name.

---

[Documentation hub](./README.md) · [Cargo AI README](../README.md)
