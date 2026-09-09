# Cargo AI Agent Definition Contract

Use this file as the complete offline contract when you need to author or review a Cargo AI agent definition without looking at repository code.
That JSON definition is the source for a generated CLI executable: it defines the inputs, structured output, and follow-up actions the hatched tool will use.

The current strict contract is `2026-09-09.r1`. Unless a section says otherwise, the supported field lists below are exhaustive for that revision. Stable objects reject additional fields; user-named maps allow names whose values satisfy the stated contract.

## Required Top-Level Shape

Every agent definition must be a JSON object with these keys. Prefer this order for readability; JSON key order does not change validation:

1. `agent_definition_schema_version`
2. optional `inputs`
3. optional `action_execution`
4. optional `runtime_vars`
5. `agent_schema`
6. `actions`

## `agent_definition_schema_version`

- Required.
- Non-empty string.
- Format: `YYYY-MM-DD.rN`
- Current strict revision: `2026-09-09.r1`.
- This identifies the Cargo AI contract used to interpret this definition. It is not the agent or package version; package version is `[project].version`.
- Copy the schema version from the current Cargo AI template or guidance. Do not invent one from the current date, an agent/package/project version, or the Cargo AI product version.
- The legacy top-level `version` key is invalid. Rename that key without changing its schema-version value.
- Syntactically valid revisions chronologically before `2026-09-09.r1` keep their legacy parsing behavior, including permissive unknown fields. This includes `2026-03-03.r1`, `2026-03-11.r1`, and `2026-03-28.r1`; it is not a closed registry of old revisions.
- Exactly `2026-09-09.r1` selects strict validation. Every other revision at or after that cutoff is unsupported. Date and revision components are compared numerically.
- Existing definitions are not rewritten automatically. To migrate, review the complete definition against this contract and validate it with the strict revision. Do not replace an unsupported version header blindly; check the intended contract or upgrade Cargo AI.

## `inputs`

- Optional.
- If present, must be an array with at least one item.
- Each top-level input may also declare optional `name`.
- For readability, prefer named input object field order as `name`, then `type`, then the value-bearing field (`text`, `url`, or `path`).
- For unnamed literal inputs, keep `type` first and the value-bearing field second.
- Supported input shapes:
  - `text`
    - required: `type`
    - optional: `name`, `text`
  - `url`
    - required: `type`
    - optional: `name`, `url`
  - `image`
    - required: `type`
    - optional: `name`, `path`
  - `file`
    - required: `type`
    - optional: `name`, `path`

Top-level input rules:
- Unnamed top-level inputs must include a baked value (`text`, `url`, or `path`).
- Named top-level inputs may either include a baked value or act as a required slot with no baked value.
- Named top-level inputs are still normal model-facing inputs for schema-backed agents.
- Named top-level inputs may also be reused explicitly by child-agent steps and targeted by `--input-override`.
- Top-level named inputs are literal-or-empty-slot only in the current slice; do not use `runtime.*` or other dynamic expressions there.

Path rules for `image` and `file`:
- Use relative paths only.
- Do not use absolute paths.
- Do not use parent traversal such as `../`.

URL guidance:
- `{"type":"url","url":"..."}` means Cargo AI fetches the URL with its own HTTP client and passes the returned body as text input.
- Treat the practical compatibility target as comparable to `curl` for ordinary static or server-rendered content.
- Do not assume browser-only or JavaScript-rendered pages will work through `type: "url"`.

## Runtime Inputs

Generated agent binaries may also accept runtime input flags such as:
- `--input-mode`
- `--input-text`
- `--input-url`
- `--input-image`
- `--input-file`
- `--input-override NAME=VALUE`

These runtime inputs are separate from the JSON `inputs` array:
- JSON `inputs` are baked into the definition as default model-facing inputs.
- Runtime input flags are supplied by the caller when the binary runs.
- If runtime input flags are used without `--input-mode`, they replace the full JSON-defined `inputs` array for that run.
- `--input-mode replace` explicitly selects runtime-only replacement.
- `--input-mode append` keeps baked inputs first and appends runtime inputs in CLI order.
- `--input-mode prepend` keeps runtime inputs in CLI order first and then places baked inputs after them.
- `--input-override NAME=VALUE` targets one declared named top-level input and replaces that named binding for the current run.
- `--input-override` is repeatable; one binding per flag, split on the first `=`, and later duplicates win.
- `--input-override` type-checks `VALUE` against the declared named input kind:
  - `text`: raw string, including empty string
  - `url`: must be an absolute `http://` or `https://` URL
  - `image`: treated like runtime `--input-image`
  - `file`: treated like runtime `--input-file` and must use a supported file extension
- Anonymous runtime `--input-*` flags do not bind named input identities.
- For schema-backed agents, anonymous runtime `--input-*` flags still control the effective root model input list exactly as before.
- For structural action-only agents, anonymous runtime `--input-*` flags remain invalid; use named top-level inputs plus `--input-override` instead.

Authoring guidance:
- Use JSON `inputs` when the definition should own a fixed instruction or a fixed local file/image path.
- Prefer named top-level inputs when a value is part of the workflow contract, reusable by child-agent steps, or overrideable by name.
- Leave one-off root-model context unnamed when it does not need child reuse or targeted override behavior.
- Use runtime flags when the caller should choose the content at invocation time.
- If you use `--input-file` and still need a text instruction, either also pass `--input-text` in replace mode or choose `--input-mode append` / `--input-mode prepend` so the baked text instruction is still included.
- `{"type":"file","path":"..."}` is for a definition-owned fixed file path, not for a caller-selected runtime file.
- If `agent_schema.properties` is empty, runtime `--input-*` flags are invalid because Cargo AI skips the model call in that structural action-only shape.

## `runtime_vars`

- Optional.
- Object keyed by runtime variable name.
- Use when the caller should control action behavior or a step-local setting at invocation time without editing the JSON.

Each `runtime_vars.<name>` entry supports:
- required `type`
- optional `default`

Supported `type` values:
- `string`
- `number`
- `integer`
- `boolean`

Rules:
- runtime variable names are flat
- runtime variable names cannot contain `.`
- `runtime` and `runtime.*` are reserved
- `default`, when present, must match the declared type
- values are supplied at invocation time through repeatable `--run-var name=value` flags
- undeclared `--run-var` names fail
- duplicate `--run-var` names fail
- if a declared runtime var has no `default`, the caller must supply it when an executed path resolves it
- quote `--run-var` values normally in the caller's shell when they contain spaces or shell-sensitive characters

Example:

```json
{
  "runtime_vars": {
    "generate_images": { "type": "boolean", "default": false },
    "hero_image_model": { "type": "string", "default": "gpt-image-1.5" },
    "score_threshold": { "type": "number", "default": 0.8 }
  }
}
```

## `agent_schema`

- Required.
- Must be an object with:
  - `type: "object"`
  - `properties: { ... }`
- Each property must define a `type`.
- Top-level property names must also be valid generated Rust identifiers; prefer names such as `summary` or `needs_review` and avoid reserved words.
- Top-level property names are reserved for action-variable lookup. Step-captured variable names cannot reuse them.
- `properties` may be empty. That declares the structural action-only shape.

Supported top-level property `type` values:
- `string`
- `number`
- `integer`
- `boolean`
- `array`
- `object`

Supported optional top-level property metadata and constraints:
- `description` on any supported field
- `enum` on `string` fields only
- `minimum`, `maximum`, `exclusiveMinimum`, and `exclusiveMaximum` on `number` and `integer` fields

Exact descriptor keys are:

| Descriptor | Allowed keys |
|---|---|
| Root `agent_schema` | `type`, `properties` |
| String | `type`, optional `description`, optional `enum` |
| Number or integer | `type`, optional `description`, optional `minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum` |
| Boolean | `type`, optional `description` |
| Array | `type`, optional `description`, required `items` |
| Object | `type`, optional `description`, required `properties` |

The same descriptor rules apply to array items and structured object properties, subject to the nesting and nullable restrictions below. `properties` allows user-chosen names; its values are schema descriptors.

Every declared output property is required. Cargo AI generates provider-facing `required` and `additionalProperties: false` metadata itself. Do not put either keyword in an authored definition. Strict definitions also reject the type alias `num`, `$ref`, `$schema`, `format`, composition keywords such as `oneOf`/`anyOf`/`allOf`, and every other unsupported schema keyword. No JSON Schema format-assertion vocabulary or remote schema-reference loading is supported.

Constraint rules:
- `enum` values must be non-empty strings
- `enum` values are exact and case-sensitive
- lower bounds may use `minimum` or `exclusiveMinimum`, but not both
- upper bounds may use `maximum` or `exclusiveMaximum`, but not both
- `array` fields must be homogeneous
- `object` fields must declare their shape explicitly
- arrays may contain supported scalar item types or declared-shape object items
- object properties inside structured tool-bound fields may be scalar or `scalar | null`

Unsupported schema shapes for the current MVP:
- nested arrays
- deeper nested objects
- union types other than `scalar | null` on object properties inside structured tool-bound fields

Structured-value lane:
- top-level `array` / `object` fields exist for structured tool consumption
- nullable support is limited to `scalar | null` object properties inside those structured payloads
- structured top-level fields may flow only into tool params as raw JSON
- `logic`, `when`, `exec.args`, string-part interpolation, `email_me`, child `run_vars`, and other scalar-first substitution surfaces must reject structured field references

Structural action-only rule:
- If `agent_schema.properties` is empty, Cargo AI skips the initial model call and begins at the action layer.
- In that shape, top-level `inputs` may still exist, but they must declare `name` because they are reusable parent-owned inputs only.
- In that shape, runtime model-facing `--input-*` flags are invalid.
- In that shape, named top-level inputs may be baked literals or required slots satisfied later by parent pass-through or `--input-override`.
- Top-level action `logic` starts with declared `runtime.*` values only.
- Step `when` and substitution surfaces may use declared `runtime.*` values plus prior captured step variables as the action runs.
- References to top-level model-output fields are invalid in that shape because no initial model output exists.

## `actions`

- Required.
- Array; `actions: []` is valid for a model-only definition that needs no follow-up actions.
- Optional top-level `action_execution` may appear alongside `actions`.
  - Allowed values: `sequential`, `parallel`
  - Omitted means `sequential`
  - `parallel` only changes scheduling across matching top-level actions
  - each action's own `run` list remains sequential in both modes
  - a runtime safety/testing override may force the invocation tree down to sequential with `--action-execution sequential`
  - that runtime override is CLI-scoped and inherited by child-agent invocations; parent JSON does not override child JSON
- Each action must contain:
  - `name`
  - `logic`
  - `run`
- Each action's `run` array must contain at least one step.

`logic` uses JSON Logic against the top-level action data object. At action start, that means:
- top-level model output fields plus declared `runtime.*` values for schema-backed agents
- declared `runtime.*` values only for the structural action-only shape

Each `logic` or `when` expression object has exactly one supported operator:

`==`, `===`, `!=`, `!==`, `var`, `!`, `!!`, `if`, `or`, `and`, `<`, `<=`, `>`, `>=`, `missing`, `missing_some`, `min`, `max`, `+`, `-`, `*`, `/`, `%`, `in`, `cat`, `substr`, `log`, `merge`, `map`, `filter`, `reduce`, `all`, `some`, `none`.

Existing flat-variable, scalar-reference and comparison-type rules still apply; the operator list does not add scoped-variable lookup. The `literal` operator is unsupported in the strict revision. For a constant true gate, use `{ "==": [1, 1] }`. Strings, numbers, booleans and arrays used as expression data remain bounded data.

If `logic` evaluates true, the action's `run` steps execute in order.
- In `sequential`, matching top-level actions run one after another.
- In `parallel`, matching top-level actions may overlap, but each action still keeps its own `run` steps in order.
- A hard failure in one top-level action does not prevent later eligible top-level actions from running.
- Cargo AI aggregates top-level hard failures after all eligible actions finish.
- Cargo AI prints one run-level execution header before actions start: `Action execution: sequential` or `Action execution: parallel`.
- Cargo AI also prints one root `using:` line near run start that shows the effective `profile`, `auth`, `server`, and `model` for that invocation.
- It adds `url=...` only when the effective URL is custom or materially different from the standard transport.
- In redirected, piped, CI, or simpler terminal output, Cargo AI prefixes parent-visible action output with deterministic labels such as `[Action 1: generate_images]`.
- In append-only output, long-running steps also emit a step-start liveness line such as `step 2/2 generate_image started; waiting for provider response...`.
- Terminal lane summaries and the final run footer also include wall-clock durations, for example `completed in 31s.` and `✅ Run complete in 32s.`.
- When attached directly to an interactive terminal, Cargo AI switches to a compact live lane dashboard.
- The first live dashboard slice shows lane label, lane status with elapsed time while running and after terminal completion/failure, terminal step marker or current step when known, and the last lifecycle message.
- In append-only output, parent-run `exec` and `email_me` output is emitted with the originating action label.
- Child-agent steps stay minimal in the parent lane with start/completion or exit-summary lines instead of recursively inlining the child transcript.
- In append-only output, provider-backed or child-agent steps emit another action-prefixed `using:` line only when the effective `profile`, `auth`, `server`, or `model` changes from the most recently printed context.
- Interactive live mode keeps the parent dashboard at the orchestration level and does not surface child or step-level `using:` lines there.

## Supported Run-Step Kinds

These documented step kinds are exhaustive for the current MVP.

### `exec`

Required fields:
- `kind`
- `program`
- `args`

### `agent`

Required fields:
- `kind`
- exactly one of `artifact` or the legacy alias `agent`

Optional fields:
- `profile`
- `usage_log`
- `inputs`
- `input_mode`
- `input_overrides`
- `run_vars`
- `ignore_tools`

### `tool`

Required fields:
- `kind`
- `name`

Optional fields:
- `params`
- `output_variable`

Use `kind: "tool"` for Cargo AI-managed project-local tools created with `cargo ai add tool <name>`.

`params` is an object whose values may be JSON literals or variable references such as `{ "var": "runtime.symbol" }`. A tool's `describe.params` contract may declare `string`, `boolean`, `integer`, `number`, `array`, or `object`. Cargo AI validates literal params during check/build and validates resolved variable params at runtime against that contract.

Tool parameter names are an open map, subject to the tool's own declared contract. An object with exactly one `var` key is a reference. Other objects and arrays are literal JSON data, including their nested keys; they are bounded but are not interpreted as definition fields or policy. A standalone null parameter is unsupported; null may appear inside a literal array or object, subject to the tool's own application checks.

For structured tool params in this slice:
- Cargo AI validates only top-level kind compatibility (`array` vs `object`)
- deeper item/object shape validation remains the tool's responsibility
- structured values are passed to tools as raw JSON with no string coercion

`output_variable` is optional. The tool's `describe.result` schema must be a nullable string. An `invoke` response must contain exactly `protocol_version` and `result`; an omitted `result` is invalid. `result` is a string or null, and must be a non-null string when `output_variable` is set. Keep structured application results inside a serialized string if the tool needs to return them; this contract does not add arbitrary result schemas.

### `email_me`

Required fields:
- `kind`
- `subject`
- `text`

### `generate_image`

Required fields:
- `kind`
- `prompt`
- `path`
- Optional:
  - `model`
  - `profile`
  - `reference_images`
- First slice writes one local image file.
- If `model` is omitted, Cargo AI falls back to the effective invocation model resolved from the current profile and any `--model` CLI override.
- If `profile` is present, Cargo AI resolves that profile at step runtime and uses it for the image step's provider/url/token context.
- With `generate_image.profile`, explicit `model` still wins, then the step-profile model, then the parent invocation model.
- `generate_image.profile` may point to a different provider from the parent invocation; for example, a parent may stay on OpenAI while one image step switches to an Ollama profile.
- Anthropic profiles support text, URL-text, image input, and structured text output, but not direct file input or `generate_image` in the current release. An Anthropic parent may select an OpenAI or Ollama step profile for image generation.
- Gemini profiles use the native Interactions API with `store = false` and support text, URL-text, image input, and structured text output, but not direct file input or `generate_image` in the current release. A Gemini parent may select an OpenAI or Ollama step profile for image generation.
- If neither the step nor the invocation provides a model, the step fails clearly at runtime.
- `model` may be:
  - a literal non-empty string
  - a single variable reference such as `{ "var": "runtime.hero_image_model" }`
  - a single top-level string output field such as `{ "var": "image_model" }`
- `generate_image.model` may not read captured `output_variable`, `status_variable`, or `error_variable` values in the current contract.
- For Cargo AI's default OpenAI account transport, use a tool-capable mainline model such as `gpt-5.2`.
- For a direct OpenAI API token and URL, prefer GPT Image models such as `gpt-image-2`, `gpt-image-1.5`, or `gpt-image-1-mini`.
- `reference_images` may contain one or more `{ "input": "<named image input>" }` or `{ "path": "./assets/reference.png" }` entries.
- `reference_images[*].input` must point to a declared top-level input with `type: "image"`.
- `reference_images[*].path` must stay at the current level or below; absolute paths and `..` traversal are rejected.
- `reference_images` order is preserved. Put the primary source/edit-target image first, then supporting detail, style, color, or material references.
- When using more than one reference image, label the roles in the prompt, such as "Image 1 is the source photo; Images 2 and 3 are style references only."
- OpenAI API-key profiles send `reference_images` through the OpenAI image edit/reference-image path; OpenAI account profiles include them as Responses image input parts.
- For Ollama's experimental OpenAI-compatible `/v1/images/generations` endpoint, use an Ollama image model on an Ollama profile. The current compatibility slice uses Ollama's documented `b64_json` response path, so Ollama-backed `generate_image` steps currently require a `.png` output path and do not support `reference_images`.
- Current-at-ship-date note: official OpenAI docs list `gpt-image-2` for image generation and editing, including high-fidelity image inputs. Verified: 2026-05-22.

Named reference image definition fragment:

```json
{
  "inputs": [
    {
      "name": "source_photo",
      "type": "image",
      "path": "./assets/source-photo.jpg"
    }
  ],
  "actions": [
    {
      "name": "edit_source_photo",
      "logic": { "==": [1, 1] },
      "run": [
        {
          "kind": "generate_image",
          "profile": "openai_api_key",
          "model": "gpt-image-2",
          "prompt": "Image 1 is the source photo. Remove furniture from the deck while preserving the deck, fence, lighting, and camera angle.",
          "reference_images": [
            { "input": "source_photo" }
          ],
          "path": "./artifacts/deck-clean.png"
        }
      ]
    }
  ]
}
```

Mixed reference image example:

```json
"reference_images": [
  { "input": "source_photo" },
  { "path": "./assets/detail-reference.png" },
  { "path": "./assets/color-reference.webp" }
]
```

## Common Optional Step Fields

These fields are available on every step kind:
- `when`
- `failure_mode`
- `platform`
- `status_variable`
- `error_variable`

`exec` and `tool` also support:
- `output_variable`

Only the common controls and the fields listed for a step's own kind are allowed. For example, `output_variable` is invalid on `agent`, and `program` is invalid on `tool`. String-part references have exactly one key, `var`; neighboring keys are rejected rather than ignored.

### `platform`

- Optional.
- Allowed values:
  - `macos`
  - `linux`
  - `windows`
- May be a single string or an array of strings in the executable JSON contract.
- Limits the step to the named runtime platform(s).
- If omitted, the step may run on any supported runtime platform.
- If the current runtime platform does not match, the step is skipped and later matching steps continue in order.

Example:

```json
{
  "kind": "exec",
  "program": "/bin/echo",
  "platform": "macos",
  "args": ["hello"]
}
```

## Returned Output vs Actions

- The top-level `agent_schema` fields are the agent's returned structured output.
- Actions run only after the model's raw structured output passes the declared shape, type, enum, numeric-bound and resource checks. Missing properties and unknown properties, including unknown root fields, fail before actions.
- `exec`, `agent`, `tool`, `email_me`, and `generate_image` steps are follow-up side effects or orchestration.
- Action steps do not mutate the returned top-level output object.
- `output_variable`, `status_variable`, and `error_variable` are action-local only.

## Step Outcome Rules

- Steps stop the action by default when they fail.
- Use `failure_mode: "continue"` only when a later step should react to a failure.
- Use `failure_mode: "abort"` when the whole invocation should stop scheduling new work and fail with an explicit abort summary.
- A hard failure stops the rest of that action's `run` list, but it does not prevent later eligible top-level actions from running; Cargo AI aggregates top-level action failures after the full scan.
- In the first `abort` slice, already-running work settles cooperatively unless a safe cancellation path already exists.
- Child-agent abort stays local to the child invocation first; the parent lane then handles that failed child exit according to the parent step's own `failure_mode`.
- `status_variable` stores:
  - `succeeded`
  - `failed`
- `error_variable` stores a human-readable failure string when the step fails.
- If `when` evaluates false, the step is skipped and `status_variable` / `error_variable` stay unset in the MVP.
- If `platform` does not match the current runtime OS, the step is skipped and `status_variable` / `error_variable` stay unset in the MVP.

## Variable Namespace Rules

Within one action:
- `output_variable`, `status_variable`, and `error_variable` share one flat namespace.
- Dotted names are invalid.
- `runtime` and `runtime.*` are reserved for declared invocation-scoped runtime variables.
- Captured names cannot collide with top-level `agent_schema` property names.
- Captured names cannot be reused within the same action.

Across different top-level actions:
- The same captured names may be reused.

Top-level action `logic` can read:
- top-level model output fields
- declared `runtime.*` values

Step `when` and string/arg substitutions can read:
- top-level model output fields
- declared `runtime.*` values
- prior `output_variable` values from earlier steps in the same action
- prior `status_variable` values from earlier steps in the same action
- prior `error_variable` values from earlier steps in the same action

## Child-Agent Path Rules

For `kind: "agent"`:
- Use explicit same-level paths such as `./child_reporter`
- Do not use bare names
- Do not use absolute paths
- Do not use `../`

## Child-Agent Data Flow

- A parent action may pass child-agent `inputs`.
- A parent action may also pass child-agent `run_vars`.
- A parent action may also pass child-agent `input_overrides`.
- If the target is another Cargo AI agent, prefer a native `kind: "agent"` step instead of wrapping the child invocation in Python or shell just to launch it.
- Wrapper scripts are exceptions for genuinely non-Cargo-AI behavior around the call, not the default way to invoke a child agent.
- A `kind: "agent"` run step may also set `profile` as a literal string or single variable reference; Cargo AI resolves it at step runtime and forwards `--profile <name>` to the child.
- `run_vars` is the child-step equivalent of repeatable `--run-var NAME=VALUE`.
- `input_overrides` is the child-step equivalent of repeatable `--input-override NAME=VALUE`.
- A `kind: "agent"` run step may also set `input_mode` to `replace`, `append`, or `prepend` when child `inputs` are present.
- Child `run_vars`, child `input_overrides`, child `inputs`, and child `input_mode` are all optional.
- Child `run_vars` must be an object keyed by the intended child runtime-var name.
- Child `run_vars` values must be scalar literals (`string`, `number`, or `boolean`) or a single variable reference in the exact shape `{ "var": "<name>" }`.
- Child `input_overrides` must be an object keyed by the intended child named-input slot.
- Child `input_overrides` values must be either a string literal, a single variable reference in the exact shape `{ "var": "<name>" }`, or a named parent-input reference in the exact shape `{ "input": "<name>" }`.
- Child `inputs` may contain either literal input objects or a named parent-input reference in the exact shape `{ "input": "<name>" }`.
- `{ "input": "<name>" }` may reference only declared named top-level inputs from the parent definition.
- Cargo AI keeps parent and child agents atomic at hatch/check time; whether a child actually declares a `run_vars` key or `input_overrides` key and whether the resolved values match are checked when the child runs.
- When a child run step provides both `run_vars` and named/input payload values, Cargo AI forwards `run_vars` first, then named `input_overrides`, and then merges anonymous child `inputs` with `input_mode`.
- Child `input_mode` applies only to child `inputs`; it does not change whether `input_overrides` are sent.
- If child `input_mode` is omitted, the child step keeps the current default behavior: child `inputs` replace the child agent's baked `inputs`.
- Child `append` keeps the child agent's baked inputs first, then appends the action-supplied child inputs in declared order.
- Native child-agent steps preserve Cargo AI semantics such as child `run_vars`, named input forwarding, child `input_overrides`, child `inputs`, `input_mode`, failure handling, depth limits, and append-only `using:` observability.
- Child `prepend` keeps the action-supplied child inputs first in declared order, then places the child agent's baked inputs after them.
- Those child inputs may use dynamic string parts resolved from the parent action-local variable bag. Child `run_vars` and child `input_overrides` stay CLI-shaped and accept only scalar literals or single variable references.
- Named child-input reuse is explicit only; Cargo AI does not automatically inherit all named inputs into children.
- If a child only consumes a forwarded named input, it does not need to declare the same name locally.
- If a child wants to forward that same named input to its own child, it should declare the same named top-level input locally so the incoming value can bind there first.
- A parent may observe child-agent success or failure through `status_variable` and `error_variable`.
- A parent cannot directly capture the child agent's top-level returned output fields into its own action-local variables.
- Treat child agents as orchestration/control-flow steps with input forwarding, not as automatic structured-output merging into the parent.

## Validation Expectations

Validation checks structure and declared contracts. It does not prove that a model's claims are true, that an instruction is trustworthy, or that a tool's deeper application data is correct. Model or tool output cannot add permissions or new action steps; executable work remains governed by the authored definition and applicable runtime permission checks. Raw `exec` output remains text.

### Stable object and reference shapes

Besides the descriptor and step tables above, these objects have exhaustive keys:

| Object | Allowed keys |
|---|---|
| Root | `agent_definition_schema_version`, `agent_schema`, `actions`, optional `inputs`, `runtime_vars`, `action_execution` |
| Top-level input | `type`, optional `name`, and optional matching `text`, `url` or `path` |
| Runtime-variable descriptor | `type`, optional `default` |
| Action | `name`, `logic`, `run` |
| Child input | exactly `input`, or `type` plus the matching required `text`, `url` or `path` |
| Child input override reference | exactly `var` or exactly `input`; a plain string is also valid |
| Child runtime-variable reference | exactly `var`; a string, number or boolean literal is also valid |
| Image reference | exactly `input` or exactly `path` |

The open-name maps are schema `properties`, `runtime_vars`, tool `params`, child `run_vars`, and child `input_overrides`. Their names and values must still satisfy the applicable identifier, reference and tool rules. Additional stable keys require a supported future contract; there is no extension container.

### Resource limits

Strict definitions and their structured model output receive bounded validation before actions:

| Resource | Maximum |
|---|---:|
| Aggregate decoded UTF-8 key/string bytes | 1 MiB |
| Individual string / key | 256 KiB / 256 bytes |
| JSON value nodes / depth, with root at zero | 65,536 / 32 |
| Members in one object / entries in one array | 4,096 / 4,096 |
| Inputs, runtime variables, schema properties per object | 256 each |
| Actions / steps per action / total steps | 256 / 256 / 4,096 |
| Tool params, child inputs/overrides/run vars, image references | 256 each |
| Enum entries / string parts | 1,024 each |
| Validation work, charged node/constraint visits | 262,144 |

Decoded-byte limits count keys and string values, not formatting whitespace. Work includes repeated node and constraint checks. These bounds apply to parsed strict data and its validation; they do not bound transport reads, JSON parsing allocations, or legacy validation. These are resource ceilings, not performance promises. Split an oversized workflow or reduce repeated/literal data instead of changing the version to bypass validation.

### Reading errors

Definition errors carry a stable `code`, a `$`-rooted JSON `path`, a `message`, sorted `expected_keys`, and a `corrective_action`. Keys that cannot use dotted notation have bracket-quoted paths, such as `$.agent_schema.properties["review.note"]`. Resource errors also report `limit`, `maximum`, and `observed`. Transport context may add a prefix without changing the semantic code or path.

Codes are `invalid_json`, `missing_required_field`, `invalid_type`, `invalid_value`, `unknown_field`, `unsupported_schema_version`, `legacy_schema_key`, `unsupported_schema_keyword`, `unsupported_operator`, `invalid_reference`, `duplicate_name`, and `limit_exceeded`. Correct the reported path and rerun validation; do not remove a required constraint just to silence an error.

Expect `cargo ai hatch <agent-name> --config <config.json> --check` to reject at least these cases:
- missing required top-level keys
- legacy `version`
- malformed `agent_definition_schema_version`
- unsupported versions at or after the strict cutoff
- unknown stable-object keys, unsupported schema keywords or logic operators
- exceeded resource limits
- unsupported `agent_schema` property types
- nested arrays, deeper nested objects, or unsupported union types in `agent_schema`
- invalid `description`, `enum`, or numeric-bound metadata on a property
- unsupported run-step kind
- missing required fields for a step kind
- invalid child-agent path shape
- malformed child-agent `input_overrides`
- malformed child-agent `run_vars`
- invalid child-agent `input_mode`
- duplicate named top-level inputs
- unnamed top-level inputs in the structural action-only shape
- malformed or unknown `{ "input": "<name>" }` child references
- invalid relative file or image paths
- invalid generated-image output path extension
- invalid `platform` value
- `output_variable` on non-`exec` / non-`tool` steps
- malformed or unknown tool `params`
- tool `describe.result` that is not a nullable string schema
- captured-variable collisions
- malformed `when`
- malformed `failure_mode`

## Minimal Valid Examples

These are schema and step fragments to use inside a complete definition.

```json
{
  "type": "object",
  "properties": {
    "unit": {
      "type": "string",
      "description": "Temperature unit.",
      "enum": ["F", "C"]
    },
    "confidence": {
      "type": "number",
      "description": "Confidence score greater than 0 and less than or equal to 1.",
      "exclusiveMinimum": 0,
      "maximum": 1
    }
  }
}
```

```json
{ "kind": "agent", "agent": "./child_reporter" }
```

```json
{ "kind": "tool", "name": "hello_tool", "params": { "name": { "var": "runtime.name" } }, "output_variable": "greeting" }
```

```json
{ "kind": "email_me", "subject": "Done", "text": "Finished." }
```

```json
{ "kind": "exec", "program": "/bin/echo", "args": ["hello"], "platform": "macos" }
```

## Default Authoring Loop

1. Draft or edit one JSON file.
2. Run `cargo ai hatch <agent-name> --config <config.json> --check`.
3. Fix reported errors.
4. Build only after `--check` passes.

For onboarding and pattern selection, read:
- `start-here.md`
- `pattern-selection.md`
- `examples/README.md`
