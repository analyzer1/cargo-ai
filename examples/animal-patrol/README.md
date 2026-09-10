# Animal Patrol

Inspect an image, review structured animal observations, and append them to a CSV you own. The definition and Rust CSV writer are editable source. Hosting is optional; the acquired project and installed package run independently with a compatible runtime profile.

The bundled image is **AI-generated**, depicting two deer in a woodland clearing. It is not an actual wildlife sighting. [Image provenance](assets/PROVENANCE.md) records the prompt and qualitative acceptance criteria. This example is not a wildlife-recognition benchmark.

From this directory, with Cargo AI and Rust installed:

```sh
cargo ai build default
cargo ai run patrol.json --profile YOUR_IMAGE_PROFILE
cargo ai package default --output-dir target/package
cargo ai packages install target/package --as animal_patrol
cargo ai packages inspect animal_patrol
cargo ai run animal_patrol::patrol --profile YOUR_IMAGE_PROFILE
```

Choose a configured image-capable OpenAI or Gemini profile that supports the definition's bounded structured-output schema. The selected profile controls the provider and model. Do not remove schema constraints to claim compatibility with another adapter. Runtime credentials stay in your own credential store and are never included in the package.

Review `patrol.json`, `tools/csv_writer/src/main.rs`, the package inventory and the subprocess permission before running. The image and instructions go to the selected provider. The CSV tool declares no network, credential, environment-read or child-process access; it appends to `findings.csv` in the runtime data directory. Executable source tools have open-world potential when granted execution; these declarations are not an operating-system sandbox.

Project runs store mutable output beneath `.cargo-ai/data`; installed runs use the installed alias's separate data directory. The CSV has columns `revision,location,animal,count,confidence,note`. One row represents an animal category and count. Repeated runs append observations. To adapt the example, replace `assets/patrol.png`, edit the definition and CSV writer, then rebuild and repackage. Keep credentials and private runtime data out of declared assets.

The qualification declaration runs the mandatory build, package, install, inspect, run and uninstall lifecycle with a loopback provider. `fixtures/findings.json` and `fixtures/expected.csv` are deterministic transport/tool fixtures, **not live perception results**. They check the actual bundled image bytes reach the fixture provider and the installed writer produces the expected CSV. The live acceptance criterion is two deer, with valid CSV shape and no invented categories; confidence and prose need not exactly match the fixture.

`spdx.json` inventories the distributed source/assets and resolved Rust tool dependencies. It records file hashes and component licenses; the final package archive digest is recorded separately to avoid a self-referential checksum. Inventory and provenance establish content correspondence, not safety.
