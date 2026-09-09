# Definition validation corpus

Each JSON file contains one complete definition and its expected semantic result:

```json
{
  "name": "case_name",
  "definition": {},
  "expected": {
    "accepted": false,
    "code": "missing_required_field",
    "path": "$.agent_definition_schema_version"
  }
}
```

Accepted cases use only `{"accepted": true}`. Consumers in the interpreted parser, generated build support and backend must evaluate the `definition` value, rather than treating the fixture wrapper as a definition. Rejection assertions compare the shared semantic code and canonical JSON path; transport prefixes and explanatory text are not the contract asserted here. Enumerate files deterministically by filename.

The corpus covers strict `2026-09-09.r1` authoring, model-only empty actions, supported scalar/structured output, named child forwarding, literal tool data, stable-key rejection, unsupported schema keywords/operators, version selection, missing/type/reference failures and duplicate names. Legacy positive cases explicitly use valid older revisions and retain unknown root keys, including an older revision outside the published examples. Future revisions are negative cases. The singular `platform` key is intentional.

These are validation fixtures, not runnable workflows. References to tools, child agents, assets and commands are synthetic; no fixture requires real credentials, a provider, filesystem materialization or network access. The remote `$ref` negative case uses a reserved invalid domain and must fail structurally without fetching it. A literal tool object with `var` plus another key is data; only an exact single-key `var` object is a variable reference.

The canonical boundary builders in `tests/support/definition_validation_cases.rs` construct exact-limit and one-over cases without oversized fixture files. The interpreted parser, generated build support and backend consume the same definition cases, while both local model-output paths consume the output-work cases. Raw malformed JSON and model/tool output envelopes require their own ingestion/runtime tests because `definition` here is already a JSON value. Successful corpus validation proves the declared structural contract, not command execution, model quality or tool availability.
