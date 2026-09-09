# Animal Patrol ownership fixture

`animal_patrol.json` sends one PNG input and requests a structured finding list. `csv_writer.rs` is editable project-owned Rust that appends those findings to `findings.csv` in the runtime data directory. Its protocol declares filesystem metadata reads and writes; no network, environment, credential, or child-process access is needed by the tool.

`patrol.png.base64` encodes a valid one-pixel PNG sentinel. It is synthetic image data, not an animal-recognition benchmark. Both OpenAI and Gemini loopback adapters return `findings.json`; `expected.csv` checks typed transfer, nullable notes, quotation escaping, and stable row order. The process test verifies the exact same image bytes and unchanged definition with both profiles.

The fixture uses only disposable local homes and fake tokens. It proves the local ownership contract, image transport, validation, CSV materialization, packaging, installation, and source-edit independence. It does not evaluate model perception or fresh-user onboarding.

The same bounded schema is deliberately rejected by the Anthropic adapter before a network request or CSV write. The test keeps its numeric constraints and chooses compatible profiles; it never strips the bounds to manufacture portability. Existing-project initialization first retains the legacy write location, then the fixture explicitly adopts the namespaced data root.
