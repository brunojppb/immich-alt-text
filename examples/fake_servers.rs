//! Fake Immich and LLM servers for a manual run of the TUI.
//!
//! Terminal 1: `cargo run --example fake_servers`
//! Terminal 2: `cargo run -- --config target/demo-config.toml`

use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::main]
async fn main() {
    let immich = MockServer::start().await;
    let llm = MockServer::start().await;

    let items: Vec<serde_json::Value> = (1..=40)
        .map(|i| {
            let description = if i % 7 == 0 {
                Some("already described")
            } else {
                None
            };
            json!({
                "id": format!("asset-{i:03}"),
                "originalFileName": format!("IMG_{:04}.HEIC", 4400 + i),
                "type": "IMAGE",
                "exifInfo": { "description": description }
            })
        })
        .collect();

    Mock::given(method("GET"))
        .and(path("/api/server/version"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "major": 3, "minor": 1, "patch": 0 })),
        )
        .mount(&immich)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/search/metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "assets": { "count": items.len(), "total": items.len(), "facets": [], "nextPage": null, "items": items }
        })))
        .mount(&immich)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/assets/[^/]+/thumbnail$"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xFF, 0xD8, 0xFF, 0xD9]))
        .mount(&immich)
        .await;
    // A few writes fail so the failed counter and the red log rows show up.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/api/assets/asset-0(09|18|27|36)$"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&immich)
        .await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/api/assets/[^/]+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&immich)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "data": [ { "id": "demo-vision" } ] })),
        )
        .mount(&llm)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(1500))
                .set_body_json(json!({ "choices": [ { "index": 0, "message": {
                    "role": "assistant",
                    "content": "A golden retriever sits on a wooden dock at sunset, looking toward the water."
                } } ] })),
        )
        .mount(&llm)
        .await;

    let config = format!(
        "[immich]\nurl = \"{}\"\napi_key = \"demo\"\ntimeout_secs = 5\n\n\
         [llm]\nbase_url = \"{}/v1\"\nmodel = \"demo-vision\"\ntimeout_secs = 10\n\n\
         [run]\nworkers = 2\nretries = 1\n",
        immich.uri(),
        llm.uri()
    );
    std::fs::create_dir_all("target").expect("create target dir");
    std::fs::write("target/demo-config.toml", config).expect("write demo config");

    println!("fake immich: {}", immich.uri());
    println!("fake llm:    {}", llm.uri());
    println!("config:      target/demo-config.toml");
    println!("now run:     cargo run -- --config target/demo-config.toml");
    println!("ctrl-c stops the servers");
    tokio::signal::ctrl_c().await.expect("ctrl-c handler");
}
