# immich-alt-text

A small terminal app that describes the photos in your [Immich](https://immich.app)
library with a vision model and writes the text back as each photo's description.

Built with Rust and [Ratatui](https://ratatui.rs). Personal project, experimental.

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

Design: `docs/design.md`. Plan: `docs/plans/2026-09-04-immich-alt-text.md`.
