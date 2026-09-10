# Testing and product qualification

[Documentation hub](./README.md) · [Cargo AI README](../README.md)

Cargo AI separates fast product confidence from paid live integration and from independently maintained package suites. This keeps ordinary pull requests deterministic while still producing a bounded release signal.

## Qualification areas

1. **Core CI** (`multi-os-ci.yml`) runs credential-free product, provider, maintained-content, package-lifecycle, build, and install checks on Ubuntu, macOS, and Windows. Provider requests use loopback fixtures. Ollama coverage tests its OpenAI-compatible transport without provisioning a model server.
2. `package-qualification.yml` checks one allowlisted public package revision on each declared platform. It runs the package's bounded declaration checks and the mandatory Cargo AI build/package/install/inspect/run/hatch/uninstall lifecycle.
3. `live-provider-conformance.yml` runs one representative model for the selected OpenAI, Anthropic, or explicitly enrolled supplemental provider. Manual dispatch selects one provider or `all` and defaults to OpenAI. Each selected job directly targets the protected `live-provider-ci` Environment, receives only its own key, and runs independently without provider-to-provider dependencies.
4. **Product Qualification** (`release-qualification.yml`) reuses Core CI and combines the credential-free families with fresh direct `live-provider-ci` provider jobs, renders a GitHub-native qualification dashboard, and fails unless every required result passes. It invokes the same Rust provider tests without reusing an earlier focused result or passing Environment secrets through a reusable workflow.

Full qualification uses nine required runner jobs: three deterministic operating systems, three canary-package operating systems, OpenAI, Anthropic, and one protected summary. Each supplemental provider enrollment adds one independent hosted job, up to 12 jobs before official packages and the unchanged 21-job global ceiling. Provider fixtures, models, package entrypoints, and package checks are not matrix dimensions.

## How one product qualification run fits together

`Product Qualification` is the GitHub Actions orchestration workflow. It reuses Core CI, starts the source-package family and fresh protected provider jobs against one exact Cargo AI commit, then reduces their sanitized results to one qualification decision:

```text
exact Cargo AI candidate commit
  +-- deterministic family
  |     +-- Ubuntu
  |     +-- macOS
  |     `-- Windows
  +-- source-package family
  |     +-- Ubuntu
  |     +-- macOS
  |     `-- Windows
  +-- live providers
  |     +-- OpenAI (required)
  |     +-- Anthropic (required)
  |     `-- enrolled supplemental providers (independent jobs)
  `-- product qualification summary
        `-- one fail-closed pass/fail decision
```

The operating-system families prove portable CLI and package behavior without provider credentials. Hosted-provider jobs are separate because the provider protocol boundary is not multiplied across operating systems. Package repositories retain their broad native tests; this workflow checks Cargo AI's bounded compatibility lifecycle for allowlisted exact package revisions.

## Local credential-free checks

Run these commands from the root of a public Cargo AI checkout. Keep local state away from your normal Cargo AI Home, disable keychain access, and serialize environment-mutating tests. On macOS or Linux, establish a disposable test environment first; use equivalent temporary environment settings on Windows:

```bash
export CARGO_AI_HOME="$(mktemp -d)/cargo-ai-home"
export CARGO_AI_DISABLE_KEYCHAIN=1
export RUST_TEST_THREADS=1
```

```bash
cargo test --locked --test qualification_policy
cargo test --locked --test product_conformance
cargo test --locked --test provider_smoke
cargo test --locked --test content_package_qualification
```

No command above needs a provider key or Cargo AI account credential, and each process test uses a temporary `CARGO_AI_HOME`. Qualification rules, report validation, summary rendering, package-catalog resolution and cache hashing run in Rust; this path requires no Python interpreter. The `qualification_policy` target builds and exercises the real maintainer-only `qualification-gate` Cargo example with synthetic inputs. The example is not an installed product command. CI retains YAML and bounded shell for checkout, job retrieval and routing.

The example routes `probe <provider>`, `aggregate` and `catalog` using the workflow’s explicit inputs. Only protected live jobs invoke probe mode. Aggregate mode requires candidate/run/job identity, matching dependency outputs and the candidate catalog before writing a passing summary. Catalog mode validates an allowlisted exact revision and a portable package-relative declaration before package checkout. Helper/build/output failures remain failures.

## Maintainer provider testing

This section is for Cargo AI maintainers validating provider adapters. It is not part of end-user provider setup. Keep deterministic, generated, and live testing separate so a credential-free contract result is never confused with paid integration evidence.

### Deterministic interpreted adapters

The ordinary provider suite uses loopback fixtures, fake credentials, and a temporary Cargo AI Home. It covers interpreted success for OpenAI, Anthropic, Gemini, xAI, Mistral, and the Ollama-compatible transport, plus focused failure and capability boundaries where applicable:

```bash
cargo test --locked --test provider_smoke
```

To isolate one interpreted adapter, use its exact test name:

```bash
cargo test --locked --test provider_smoke interpreted_openai_smoke_isolated_and_deterministic -- --exact
cargo test --locked --test provider_smoke interpreted_anthropic_smoke_isolated_and_deterministic -- --exact
cargo test --locked --test provider_smoke interpreted_gemini_smoke_isolated_and_deterministic -- --exact
cargo test --locked --test provider_smoke interpreted_xai_smoke_isolated_and_deterministic -- --exact
cargo test --locked --test provider_smoke interpreted_mistral_smoke_isolated_and_deterministic -- --exact
cargo test --locked --test provider_smoke interpreted_ollama_smoke_isolated_and_deterministic -- --exact
```

### Deterministic generated adapters

Generated-provider parity cases hatch and run complete standalone executables against the same loopback assertions. They are ignored by the ordinary Rust invocation because they compile full binaries. **Core CI** runs one sequential batch containing all six original case bodies on Ubuntu, macOS, and Windows:

```bash
cargo test --locked --test provider_smoke generated_provider_batch_isolated_and_deterministic -- --ignored --exact --nocapture
```

The batch prepares a neutral release seed, then copies its compatible template/compiler cache into each fresh case home. It preserves timestamps and executable permissions, verifies immutable seed SHA-256 digests in Rust, and transfers no credentials or provider-case state. Every generated application is still assembled and executed. The six individually selectable cases remain available for diagnosis:

```bash
cargo test --locked --test provider_smoke generated_openai_smoke_isolated_and_deterministic -- --ignored --exact
cargo test --locked --test provider_smoke generated_anthropic_smoke_isolated_and_deterministic -- --ignored --exact
cargo test --locked --test provider_smoke generated_gemini_smoke_isolated_and_deterministic -- --ignored --exact
cargo test --locked --test provider_smoke generated_xai_smoke_isolated_and_deterministic -- --ignored --exact
cargo test --locked --test provider_smoke generated_mistral_smoke_isolated_and_deterministic -- --ignored --exact
cargo test --locked --test provider_smoke generated_ollama_smoke_isolated_and_deterministic -- --ignored --exact
```

These checks prove interpreted/generated parity for representative fixtures. They do not contact a provider, validate an account, or certify every model.

### Explicit live provider checkpoints

Live cases are separately ignored and must be intentional. Load the matching key and model into the shell environment through an approved secret mechanism, then run only the selected test:

| Provider | Required environment | Exact test |
| --- | --- | --- |
| OpenAI | `OPENAI_API_KEY`, `OPENAI_MODEL` | `live_openai_smoke_uses_isolated_stdin_credentials` |
| Anthropic | `ANTHROPIC_API_KEY`, `ANTHROPIC_MODEL` | `live_anthropic_smoke_uses_isolated_stdin_credentials` |
| Gemini | `GEMINI_API_KEY`, `GEMINI_MODEL` | `live_gemini_smoke_uses_isolated_stdin_credentials` |
| xAI | `XAI_API_KEY`, `XAI_MODEL` | `live_xai_smoke_uses_isolated_stdin_credentials` |
| Mistral | `MISTRAL_API_KEY`, `MISTRAL_MODEL` | `live_mistral_smoke_uses_isolated_stdin_credentials` |

After the selected provider's variables are already present, run its exact checkpoint:

```bash
cargo test --locked --test provider_smoke live_openai_smoke_uses_isolated_stdin_credentials -- --ignored --exact
cargo test --locked --test provider_smoke live_anthropic_smoke_uses_isolated_stdin_credentials -- --ignored --exact
cargo test --locked --test provider_smoke live_gemini_smoke_uses_isolated_stdin_credentials -- --ignored --exact
cargo test --locked --test provider_smoke live_xai_smoke_uses_isolated_stdin_credentials -- --ignored --exact
cargo test --locked --test provider_smoke live_mistral_smoke_uses_isolated_stdin_credentials -- --ignored --exact
```

Each live test writes its key into a temporary isolated profile through stdin. The key is not a process argument. Never place real credentials in source, agent JSON, command arguments, logs, artifacts, or a normal Cargo AI Home. Use an operator-selected model; a live pass is representative provider-wiring evidence, not a provider model catalog or per-model certification.

Ollama has no live-provider qualification job. The credential-free adapter checks its compatible transport; provisioning and managing a real local model server is outside the current qualification workflow.

## Public source-package qualification

Package repositories own their full unit, integration, and domain-specific suites. Cargo AI does not copy those tests into this repository. Instead, an enrolled repository exposes a small `cargo-ai-qualification.toml` data declaration with:

- one stable package id and build profile;
- one to three runner platforms;
- at most two representative run/hatch entrypoints;
- at most five structured checks with a program, argument array, relative working directory, and timeout.

Shell command strings, path traversal, embedded credentials, and secret requests are rejected. The central catalog distinguishes qualification canaries from official packages. A passing canary proves the harness; it never counts as an official package.

The public `cargo-ai/cargo-ai-qualification-canary` is the minimal real cross-repository fixture. The reusable workflow checks out both Cargo AI and package revisions by immutable commit, removes checkout credentials before package-controlled code runs, and emits JUnit plus sanitized provenance. Package build scripts and procedural macros still execute as code on a disposable runner, so only reviewed catalog entries are eligible.

The installed interpreted run proves the canary's structured Rust tool through the version-bound package runtime. The installed hatch compile check uses `--ignore-tools`: current hatch auditing resolves project source tools, while installed runtime tools are checked and executed by the interpreted package path. This is a stated coverage boundary, not a claim that hatch independently re-audits the installed runtime tool.

To run the external canary lifecycle locally after checking out both repositories:

```bash
CARGO_AI_QUALIFICATION_PACKAGE_ROOT=../cargo-ai-qualification-canary \
  cargo test --locked --test content_package_qualification \
  external_package_qualification_runs_mandatory_lifecycle -- --ignored --exact
```

## Exact revisions and failure attribution

The reusable workflow implementation commit, tested Cargo AI candidate commit, and package commit are separate provenance values. Callers pin the reusable workflow to a reviewed full commit and pass exact lowercase 40-character candidate/package commits.

Candidate failure always blocks. When an exact last product-qualified Cargo AI commit is supplied, only a failing package/OS cell runs one fresh inline baseline attempt. Candidate failure plus baseline success indicates a probable Cargo AI regression; both failing indicates a package or infrastructure suspect; no usable baseline remains unclassified. The diagnostic result never converts candidate failure into success.

## Hosted provider configuration

Commission qualification progressively against one exact Cargo AI commit. Run **Core CI**, then **Package Qualification**, then **Live Provider Conformance** with its default `openai` choice. OpenAI and Anthropic are required for Product Qualification. Validate each primary provider, then enroll supplemental Gemini, xAI or Mistral when its coverage is wanted. A single-provider run starts only the selected provider job and is integration evidence, not a Product Qualification decision. **Product Qualification** starts new direct jobs for required OpenAI and Anthropic plus every explicitly enrolled supplemental provider and remains the only complete aggregate gate.

The live workflow has no semantic dependency between providers:

```text
workflow dispatch
  +-- OpenAI (required)
  +-- Anthropic (selected directly or by all)
  +-- Gemini (when enrolled)
  +-- xAI (when enrolled)
  +-- Mistral (when enrolled)
  `-- complete after every selected job finishes
```

Store these non-secret repository variables under **Settings → Secrets and variables → Actions → Variables**. Set a value to the exact lowercase string `true` to enroll that supplemental provider; leave it unset or set it to `false` to leave it unconfigured:

- `LIVE_GEMINI_ENABLED`
- `LIVE_XAI_ENABLED`
- `LIVE_MISTRAL_ENABLED`

The workflow never probes secret presence to infer enrollment. An invalid enrollment value fails visibly. Explicitly dispatching an optional provider also requires its enrollment variable to equal `true`.

Create a GitHub Environment named `live-provider-ci`. Store the required primary configuration there:

- `OPENAI_API_KEY`
- `OPENAI_MODEL` as a non-secret Environment variable
- `ANTHROPIC_API_KEY`
- `ANTHROPIC_MODEL` as a non-secret Environment variable

For each supplemental provider being enrolled, add only its matching Environment secret and non-secret model variable: `GEMINI_API_KEY`/`GEMINI_MODEL`, `XAI_API_KEY`/`XAI_MODEL`, or `MISTRAL_API_KEY`/`MISTRAL_MODEL`. Select one representative hosted model per enrolled provider. Do not add an Ollama secret or model variable; real local-server provisioning is outside this workflow.

Restrict `live-provider-ci` to the trusted default branch and approved release tags. Before any provider key is injected, each focused or aggregate live job directly targets that Environment, requires an exact lowercase Cargo AI commit, and verifies that it is the trusted triggering commit or one of its ancestors. It is intended to run unattended, so human release approval belongs to a separate `release-qualification` Environment attached only to the final aggregate summary.

The live tests write each selected key to a temporary isolated profile through stdin. Keep every key only as a `live-provider-ci` Environment secret; do not duplicate it into `release-qualification`, repository/organization secrets, workflow YAML, or inherited secret sets. Keys are not command arguments, logs, artifacts, caches, deterministic jobs, source-package jobs, other provider jobs, or the final summary. Missing OpenAI or Anthropic configuration and missing configuration for an explicitly selected or enrolled provider fail rather than silently skipping. Unenrolled optional providers are intentionally reported as not configured and do not block qualification.

## Evidence and release interpretation

Required checks should include the stable deterministic summary on ordinary pull requests and the protected release summary before promotion. Package evidence records Cargo AI commit, optional baseline commit, package repository/commit, logical OS, runner image/version, architecture, declaration digest, workflow ref, classification, and result. It must never contain prompts, model output, tokens, raw provider bodies, Cargo AI Home state, or package runtime data.

The protected aggregate job writes the canonical human-readable dashboard directly to the GitHub Actions run summary. Open the Cargo AI repository, select **Actions**, select **Product Qualification**, and open a run's **Summary** page. When required proof passes, the headline says **Product Qualification: Passed**. The front table shows required product, package and primary-provider evidence; supplemental providers appear inside **Supplemental provider details**. Rows link to their producing jobs.

OpenAI and Anthropic must pass. An enrolled Gemini, Mistral or xAI probe may instead report **not verified — rate limited** when its completed isolated usage record positively classifies HTTP 429 as `ratelimited`. That qualified warning leaves its job, aggregate and overall workflow successful when every other obligation passes; the unsuccessful service result remains in collapsed details. Auth/configuration errors, malformed responses, unknown errors, timeouts, arbitrary 5xx responses, harness failures and missing/stale/cancelled evidence still block. Unenrolled supplemental providers are `not configured`, never passed. The legacy `LIVE_ANTHROPIC_ENABLED` flag cannot disable required Anthropic proof.

Qualification uses an explicit report mode and sanitized candidate/provider/run/attempt/probe-bound records. Standalone Live Provider Conformance and direct live tests remain strict: unsuccessful service calls fail. The policy does not substitute providers, retry automatically or grant a pass to unavailable evidence. Earlier failed workflow runs retain their original conclusions.

Other dashboard states remain `pass`, `fail`, `cancelled`, `skipped`, and `missing`. The official-package row is `skipped` only while the validated catalog count is zero; enrolling an official package without aggregate results changes that row to `missing` and blocks release. JUnit and provenance artifacts remain the durable evidence behind the summary.

When GitHub reruns only failed jobs, the dashboard safely selects the newest completed attempt for each expected job from the same workflow run and exact candidate commit. A duplicate or mismatched result is treated as missing rather than guessed.

If a developer rejects the protected summary Environment or cancels the workflow before that job starts, GitHub's run status remains authoritative because no summary job ran to publish a dashboard.

This view is run-scoped and entirely GitHub-hosted. It requires no Cargo-AI.org page, GitHub Pages deployment, application server, database, custom API, or cross-system synchronization. The repository maintains only the workflow/reporting logic and normal GitHub Environment configuration.

The package catalog starts fail-closed until the public canary caller and its immutable commit are reviewed and pinned. Official-package count begins at zero. Adding packages increases only package shards; it does not multiply providers, models, or entrypoints.

---

[Documentation hub](./README.md) · [Cargo AI README](../README.md)
