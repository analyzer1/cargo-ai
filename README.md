# cargo-ai™

[![Security Audit](https://github.com/cargo-ai/cargo-ai/actions/workflows/security-audit.yml/badge.svg)](https://github.com/cargo-ai/cargo-ai/actions/workflows/security-audit.yml)
[![Core CI](https://github.com/cargo-ai/cargo-ai/actions/workflows/multi-os-ci.yml/badge.svg)](https://github.com/cargo-ai/cargo-ai/actions/workflows/multi-os-ci.yml)
[![Product Qualification](https://github.com/cargo-ai/cargo-ai/actions/workflows/release-qualification.yml/badge.svg)](https://github.com/cargo-ai/cargo-ai/actions/workflows/release-qualification.yml)
[![Status: Stable – Ongoing Development](https://img.shields.io/badge/Status-Stable_–_Ongoing_Development-blue)](https://github.com/cargo-ai/cargo-ai)

Build declarative AI agents. Ship them as local CLI apps.

Cargo AI is an open-source Rust harness builder for auditable AI workflows. Define inputs, structured output, actions, and tool connections in readable JSON; run the definition directly; or hatch it into a native executable you can inspect and keep.

```bash
cargo ai run --config ./agent.json
cargo ai hatch my-agent --config ./agent.json
```

## Why Cargo AI

- **Readable by design:** one JSON definition makes inputs, output, and side effects reviewable and diffable.
- **Run or hatch:** iterate through the Cargo AI runtime, then export a native CLI executable from the same definition.
- **Real workflow building blocks:** use text, URLs, images, files, conditions, local commands, tools, email, image generation, and child agents where supported.
- **Provider choice:** connect to OpenAI, Anthropic, Gemini, xAI, Mistral, or a local Ollama server without changing the agent format.
- **Project and package workflows:** assemble agents, Rust tools, and assets into inspectable local or hosted packages with explicit permission boundaries.
- **Portable and auditable:** target macOS, Linux, and Windows while keeping generated source and shipped behavior visible.

## Quick Start

This path creates one local agent, runs it, validates it, and hatches it. A Cargo AI account is not required.

### 1. Install

Install Rust and Cargo using the official [Rust installation guide](https://rust-lang.org/tools/install/), then install Cargo AI:

```bash
cargo install cargo-ai --locked
cargo ai --help
```

See [Install Cargo AI](./docs/install/README.md) for platform and `PATH` details.

### 2. Configure a Model

If your ChatGPT plan includes Codex, you can reuse that subscription-backed sign-in without creating an API key. ChatGPT Plus currently includes Codex in the CLI; access and limits can vary by plan and workspace. Install [Codex CLI](https://learn.chatgpt.com/docs/codex/cli) first. See [OpenAI authentication](https://learn.chatgpt.com/docs/auth) and [Codex pricing and plan access](https://learn.chatgpt.com/docs/pricing). Verified: 2026-08-30.

```bash
cargo ai profile add openai-account \
  --server openai \
  --model gpt-5.6-terra \
  --auth openai_account

cargo ai auth login openai --profile openai-account --set-default
```

The Cargo AI login command starts Codex's browser sign-in, verifies the resulting local session, and associates it with the profile.

If that model is not available to your plan, choose another current model exposed by your Codex account. Direct OpenAI API keys and every alternative provider are documented under [Model providers](./docs/providers/README.md).

### 3. Create `agent.json`

```json
{
  "agent_definition_schema_version": "2026-09-09.r1",
  "inputs": [
    {
      "type": "text",
      "text": "What is 2 + 2? Return the answer as an integer."
    }
  ],
  "agent_schema": {
    "type": "object",
    "properties": {
      "answer": {
        "type": "integer",
        "description": "The result of the math problem."
      }
    }
  },
  "actions": [
    {
      "name": "show_answer",
      "logic": { "==": [{ "var": "answer" }, 4] },
      "run": [
        {
          "platform": ["macos", "linux"],
          "kind": "exec",
          "program": "printf",
          "args": ["The answer is 4.\\n"]
        },
        {
          "platform": "windows",
          "kind": "exec",
          "program": "cmd",
          "args": ["/C", "echo", "The answer is 4."]
        }
      ]
    }
  ]
}
```

`agent_definition_schema_version` identifies the Cargo AI definition contract, not the agent, package, or product version. Copy it from a current Cargo AI template or guidance bundle rather than inventing one.

### 4. Run It

```bash
cargo ai run --config ./agent.json --profile openai-account
```

Cargo AI resolves the inputs, requests the declared structured result, validates it locally, and runs matching actions only after validation succeeds.

### 5. Validate and Hatch It

```bash
cargo ai hatch first-agent --config ./agent.json --check
cargo ai hatch first-agent --config ./agent.json
./first-agent
```

On Windows, run `.\first-agent.exe` in PowerShell or `first-agent.exe` in Command Prompt. The longer [getting-started guide](./docs/getting-started.md) explains the same workflow and its next steps.

## Core Model

A Cargo AI definition has four main parts:

1. `inputs` — optional ordered text, URL, image, or file content for the model.
2. `runtime_vars` — optional typed values supplied by the caller to control logic or selected step settings without editing JSON.
3. `agent_schema` — the structured result Cargo AI requests and validates.
4. `actions` — conditional follow-up work composed from ordered `run` steps.

Runtime flags can replace, append, or prepend model inputs. Named inputs and runtime variables make reusable workflows explicit. Actions can run sequentially or in parallel while each action keeps its own steps ordered. Child-agent inputs and runtime values are forwarded deliberately; parent and child output objects are not silently merged.

Read [Agent definitions](./docs/agent-definitions.md) and [Actions and child agents](./docs/actions-and-child-agents.md) for the human guides. The version-matched offline contract installed for coding assistants remains under `templates/guidance/`.

## Capabilities

| Area | Current surface |
| --- | --- |
| Inputs | Ordered text, URL, image, and file inputs; named bindings and runtime overrides |
| Structured output | Typed JSON schema with scalar fields and a bounded structured-data lane for tools |
| Actions | Local commands, Cargo AI tools, child agents, email, and image generation |
| Control flow | JSON Logic, per-step conditions, failure policies, platform selectors, and sequential/parallel action scheduling |
| Runtime | Direct interpreted execution or generated native executables |
| Projects | Explicit build profiles, project-local Rust tools, assets, and runtime defaults |
| Packages | Local and hosted install, version management, exported entrypoints, permissions, and persistent data |
| Observability | Deterministic terminal status plus opt-in metadata-only usage ledgers |

Provider capabilities differ. Unsupported input or action combinations fail explicitly instead of silently dropping data or falling back to another provider.

## Model Providers

| Provider | Connection |
| --- | --- |
| [OpenAI](./docs/providers/openai.md) | ChatGPT/Codex account session or direct API key |
| [Anthropic](./docs/providers/anthropic.md) | Native Messages API with an Anthropic API key |
| [Google Gemini](./docs/providers/gemini.md) | Native Interactions API with a Gemini API key |
| [xAI](./docs/providers/xai.md) | Responses API with an xAI API key |
| [Mistral](./docs/providers/mistral.md) | Chat Completions API with a Mistral API key |
| [Ollama](./docs/providers/ollama.md) | Locally operated OpenAI-compatible server |

The [provider overview](./docs/providers/README.md) compares the current input and image-generation boundaries. Model availability belongs to each provider or account; Cargo AI does not maintain a model allowlist or certify every model/schema combination.

## Build Beyond the First Agent

Bootstrap a project and install version-matched AI authoring guidance:

```bash
cargo ai new my-agent-project
cd my-agent-project
cargo ai add guidance --style codex
```

Use `--style claude` for Claude Code, or repeat `--style` to install both discovery entrypoints. The installed `.cargo-ai/guidance/` bundle is self-contained and is the authoritative offline authoring contract for that Cargo AI version.

Use `cargo ai guidance status` for a read-only ownership/version check and `cargo ai guidance update` for an explicit offline update from the installed binary. User-owned instructions are preserved. See [guided setup](./docs/getting-started.md#author-with-an-ai-coding-assistant) and [guidance maintenance](./docs/projects-and-tools.md#maintain-assistant-guidance).

Projects can add local Rust tools, explicit build profiles, package metadata, assets, and runtime defaults. Start with [Projects and local tools](./docs/projects-and-tools.md), then use [Packages](./docs/packages.md) when the workflow should be installed, versioned, or shared.

## Accounts and Sharing Are Optional

Local run, hatch, build, and package workflows do not require a Cargo AI account. Register only when you want account-backed definition storage, public handles, email workflows, or hosted package publishing.

See [Accounts and sharing](./docs/accounts-and-sharing.md). Account-agent management stays under `cargo ai agents`; account-backed execution and hatching use the top-level `cargo ai run` and `cargo ai hatch` commands.

## Documentation

- [Documentation home](./docs/README.md)
- [Getting started](./docs/getting-started.md)
- [Install](./docs/install/README.md) and [Cargo AI Home](./docs/cargo-ai-home.md)
- [Model providers](./docs/providers/README.md)
- [Agent definitions](./docs/agent-definitions.md)
- [Actions and child agents](./docs/actions-and-child-agents.md)
- [Projects and local tools](./docs/projects-and-tools.md)
- [Packages](./docs/packages.md)
- [Accounts and sharing](./docs/accounts-and-sharing.md)
- [Troubleshooting](./docs/troubleshooting.md)
- [Testing and Product Qualification](./docs/testing-and-release-qualification.md)
- [Versioning](./VERSIONING.md) and [release notes](./releases/)

Runnable repository examples include [adder_test.json](./adder_test.json) and [weather_test.json](./weather_test.json).

## Project Status

Cargo AI is stable for ongoing use and remains under active development. The Security Audit, Core CI, and Product Qualification badges at the top of this page show the current public automation state. Product Qualification combines credential-free multi-platform behavior, package lifecycle checks, enrolled hosted-provider checks, and one fail-closed GitHub-native summary; see [Testing and Product Qualification](./docs/testing-and-release-qualification.md).

Scheduling is not built into Cargo AI today. Use an operating-system scheduler such as `cron` or Windows Task Scheduler when a local agent must run on a schedule.

## License

MIT. See [LICENSE](./LICENSE).
