# Build and Run Your First Cargo AI Agent

This guide takes you from installation to one local JSON agent that runs directly and hatches into a native CLI executable. A Cargo AI account is not required.

## 1. Install Cargo AI

Install Rust and Cargo first, then install Cargo AI:

```bash
cargo install cargo-ai --locked
cargo ai --help
```

See [Install Cargo AI](./install/README.md) for platform and `PATH` details.

## 2. Configure a Model

The recommended first path uses an existing ChatGPT subscription through a local Codex sign-in. Codex CLI supports signing in with ChatGPT for subscription access, and ChatGPT Plus includes Codex in the CLI. Install [Codex CLI](https://learn.chatgpt.com/docs/codex/cli) first. [OpenAI authentication](https://learn.chatgpt.com/docs/auth) and [Codex pricing and plan access](https://learn.chatgpt.com/docs/pricing) describe the current sign-in and plan boundaries. Verified: 2026-08-30.

Create a Cargo AI profile, then sign in:

```bash
cargo ai profile add openai-account \
  --server openai \
  --model gpt-5.6-terra \
  --auth openai_account

cargo ai auth login openai --profile openai-account --set-default
```

The Cargo AI login command starts Codex's browser sign-in, verifies the resulting local session, and associates it with the profile.

Model availability can vary by plan and workspace. If `gpt-5.6-terra` is unavailable to your account, select another current model exposed by your Codex plan. See [OpenAI setup](./providers/openai.md) or choose a different [model provider](./providers/README.md).

## 3. Create `agent.json`

Create a file named `agent.json` with this complete definition:

```json
{
  "agent_definition_schema_version": "2026-03-03.r1",
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

`agent_definition_schema_version` identifies the Cargo AI definition contract. Copy this value from a current Cargo AI template or guidance bundle; do not invent it from the product, project, or package version.

## 4. Run the JSON Directly

```bash
cargo ai run --config ./agent.json --profile openai-account
```

Cargo AI sends the declared inputs to the selected model, validates the structured response against `agent_schema`, and runs matching actions only after validation succeeds.

## 5. Validate and Hatch It

Validate the definition and generated project without exporting a binary:

```bash
cargo ai hatch first-agent --config ./agent.json --check
```

The check uses Cargo’s dev profile and exports no binary. Then hatch the native executable with Cargo’s release profile:

```bash
cargo ai hatch first-agent --config ./agent.json
./first-agent
```

On Windows, run `.\first-agent.exe` in PowerShell or `first-agent.exe` in Command Prompt.

The executable uses your configured default profile unless you pass runtime provider options explicitly. A standalone recipient does not need Cargo AI installed unless the agent depends on installed package entrypoints.

## Author With an AI Coding Assistant

For a larger project, bootstrap a project boundary and install the version-matched offline guidance bundle:

```bash
cargo ai new my-agent-project
cd my-agent-project
cargo ai add guidance --style codex
```

Use `--style claude` for Claude Code, or repeat `--style` to install both discovery entrypoints. Cargo AI preserves divergent user-owned root instruction files and fails closed when an existing managed bundle conflicts; it does not silently overwrite them.

Tell the assistant what the agent should do, its inputs and outputs, and whether it needs files, commands, email, tools, or child agents. Review the resulting JSON, then repeat `hatch --check` until validation passes.

## Next

- [Agent definitions](./agent-definitions.md)
- [Actions and child agents](./actions-and-child-agents.md)
- [Projects and local tools](./projects-and-tools.md)
- [Documentation home](./README.md)
- [Public README](../README.md)
