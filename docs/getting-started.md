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

`agent_definition_schema_version` identifies the Cargo AI definition contract. Copy this value from a current Cargo AI template or guidance bundle; do not invent it from the product, project, or package version.

`2026-09-09.r1` is the current strict revision. It rejects unknown definition fields and unsupported schema keywords. Valid earlier revisions retain their legacy parsing behavior; other revisions at or after this cutoff are unsupported. See [Author Agent Definitions](./agent-definitions.md) before migrating an existing definition.

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

Start with the official [Cargo AI website](https://cargo-ai.org) and follow its repository/documentation links. An assistant should read those instructions rather than infer a repository address or assume prior Cargo AI knowledge. The website links to [cargo-ai/cargo-ai](https://github.com/cargo-ai/cargo-ai). Verified: 2026-09-09.

Before changing anything, inspect the current directory, existing assistant instructions, operating system, and installed `cargo`, `rustc`, `git`, and `cargo ai --help`/`--version`. Use the installed command help and version-matched guidance for supported behavior. If a command below is unavailable in the installed release, explain the mismatch and propose an explicit upgrade; do not borrow an unreleased schema or silently reinstall tools. [Platform installation instructions](./install/README.md) cover the required toolchain.

For a larger project, bootstrap a project boundary and install the version-matched offline guidance bundle:

```bash
cargo ai new my-agent-project
cd my-agent-project
cargo ai add guidance --style codex
```

For an existing directory, review its files first and use `cargo ai init` there instead of `new`. Keep its files and assistant instructions; resolve reported conflicts with the owner. Use `--vcs none` when Git initialization is not wanted.

Use `--style claude` for Claude Code, or repeat `--style` to install both discovery entrypoints. Cargo AI preserves existing user-owned root instruction files and supplies a loader snippet for review. A manifest records ownership of generated files. Check that bundle without changing it:

```bash
cargo ai guidance status
```

After an explicitly approved Cargo AI binary upgrade, `cargo ai guidance update` updates unchanged managed files from the installed binary, offline. It preserves user instructions and blocks modified, incomplete, malformed or legacy unmanaged bundles. See [guidance maintenance](./projects-and-tools.md#maintain-assistant-guidance) for recovery and Git behavior.

Keep installation, sign-in, profile/credential access, permission grants and first-run side effects explicit. Select a supported provider/profile with the user; the authoring assistant and model provider are separate choices. Never put credentials in project source, guidance or packages. This workflow does not require a plugin or a desktop authoring application.

Tell the assistant what the agent should do, its inputs and outputs, and whether it needs files, commands, email, tools, or child agents. Review the resulting JSON, validate it with `hatch --check`, then approve and observe a first run with the selected profile. Record what ran, expected versus observed output, assistance or failures, and any remaining setup or unsupported capability. A structural check alone does not prove a successful model run. Success in one tested environment is not a universal one-prompt setup guarantee.

## Next

- [Agent definitions](./agent-definitions.md)
- [Actions and child agents](./actions-and-child-agents.md)
- [Projects and local tools](./projects-and-tools.md)
- [Documentation home](./README.md)
- [Public README](../README.md)
