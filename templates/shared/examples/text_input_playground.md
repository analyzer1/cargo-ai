# `text_input_playground.json`

Use this as a simple text-input example you can copy into an empty folder and hatch locally.

```json
{
  "agent_definition_schema_version": "2026-09-09.r1",
  "inputs": [
    {
      "type": "text",
      "text": "Summarize the meeting notes in one sentence."
    }
  ],
  "agent_schema": {
    "type": "object",
    "properties": {
      "summary": {
        "type": "string",
        "description": "One-sentence summary of the notes."
      }
    }
  },
  "actions": [
    {
      "name": "print_summary",
      "logic": {
        "!=": [
          { "var": "summary" },
          ""
        ]
      },
      "run": [
        {
          "kind": "exec",
          "program": "echo",
          "args": ["Summary created."]
        },
        {
          "kind": "exec",
          "program": "echo",
          "args": [{ "var": "summary" }]
        }
      ]
    }
  ]
}
```

Save it as `text_input_playground.json`, then run:

```bash
cargo ai hatch text_input_playground.json
./text_input_playground
```

For Windows users, run `text_input_playground.exe` or just `text_input_playground`.

You can also override the baked input at runtime:

```bash
./text_input_playground --input-text "This was a long meeting about launch timing, customer rollout, support staffing, and pricing changes. Summarize the most important takeaway in one sentence."
```

If you want the matching explanatory sidecar file next to the JSON, save this as `text_input_playground.md`:

```md
# `text_input_playground`

## Purpose
- Summarize meeting notes in one sentence.

## Inputs
- One text input with the notes to summarize.

## Output
- `summary`: one-sentence summary of the notes.

## Action Flow
- If `summary` is not empty, run `echo "Summary created."`
- Then print the actual `summary` value.

## Local Loop
- Hatch with `cargo ai hatch text_input_playground.json`
- Run `./text_input_playground`
- Override the baked text later with `./text_input_playground --input-text "..."`
```
