use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use immich_alt_text::config::{Config, ImmichConfig, LlmConfig, RunConfig, UiConfig};
use immich_alt_text::engine::{self, EngineOptions};
use immich_alt_text::events::{Command, Event, Stage};
use serde_json::json;
use tokio::sync::{mpsc, Notify};
use wiremock::matchers::{body_json, body_string_contains, method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0];

fn config(immich: &MockServer, llm: &MockServer) -> Config {
    config_with_run(immich, llm, 1, 3, 10)
}

fn config_with_run(
    immich: &MockServer,
    llm: &MockServer,
    workers: usize,
    retries: u32,
    page_size: u32,
) -> Config {
    Config {
        immich: ImmichConfig {
            url: immich.uri(),
            api_key: "k".into(),
            timeout_secs: 5,
        },
        llm: LlmConfig {
            base_url: format!("{}/v1", llm.uri()),
            api_key: String::new(),
            model: "m".into(),
            max_tokens: 50,
            timeout_secs: 5,
            prompt: "describe".into(),
        },
        run: RunConfig {
            workers,
            retries,
            page_size,
        },
        ui: UiConfig::default(),
    }
}

fn fast() -> EngineOptions {
    EngineOptions {
        backoff_base: Duration::from_millis(1),
    }
}

fn search_page(items: &[(&str, &str, Option<&str>)]) -> serde_json::Value {
    search_page_with_next(items, None)
}

fn search_page_with_next(
    items: &[(&str, &str, Option<&str>)],
    next_page: Option<u32>,
) -> serde_json::Value {
    let items: Vec<_> = items
        .iter()
        .map(|(id, name, desc)| {
            json!({ "id": id, "originalFileName": name, "type": "IMAGE",
                    "exifInfo": { "description": desc } })
        })
        .collect();
    json!({ "assets": { "count": items.len(), "total": items.len(), "facets": [],
                        "nextPage": next_page.map(|n| n.to_string()), "items": items } })
}

fn completion(text: &str) -> serde_json::Value {
    json!({ "choices": [ { "index": 0, "message": { "role": "assistant", "content": text } } ] })
}

struct NotifyResponder {
    notify: Arc<Notify>,
    status: u16,
}

impl Respond for NotifyResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        self.notify.notify_one();
        ResponseTemplate::new(self.status)
    }
}

async fn mount_immich_basics(immich: &MockServer, items: &[(&str, &str, Option<&str>)]) {
    Mock::given(method("POST"))
        .and(path("/api/search/metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_page(items)))
        .mount(immich)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/assets/[^/]+/thumbnail$"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(JPEG.to_vec()))
        .mount(immich)
        .await;
}

async fn mount_preview(immich: &MockServer, id: &str, jpeg: &[u8], delay: Duration, status: u16) {
    Mock::given(method("GET"))
        .and(path(format!("/api/assets/{id}/thumbnail")))
        .respond_with(
            ResponseTemplate::new(status)
                .set_delay(delay)
                .set_body_bytes(jpeg.to_vec()),
        )
        .mount(immich)
        .await;
}

async fn mount_search_page(
    immich: &MockServer,
    page: u32,
    size: u32,
    items: &[(&str, &str, Option<&str>)],
    next_page: Option<u32>,
) {
    Mock::given(method("POST"))
        .and(path("/api/search/metadata"))
        .and(body_json(json!({
            "type": "IMAGE",
            "withExif": true,
            "size": size,
            "page": page,
            "order": "desc",
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(search_page_with_next(items, next_page)),
        )
        .mount(immich)
        .await;
}

async fn assert_no_matching_event(
    rx: &mut mpsc::Receiver<Event>,
    within: Duration,
    matches: impl Fn(&Event) -> bool,
) {
    let wait = tokio::time::timeout(within, async {
        loop {
            match rx.recv().await {
                Some(event) if matches(&event) => panic!("unexpected event after stop: {event:?}"),
                Some(_) => continue,
                None => return,
            }
        }
    })
    .await;

    if let Ok(()) = wait {}
}

async fn wait_for_request_count(server: &MockServer, expected: usize) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let requests = server.received_requests().await.unwrap_or_default();
            if requests.len() >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("timed out waiting for requests");
}

fn drain_ready_events(rx: &mut mpsc::Receiver<Event>) -> Vec<Event> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

async fn next_event(rx: &mut mpsc::Receiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for an event")
        .expect("event channel closed")
}

/// Drains events until `stop` matches. Returns everything seen, the match last.
async fn collect_until(
    rx: &mut mpsc::Receiver<Event>,
    stop: impl Fn(&Event) -> bool,
) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let e = next_event(rx).await;
        let done = stop(&e);
        seen.push(e);
        if done {
            return seen;
        }
    }
}

async fn start_until_first_event(
    handle: &engine::EngineHandle,
    rx: &mut mpsc::Receiver<Event>,
) -> Event {
    for _ in 0..3 {
        handle.send(Command::Start).await;
        if let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            return event;
        }
    }
    panic!("start did not begin a new run");
}

#[tokio::test]
async fn skips_described_assets_and_writes_the_rest() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(
        &immich,
        &[
            ("a1", "IMG_1.HEIC", None),
            ("a2", "IMG_2.HEIC", Some("a dog")),
            ("a3", "IMG_3.HEIC", Some("  ")),
        ],
    )
    .await;
    Mock::given(method("PUT"))
        .and(path("/api/assets/a1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&immich)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/assets/a3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&immich)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/assets/a2"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&immich)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("A dog on a dock.")))
        .expect(2)
        .mount(&llm)
        .await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    let events = collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;

    assert_eq!(
        events[0],
        Event::PageLoaded {
            scanned: 3,
            queued: 2
        }
    );
    let done: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::AssetDone {
                name, description, ..
            } => {
                assert_eq!(description, "A dog on a dock.");
                Some(name.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(done, vec!["IMG_1.HEIC", "IMG_3.HEIC"]);
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::DiscoveryDone { total_queued: 2 })));
    assert!(!events
        .iter()
        .any(|e| matches!(e, Event::AssetFailed { .. })));
    match events.last().unwrap() {
        Event::RunFinished { done, failed, .. } => {
            assert_eq!(*done, 2);
            assert_eq!(*failed, 0);
        }
        other => panic!("unexpected last event {other:?}"),
    }
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn emits_stages_in_order_for_one_asset() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[("a1", "IMG_1.HEIC", None)]).await;
    Mock::given(method("PUT"))
        .and(path("/api/assets/a1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&immich)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("x")))
        .mount(&llm)
        .await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    let events = collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;

    let stages: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::AssetStarted { .. } => Some("started".to_string()),
            Event::AssetStage { stage, .. } => Some(stage.label().to_string()),
            Event::AssetDone { .. } => Some("done".to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        stages,
        vec!["started", "fetching", "calling llm", "writing", "done"]
    );
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn retries_transient_llm_errors_then_succeeds() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[("a1", "IMG_1.HEIC", None)]).await;
    Mock::given(method("PUT"))
        .and(path("/api/assets/a1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&immich)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .expect(2)
        .mount(&llm)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("third time")))
        .expect(1)
        .mount(&llm)
        .await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    let events = collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;

    assert!(events
        .iter()
        .any(|e| matches!(e, Event::AssetDone { description, .. } if description == "third time")));
    assert!(!events
        .iter()
        .any(|e| matches!(e, Event::AssetFailed { .. })));
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn gives_up_after_all_attempts_and_continues() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(
        &immich,
        &[("a1", "IMG_1.HEIC", None), ("a2", "IMG_2.HEIC", None)],
    )
    .await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/api/assets/[^/]+$"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&immich)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(4)
        .expect(4)
        .mount(&llm)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("ok")))
        .expect(1)
        .mount(&llm)
        .await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    let events = collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;

    let failed: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::AssetFailed { .. }))
        .collect();
    assert_eq!(failed.len(), 1);
    if let Event::AssetFailed { name, error, .. } = failed[0] {
        assert_eq!(name, "IMG_1.HEIC");
        assert!(error.contains("llm"), "{error}");
        assert!(error.contains("4 tries"), "{error}");
    }
    assert!(matches!(
        events.last().unwrap(),
        Event::RunFinished {
            done: 1,
            failed: 1,
            ..
        }
    ));
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn permanent_llm_error_does_not_retry() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[("a1", "IMG_1.HEIC", None)]).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("")))
        .expect(1)
        .mount(&llm)
        .await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    let events = collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::AssetFailed { .. })));
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn immich_write_failure_marks_asset_failed() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[("a1", "IMG_1.HEIC", None)]).await;
    Mock::given(method("PUT"))
        .and(path("/api/assets/a1"))
        .respond_with(ResponseTemplate::new(500))
        .expect(4)
        .mount(&immich)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("ok")))
        .mount(&llm)
        .await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    let events = collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;
    assert!(events
        .iter()
        .any(|e| matches!(e, Event::AssetFailed { error, .. } if error.contains("immich"))));
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn pause_stops_new_assets_until_resume() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(
        &immich,
        &[("a1", "1", None), ("a2", "2", None), ("a3", "3", None)],
    )
    .await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/api/assets/[^/]+$"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&immich)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(completion("ok"))
                .set_delay(Duration::from_millis(200)),
        )
        .mount(&llm)
        .await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    collect_until(&mut rx, |e| matches!(e, Event::AssetStarted { .. })).await;
    handle.send(Command::Pause).await;
    collect_until(&mut rx, |e| matches!(e, Event::AssetDone { .. })).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    while let Ok(e) = rx.try_recv() {
        assert!(
            !matches!(e, Event::AssetStarted { .. }),
            "started while paused: {e:?}"
        );
    }

    handle.send(Command::Resume).await;
    let events = collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;
    assert!(matches!(
        events.last().unwrap(),
        Event::RunFinished {
            done: 3,
            failed: 0,
            ..
        }
    ));
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn unauthorized_immich_is_fatal_and_stops_the_run() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/search/metadata"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&immich)
        .await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    let e = next_event(&mut rx).await;
    assert!(
        matches!(&e, Event::Fatal { error } if error.contains("401")),
        "{e:?}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .is_err(),
        "no more events after Fatal"
    );
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn start_after_finish_runs_again() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[]).await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;
    handle.send(Command::Start).await;
    let events = collect_until(&mut rx, |e| matches!(e, Event::RunFinished { .. })).await;
    assert_eq!(
        events[0],
        Event::PageLoaded {
            scanned: 0,
            queued: 0
        }
    );
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn quit_stops_everything() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[("a1", "1", None)]).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(completion("ok"))
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&llm)
        .await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    collect_until(&mut rx, |e| matches!(e, Event::AssetStage { .. })).await;
    let started = std::time::Instant::now();
    handle.shutdown(Duration::from_secs(1)).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "shutdown must not wait for the slow LLM call"
    );
}

#[tokio::test]
async fn fatal_cancellation_stops_other_workers_in_writing_without_late_events() {
    const JPEG_A1: &[u8] = &[0xFF, 0xD8, 0xFF, 0xA1];
    const JPEG_A2: &[u8] = &[0xFF, 0xD8, 0xFF, 0xA2];

    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_search_page(
        &immich,
        1,
        10,
        &[("a1", "IMG_1.HEIC", None), ("a2", "IMG_2.HEIC", None)],
        None,
    )
    .await;
    mount_preview(&immich, "a1", JPEG_A1, Duration::ZERO, 200).await;
    mount_preview(&immich, "a2", JPEG_A2, Duration::from_millis(50), 200).await;
    Mock::given(method("PUT"))
        .and(path("/api/assets/a1"))
        .respond_with(
            ResponseTemplate::new(500)
                .set_delay(Duration::from_millis(200))
                .set_body_json(json!({})),
        )
        .expect(1)
        .mount(&immich)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains(
            base64::engine::general_purpose::STANDARD.encode(JPEG_A1),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("ok 1")))
        .expect(1)
        .mount(&llm)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains(
            base64::engine::general_purpose::STANDARD.encode(JPEG_A2),
        ))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&llm)
        .await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config_with_run(&immich, &llm, 2, 3, 10), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    collect_until(&mut rx, |event| {
        matches!(
            event,
            Event::AssetStage { id, stage } if id == "a1" && *stage == Stage::Writing
        )
    })
    .await;
    let events = collect_until(&mut rx, |event| matches!(event, Event::Fatal { .. })).await;

    assert!(events
        .iter()
        .any(|event| matches!(event, Event::Fatal { error } if error.contains("401"))));
    assert_no_matching_event(&mut rx, Duration::from_millis(350), |_| true).await;
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn quit_command_stops_writing_retries_without_late_events() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[("a1", "1", None)]).await;
    Mock::given(method("PUT"))
        .and(path("/api/assets/a1"))
        .respond_with(
            ResponseTemplate::new(500)
                .set_delay(Duration::from_millis(250))
                .set_body_json(json!({})),
        )
        .expect(1)
        .mount(&immich)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("ok")))
        .mount(&llm)
        .await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    collect_until(&mut rx, |event| {
        matches!(
            event,
            Event::AssetStage { id, stage } if id == "a1" && *stage == Stage::Writing
        )
    })
    .await;

    let started = std::time::Instant::now();
    handle.send(Command::Quit).await;
    tokio::time::timeout(Duration::from_millis(500), async {
        while let Some(event) = rx.recv().await {
            assert!(
                !matches!(event, _),
                "unexpected event after quit: {event:?}"
            );
        }
    })
    .await
    .expect("engine did not stop after quit");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "quit took too long to stop the engine"
    );
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn pause_prevents_new_asset_started_events_until_resume_with_multiple_workers() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(
        &immich,
        &[
            ("a1", "IMG_1.HEIC", None),
            ("a2", "IMG_2.HEIC", None),
            ("a3", "IMG_3.HEIC", None),
        ],
    )
    .await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/api/assets/[^/]+$"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&immich)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(completion("ok"))
                .set_delay(Duration::from_millis(150)),
        )
        .mount(&llm)
        .await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config_with_run(&immich, &llm, 2, 3, 10), tx, fast()).unwrap();
    handle.send(Command::Start).await;

    let mut started_names = Vec::new();
    while started_names.len() < 2 {
        let event = next_event(&mut rx).await;
        if let Event::AssetStarted { name, .. } = event {
            started_names.push(name);
        }
    }

    loop {
        let event = next_event(&mut rx).await;
        if matches!(event, Event::AssetDone { .. }) {
            handle.send(Command::Pause).await;
            break;
        }
    }
    let _buffered = drain_ready_events(&mut rx);

    assert_no_matching_event(&mut rx, Duration::from_millis(300), |event| {
        matches!(event, Event::AssetStarted { .. })
    })
    .await;

    handle.send(Command::Resume).await;
    let events = collect_until(&mut rx, |event| matches!(event, Event::RunFinished { .. })).await;
    assert!(matches!(
        events.last().unwrap(),
        Event::RunFinished {
            done: 3,
            failed: 0,
            ..
        }
    ));
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn discovery_follows_next_page_and_reports_cumulative_counts() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_search_page(
        &immich,
        1,
        2,
        &[
            ("a1", "IMG_1.HEIC", None),
            ("a2", "IMG_2.HEIC", Some("done")),
        ],
        Some(2),
    )
    .await;
    mount_search_page(
        &immich,
        2,
        2,
        &[
            ("a3", "IMG_3.HEIC", None),
            ("a4", "IMG_4.HEIC", Some("done")),
        ],
        None,
    )
    .await;
    mount_preview(&immich, "a1", JPEG, Duration::ZERO, 200).await;
    mount_preview(&immich, "a3", JPEG, Duration::ZERO, 200).await;
    Mock::given(method("PUT"))
        .and(path("/api/assets/a1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&immich)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/assets/a3"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&immich)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("ok")))
        .expect(2)
        .mount(&llm)
        .await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config_with_run(&immich, &llm, 2, 3, 2), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    let events = collect_until(&mut rx, |event| matches!(event, Event::RunFinished { .. })).await;

    let pages: Vec<Event> = events
        .iter()
        .filter(|event| matches!(event, Event::PageLoaded { .. }))
        .cloned()
        .collect();
    assert_eq!(
        pages,
        vec![
            Event::PageLoaded {
                scanned: 2,
                queued: 1
            },
            Event::PageLoaded {
                scanned: 4,
                queued: 2
            }
        ]
    );
    assert!(events
        .iter()
        .any(|event| matches!(event, Event::DiscoveryDone { total_queued: 2 })));
    assert!(matches!(
        events.last().unwrap(),
        Event::RunFinished {
            done: 2,
            failed: 0,
            ..
        }
    ));
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn start_after_paused_fatal_run_does_not_inherit_pause_state() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[("a1", "IMG_1.HEIC", None)]).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401))
        .up_to_n_times(1)
        .expect(1)
        .mount(&llm)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("ok")))
        .expect(1)
        .mount(&llm)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/assets/a1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&immich)
        .await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    collect_until(&mut rx, |event| matches!(event, Event::AssetStarted { .. })).await;
    handle.send(Command::Pause).await;
    collect_until(&mut rx, |event| matches!(event, Event::Fatal { .. })).await;
    assert_no_matching_event(&mut rx, Duration::from_millis(100), |_| true).await;

    let first = start_until_first_event(&handle, &mut rx).await;
    let mut events = vec![first];
    let more = tokio::time::timeout(Duration::from_secs(1), async {
        collect_until(&mut rx, |event| matches!(event, Event::RunFinished { .. })).await
    })
    .await
    .unwrap_or_else(|_| panic!("timed out after first restart event: {:?}", events[0]));
    events.extend(more);
    assert!(events
        .iter()
        .any(|event| matches!(event, Event::AssetStarted { name, .. } if name == "IMG_1.HEIC")));
    assert!(matches!(
        events.last().unwrap(),
        Event::RunFinished {
            done: 1,
            failed: 0,
            ..
        }
    ));
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn immediate_pause_after_start_is_not_lost() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[("a1", "IMG_1.HEIC", None)]).await;
    Mock::given(method("PUT"))
        .and(path("/api/assets/a1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&immich)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(completion("ok"))
                .set_delay(Duration::from_millis(100)),
        )
        .expect(1)
        .mount(&llm)
        .await;

    let (tx, mut rx) = mpsc::channel(256);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    handle.send(Command::Pause).await;

    let paused_events = collect_until(&mut rx, |event| {
        matches!(event, Event::DiscoveryDone { total_queued: 1 })
    })
    .await;
    assert!(
        !paused_events
            .iter()
            .any(|event| matches!(event, Event::AssetStarted { .. })),
        "pause was lost before discovery completed: {paused_events:?}"
    );
    assert_no_matching_event(&mut rx, Duration::from_millis(200), |event| {
        matches!(event, Event::AssetStarted { .. })
    })
    .await;

    handle.send(Command::Resume).await;
    let events = collect_until(&mut rx, |event| matches!(event, Event::RunFinished { .. })).await;
    assert!(events
        .iter()
        .any(|event| matches!(event, Event::AssetStarted { id, .. } if id == "a1")));
    assert!(matches!(
        events.last().unwrap(),
        Event::RunFinished {
            done: 1,
            failed: 0,
            ..
        }
    ));
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn quit_drops_blocked_non_terminal_events_after_cancellation() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    mount_immich_basics(&immich, &[("a1", "IMG_1.HEIC", None)]).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(completion("ok")))
        .expect(1)
        .mount(&llm)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/assets/a1"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&immich)
        .await;

    let (tx, mut rx) = mpsc::channel(1);
    let handle = engine::spawn_with(config(&immich, &llm), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    collect_until(&mut rx, |event| {
        matches!(
            event,
            Event::AssetStage { id, stage } if id == "a1" && *stage == Stage::Fetching
        )
    })
    .await;
    wait_for_request_count(&llm, 1).await;

    handle.send(Command::Quit).await;
    let buffered = next_event(&mut rx).await;
    assert!(matches!(
        buffered,
        Event::AssetStage { id, stage } if id == "a1" && stage == Stage::CallingLlm
    ));
    assert_no_matching_event(&mut rx, Duration::from_millis(300), |_| true).await;
    handle.shutdown(Duration::from_secs(1)).await;
}

#[tokio::test]
async fn fatal_cancellation_wins_over_saturated_non_terminal_events() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;
    let fatal_response = Arc::new(Notify::new());
    mount_search_page(&immich, 1, 1, &[("a1", "IMG_1.HEIC", None)], Some(2)).await;
    Mock::given(method("POST"))
        .and(path("/api/search/metadata"))
        .and(body_json(json!({
            "type": "IMAGE",
            "withExif": true,
            "size": 1,
            "page": 2,
            "order": "desc",
        })))
        .respond_with(NotifyResponder {
            notify: fatal_response.clone(),
            status: 401,
        })
        .expect(1)
        .mount(&immich)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/assets/a1/thumbnail"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(JPEG.to_vec()))
        .expect(0)
        .mount(&immich)
        .await;

    let (tx, mut rx) = mpsc::channel(1);
    let handle = engine::spawn_with(config_with_run(&immich, &llm, 1, 3, 1), tx, fast()).unwrap();
    handle.send(Command::Start).await;
    fatal_response.notified().await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    let first = next_event(&mut rx).await;
    assert_eq!(
        first,
        Event::PageLoaded {
            scanned: 1,
            queued: 1
        }
    );

    let fatal = next_event(&mut rx).await;
    assert!(
        matches!(fatal, Event::Fatal { ref error } if error.contains("401")),
        "expected Fatal after cancellation won the race, got {fatal:?}"
    );
    assert_no_matching_event(&mut rx, Duration::from_millis(200), |event| {
        !matches!(event, Event::Fatal { .. })
    })
    .await;
    handle.shutdown(Duration::from_secs(1)).await;
}
