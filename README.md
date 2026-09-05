# immich-alt-text

A small terminal app that describes the photos in your [Immich](https://immich.app)
library with a vision model and writes the text back as each photo's description.

![immich-alt-text running in the terminal](tui.jpg)

Built with Rust and [Ratatui](https://ratatui.rs). Personal project, experimental.
See the [architecture guide](ARCHITECTURE.md) for the CLI's module boundaries,
runtime lifecycle, concurrency model, TUI behavior, and testing seams.

## What it does

1. Lists every image in your Immich library whose description is empty.
2. Downloads the preview JPEG for each one.
3. Sends it to an OpenAI-compatible chat endpoint with a vision model.
4. Writes the returned sentence back to Immich.

Immich keeps the state. A photo with a description is skipped, so you can stop and
start the run at any time. Hand-written descriptions are never touched.

## Requirements

- Rust 1.88 or newer.
- An Immich server and an API key (Account settings → API keys). For the least
  privilege, enable these permissions when creating the key:
  - `asset.read` — list assets and read their descriptions.
  - `asset.view` — download preview thumbnails for images.
  - `asset.update` — write the generated description back to Immich.
  The CLI's server-version check does not need an additional permission. These
  are Immich's fine-grained API-key permissions (see the [Immich API
  documentation](https://api.immich.app/)); on older Immich versions that do not
  offer them, use the server's full-access API-key option instead.
- A vision model behind an OpenAI-compatible API. Tested with LM Studio at
  `http://localhost:1234/v1`. Ollama, llama.cpp server, vLLM, OpenRouter, and OpenAI
  work with the same setting.

## Run

```bash
cargo install --path .
immich-alt-text
```

The first launch opens the settings screen. Fill in the Immich URL, the API key, the
LLM base URL, the model name, and press `ctrl-t` to test both connections.
`ctrl-s` saves to `~/.config/immich-alt-text/config.toml` and returns to the run
screen. Press `s` to start.

## Keys

| Screen | Key | Action |
| --- | --- | --- |
| run | `s` | start a run |
| run | `p` | pause or resume |
| run | `↑` `↓` | move through the log |
| run | `enter` | show the full description of the highlighted row |
| run | `c` | open settings |
| run | `q` or `ctrl-c` | quit |
| settings | `tab` `shift-tab` | move between fields |
| settings | `ctrl-r` | show or hide API keys |
| settings | `ctrl-t` | test both connections |
| settings | `ctrl-s` | save and go back |
| settings | `esc` | discard edits and go back |

## Config file

```toml
[immich]
url = "https://photos.home.lan"
api_key = "..."
timeout_secs = 30

[llm]
base_url = "http://localhost:1234/v1"
api_key = ""            # optional
model = "gemma-3-12b-it"
max_tokens = 200
timeout_secs = 120
prompt = """
Write alt text for this photo: one or two plain sentences describing what is
visible. No preamble, no quotes, no "This image shows".
"""

[run]
workers = 1             # parallel LLM calls, 1–64
retries = 3             # 0–10 retries; default backoff is 2 s, 4 s, 8 s
page_size = 1000        # 1–1000

[ui]
theme = "btop"          # or "mono"
```

`prompt`, the timeouts, `retries`, `page_size`, and `theme` are file-only. The
settings screen covers the rest.

## Logs

UTC daily logs are named `~/.local/state/immich-alt-text/debug.log.YYYY-MM-DD`.
Set `RUST_LOG=debug` for more detail. Request bodies and keys are never logged.

## Try it without a real library

```bash
cargo run --example fake_servers          # terminal 1
cargo run -- --config target/demo-config.toml   # terminal 2
```

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

### Pull request build artifacts

Every pull request opened, synchronized, or reopened builds three release
binaries. Open the workflow run's artifacts in GitHub Actions to download
either one of these target-specific artifacts or the combined
`immich-alt-text-builds` bundle:

- `immich-alt-text-x86_64-unknown-linux-musl` — portable Linux x86_64
- `immich-alt-text-aarch64-unknown-linux-musl` — portable Linux ARM64
- `immich-alt-text-aarch64-apple-darwin` — Apple Silicon macOS

GitHub downloads each artifact as an outer `.zip` file. After extracting it, a
target-specific artifact contains one inner `.tar.gz`; the combined
`immich-alt-text-builds` artifact contains all three inner target archives.
Each `.tar.gz` has the executable `immich-alt-text` at its archive root.

These PR artifacts are retained for 14 days. To download one, open the pull
request's **Checks** tab, select the **Build artifacts** workflow run, and
choose an artifact from the **Artifacts** section.

These are unsigned development artifacts. Tag-triggered GitHub Releases,
signing, and checksums are intentionally deferred.

Design: `docs/design.md`. Plan: `docs/plans/2026-09-04-immich-alt-text.md`.
