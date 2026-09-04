# immich-alt-text: design

Date: 2026-09-04
Status: approved in a brainstorming session, ready for an implementation plan

## 1. Goal

A small Rust CLI with a terminal user interface (TUI). It reads photos from a
personal Immich server, sends each photo to a vision model over an
OpenAI-compatible API, and writes the returned description back to Immich.
The TUI shows progress, the photo in flight, and a log of results. The user
controls the run with the keyboard.

This is a personal, experimental project. Keep it tiny. Do not add features
that this document does not name.

## 2. Decisions made

| Topic | Decision |
| --- | --- |
| Scope of a run | All assets of type `IMAGE` whose description is null or blank. Immich holds the state. A re-run picks up unfinished photos. Hand-written descriptions stay untouched. |
| Videos | Excluded. |
| LLM API | OpenAI chat completions only. Works with LM Studio, Ollama `/v1`, llama.cpp server, vLLM, OpenRouter, OpenAI. No provider abstraction. |
| Concurrency | Worker count from config. Default 1. Not changeable while running. |
| Write-back | Automatic. The TUI shows the last results in a log. No review step. |
| Prompt | Built-in default, overridable in the config file. No prompt editor in the TUI. |
| Config | TOML file plus a settings screen in the TUI. |
| Failures | Retry transient errors 3 times with backoff (4 attempts in total), then mark the asset failed and continue. Auth and config errors stop the run. |
| Keys | Start, pause, resume, quit, scroll the log, expand a log row, open settings. |
| Architecture | One tokio runtime. The engine emits `Event`s over a channel. The UI sends `Command`s back. No shared mutable state between them. |

## 3. Immich API contract

Confirmed against the Immich OpenAPI spec for v3.1.0 (stable) and v3.2.0-rc.
All paths sit under `/api`. Auth header: `x-api-key: <key>`.

| Purpose | Call | Notes |
| --- | --- | --- |
| Connection test | `GET /server/version` | Returns `major`, `minor`, `patch`. |
| List assets | `POST /search/metadata` | Body: `{ "type": "IMAGE", "withExif": true, "size": 1000, "page": N, "order": "desc" }`. Response: `assets.items[]`, `assets.nextPage`. `page` and `type` are deprecated in v3.2 but still work. Filter on our side: keep items where `exifInfo.description` is null or blank after trim. |
| Preview JPEG | `GET /assets/{id}/thumbnail?size=preview` | Returns `image/jpeg`, about 1440px on the long side. |
| Write description | `PUT /assets/{id}` | Body: `{ "description": "<text>" }`. |

Asset fields the CLI reads: `id`, `originalFileName`, `exifInfo.description`.

Future note, not in scope: v3.2 adds `cursor`, `filter`, and `orderBy` to the
search body. Move to them when the deprecated fields disappear.

## 4. LLM API contract

`POST {base_url}/chat/completions`. Header `Authorization: Bearer <key>` only
when the key is not empty.

Request:

```json
{
  "model": "<model>",
  "max_tokens": 200,
  "messages": [
    {
      "role": "user",
      "content": [
        { "type": "text", "text": "<prompt>" },
        { "type": "image_url", "image_url": { "url": "data:image/jpeg;base64,<...>" } }
      ]
    }
  ]
}
```

Response: `choices[0].message.content`, trimmed. An empty string counts as a
failure for that asset. The code does not rewrite the text. Output shape is
the prompt's job.

Default prompt:

```
Write alt text for this photo: one or two plain sentences describing what is
visible. No preamble, no quotes, no "This image shows".
```

## 5. Modules

One binary crate, `immich-alt-text`, Rust 2021 edition.

| Module | One job | Depends on |
| --- | --- | --- |
| `config` | Load, validate, save the TOML file. Default path from `$XDG_CONFIG_HOME` or `~/.config`. | `serde`, `toml` |
| `immich` | Typed client: list image pages, fetch preview JPEG, update description, get version. | `reqwest`, `serde` |
| `llm` | One function: JPEG bytes plus prompt in, description out. | `reqwest`, `base64` |
| `engine` | The pipeline. Owns paging, the worker pool, retries. Emits `Event`s, accepts `Command`s. | `immich`, `llm`, `tokio` |
| `app` | Pure state: counters, log ring buffer, screen enum, settings form. `on_event`, `on_key`. No I/O. | nothing |
| `ui` | Ratatui widgets that draw `app::App`. Render only. | `ratatui` |
| `theme` | Named color constants and the mono fallback. | `ratatui` |
| `main` | Terminal setup, tokio runtime, event loop, teardown. | all |

Crates: `ratatui`, `tokio`, `reqwest` (feature `json`;
`reqwest` uses rustls by default in 0.13), `serde`, `serde_json`, `toml`, `base64`, `chrono`,
`thiserror`, `anyhow`, `tracing`, `tracing-appender`, `tokio-util` (for
`CancellationToken`), `clap` (for `--config`).

Dev crates: `wiremock`, `insta`.

### Boundaries

```rust
enum Command { Start, Pause, Resume, Quit }

enum Event {
    PageLoaded { scanned: u64, queued: u64 },
    DiscoveryDone { total_queued: u64 },
    AssetStarted { id: String, name: String },
    AssetStage { id: String, stage: Stage },   // Fetching | CallingLlm | Writing
    AssetDone { id: String, name: String, description: String, took: Duration },
    AssetFailed { id: String, name: String, error: String },
    RunFinished { done: u64, failed: u64, elapsed: Duration },
    Fatal { error: String },
    ConnectionTest { immich: Result<String, String>, llm: Result<String, String> },
}
```

Each engine runtime owns its `tokio::sync::mpsc` event receiver. `main` forwards
events from only the active runtime to `app.on_event`; replacing a runtime drops
its queued events. `app.on_key` may return a `Command`, which `main` sends to the
engine. The engine never sees the UI. The UI never makes HTTP calls.

## 6. Engine behavior

**Discovery.** Page through `/search/metadata` from page 1, newest first.
Keep assets with an empty description. Emit `PageLoaded` per page. Push kept
assets into a bounded channel with capacity `workers * 4`. Discovery blocks
when the channel is full, so memory stays flat for a large library. Emit
`DiscoveryDone` after the last page.

**Workers.** `workers` tasks. Each loop:

1. Take an asset from the queue.
2. Emit `AssetStarted`.
3. Emit `AssetStage(Fetching)`. Fetch the preview JPEG.
4. Emit `AssetStage(CallingLlm)`. Call the LLM.
5. Emit `AssetStage(Writing)`. Write the description to Immich.
6. Emit `AssetDone` with the elapsed time.

Each HTTP call runs with a timeout. Immich calls use `immich.timeout_secs`.
LLM calls use `llm.timeout_secs`.

**Retries.** Each stage runs inside a retry wrapper. `run.retries` is the number
of retries after the first try, default 3, so up to 4 attempts. Backoff between
attempts: 2s, 4s, 8s. Only `Transient` errors retry. After the last attempt,
emit `AssetFailed` and continue with the next asset.

**Error classes.**

| Class | Examples | Effect |
| --- | --- | --- |
| `Transient` | connection refused, timeout, HTTP 5xx, 429 | retry, then `AssetFailed` |
| `Permanent` per asset | HTTP 404 for one asset, malformed JSON, empty LLM text | `AssetFailed`, no retry |
| `Fatal` | HTTP 401 or 403 from Immich, HTTP 401 or 404 (unknown model) from the LLM | `Event::Fatal`, run stops |

**Commands.** `Start` begins discovery when the state is idle or finished.
`Pause` stops handing out new assets. In-flight assets finish and report.
`Resume` continues. `Quit` cancels the `CancellationToken`. All tasks stop.
`main` waits up to 5 seconds for in-flight writes to finish.

**Run end.** When discovery is done and the queue is empty and no worker is
busy, emit `RunFinished`.

## 7. App state

`App` is a plain struct with no I/O.

- `screen: Screen` where `Screen` is `Run` or `Settings`.
- `run_state: RunState` where `RunState` is `Idle`, `Running`, `Paused`,
  `Finished`, `Error(String)`.
- Counters: `scanned`, `queued`, `done`, `failed`.
- `in_flight: Vec<InFlight>` with `id`, `name`, `stage`, `started_at`. One
  entry per busy worker.
- `log: VecDeque<LogRow>` capped at 500. Newest first. A `LogRow` is `Done`
  with description and duration, or `Failed` with the error.
- `log_selected: Option<usize>` and `log_expanded: bool`.
- `rate` and `eta` from a moving average over the last 20 `AssetDone` events.
- `settings: SettingsForm` with one field per config value shown in the
  settings screen, a `focused` index, a `show_key` flag, and the last
  connection test result.

Key handling, run screen:

| Key | Action |
| --- | --- |
| `s` | `Command::Start` when idle or finished |
| `p` | `Command::Pause` when running, `Command::Resume` when paused |
| `↑` `↓` | move the log highlight |
| `enter` | toggle the expanded popup for the highlighted row |
| `esc` | close the popup |
| `c` | open settings |
| `q`, `ctrl-c` | `Command::Quit`, then exit |

Key handling, settings screen:

| Key | Action |
| --- | --- |
| `tab`, `shift-tab` | move focus between fields |
| printable, backspace | edit the focused field |
| `ctrl-r` | toggle API key visibility |
| `ctrl-t` | test both connections, show results in the form |
| `ctrl-s` | validate and save, then return to the run screen |
| `esc` | discard edits and return |

Saving while a run is running is refused with a footer message. Pause first.
Replacement waits for any in-flight Immich write to receive a response or hit
its configured HTTP timeout before the new engine becomes active.

## 8. UI

Two screens. Style: thin rounded borders, a title on each box, dim labels,
bright values, one colored word for state.

### Run screen at 120x40

```
╭─ immich-alt-text ───────────────────────── photos.home.lan ─ gemma-3-12b @ localhost:1234 ─ 1 worker ─ RUNNING ╮
│ ╭─ progress ───────────────────────────────────────────────╮ ╭─ counters ────────────────────────────────────╮ │
│ │ ████████████████████░░░░░░░░░░░░░░░░░░░░░░░░  1 284 / 3 102 │ │ scanned    14 920      done       1 284        │ │
│ │ elapsed 01:42:17   rate 12.6/min   eta 02:24              │ │ queued      1 818      failed        3        │ │
│ │                                                          │ │ avg llm   4.1 s        avg total   4.7 s      │ │
│ ╰──────────────────────────────────────────────────────────╯ ╰───────────────────────────────────────────────╯ │
│ ╭─ in flight ──────────────────────────────────────────────────────────────────────────────────────────────╮ │
│ │ ● IMG_4471.HEIC      calling llm…   3.2 s                                                                │ │
│ ╰──────────────────────────────────────────────────────────────────────────────────────────────────────────╯ │
│ ╭─ log ────────────────────────────────────────────────────────────────────────────────────────────────────╮ │
│ │ 18:42:11  ✓ IMG_4470.HEIC  4.3 s  A golden retriever sits on a wooden dock at sunset, looking toward…    │ │
│ │ 18:42:02  ✗ IMG_4468.HEIC        llm: timeout after 120 s (3 tries)                                      │ │
│ ╰──────────────────────────────────────────────────────────────────────────────────────────────────────────╯ │
│  s start   p pause   ↑↓ scroll log   enter expand   c settings   q quit                                        │
╰────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
```

- **Header**: Immich host, model and LLM host, worker count, run state.
- **Progress**: bar against `queued` so far. Elapsed, rate per minute, ETA.
  The bar and ETA move from page 1 and refine as discovery continues.
- **Counters**: scanned, queued, done, failed, average LLM time, average
  total time per asset.
- **In flight**: one row per busy worker. File name, stage, live timer.
- **Log**: ring buffer, newest first. One line per row. Failures show the
  error in red. `enter` opens a centered popup with the full text.
- **Footer**: key hints. Keys that do nothing in the current state are dim.

### Settings screen

```
╭─ settings ──────────────────────────────────────────────────╮
│ immich url        https://photos.home.lan                    │
│ immich api key    ••••••••••••••••••••••••••••••  ctrl-r show │
│ llm base url      http://localhost:1234/v1                   │
│ llm api key                                                   │
│ llm model         gemma-3-12b-it                              │
│ workers           1                                           │
│ max tokens        200                                         │
│                                                               │
│ ctrl-t test connections   immich ✓ v3.1.0     llm ✓ 200 OK   │
│                                                               │
│ ctrl-s save   ctrl-t test   esc back                          │
╰───────────────────────────────────────────────────────────────╯
```

First launch with no config file opens this screen with the LM Studio defaults
filled in. Prompt, timeouts, retries, and page size are file-only.

The connection test calls `GET /server/version` on Immich and
`GET {base_url}/models` on the LLM. It runs on the tokio runtime and reports on
a separate channel with an `Event::ConnectionTest` variant carrying a request
ID and both results. Starting a newer test cancels the old task, and the app
ignores a stale result that was already queued.

### Small terminals

- Below 80 columns: the counters box moves under the progress box.
- Below 24 rows: the in-flight box is hidden.
- At 40x10: header, bar, and footer only. Nothing panics.

### Colors

The palette follows btop. `theme` holds these as named constants.

| Element | Color |
| --- | --- |
| Borders | dim gray |
| Box titles | bright white |
| Labels | dim gray |
| Values | white |
| Progress bar | graded green → yellow → orange → red along its length, 256-color palette |
| `RUNNING` | green |
| `PAUSED` | yellow |
| `ERROR` | red |
| `IDLE`, `FINISHED` | cyan |
| Log timestamp | dim gray |
| Log ✓ | green |
| Log ✗ and error text | red |
| File names | cyan |
| Durations | yellow |
| Descriptions | white |
| In-flight dot | magenta |
| Stage word | cyan |
| Footer key letters | magenta |

`[ui] theme = "mono"` turns all colors off.

## 9. Config

Path: `~/.config/immich-alt-text/config.toml`. Override with `--config <path>`.
Written with mode 0600. Missing keys take defaults.

```toml
[immich]
url = "https://photos.home.lan"
api_key = "..."
timeout_secs = 30

[llm]
base_url = "http://localhost:1234/v1"
api_key = ""
model = "gemma-3-12b-it"
max_tokens = 200
timeout_secs = 120
prompt = """
Write alt text for this photo: one or two plain sentences describing what is
visible. No preamble, no quotes, no "This image shows".
"""

[run]
workers = 1
retries = 3
page_size = 1000

[ui]
theme = "btop"
```

Validation: both URLs parse and use `http` or `https`. `workers` is between
1 and 64. `page_size` is between 1 and 1000. `retries` is at most 10. `model`
is not empty.
`immich.api_key` not empty.

Saving stages a mode-0600 temporary file in the destination directory, flushes
and syncs it, then atomically renames it over the previous config.

## 10. Logging and shutdown

`tracing` writes UTC daily files named
`~/.local/state/immich-alt-text/debug.log.YYYY-MM-DD`. Level from `RUST_LOG`,
default `info`. Every HTTP call logs method,
path, status, and duration. Bodies and keys are never logged.

`q` and `ctrl-c` cancel the engine, wait up to 5 seconds for in-flight writes,
restore the terminal, exit 0. A panic hook restores the terminal before the
panic message prints.

## 11. Testing

| Module | Tests |
| --- | --- |
| `config` | Round-trip a full file. Load a minimal file and check defaults. Reject an invalid URL and `workers = 0`. |
| `immich` | `wiremock`. Assert header, body, and query for each call. Cover 200, 401, 500, malformed body. |
| `llm` | `wiremock`. Assert the data URI prefix and message shape. Cover 200, empty content, 401, 500. |
| `engine` | `wiremock` for both servers. (a) 3 assets, one with a description: exactly 2 LLM calls, 2 writes, expected event order. (b) LLM fails twice then succeeds: one `AssetDone`. (c) LLM fails 3 times: `AssetFailed`. (d) `Pause` mid-run: no new `AssetStarted` until `Resume`. (e) 401 from Immich: `Fatal`. |
| `app` | Feed events, check counters, rate, ring buffer cap. Feed keys, check screen changes and that `p` does nothing when idle. |
| `ui` | `insta` snapshots with `TestBackend` at 120x40, 80x24, 40x10. |
| Manual | `cargo run --example fake_servers` starts a fake Immich and a fake LLM and writes `target/demo-config.toml`. `cargo run -- --config target/demo-config.toml` then shows the TUI moving with no real library. |

No test needs a real Immich or a real model.

## 12. Build order

1. **Skeleton**: Cargo project, `config`, `Event` and `Command` enums,
   `theme`, empty `app`. Everything depends on this.
2. **`immich` client** with tests.
3. **`llm` client** with tests.
4. **`engine`**, depends on 2 and 3.
5. **`app` + `ui` + `main`**, depends on 1. Built against fake events, joined
   with the engine last.

Tasks 2, 3, and 5 can run in parallel after task 1.

## 13. Out of scope

Headless mode, album or date filters, a review step, prompt editing in the
TUI, live worker count changes, video support, other LLM providers, image
thumbnails in the terminal, and any local database.
