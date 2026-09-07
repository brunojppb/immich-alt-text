//! Typed client for the parts of the Immich API this tool uses.
//! All paths sit under `/api`. Auth is the `x-api-key` header.

use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ImmichError {
    /// Network or server trouble that may pass. Worth a retry.
    #[error("immich: {0}")]
    Transient(String),
    /// Wrong for this one asset. Skip it.
    #[error("immich: {0}")]
    Permanent(String),
    /// Wrong for the whole run: bad key or bad server.
    #[error("immich: {0}")]
    Fatal(String),
}

/// What Immich already knows about a photo, ready for the prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssetContext {
    /// Named people, hidden people removed.
    pub people: Vec<String>,
    /// City, state, and country, in that order.
    pub place: Vec<String>,
    /// Local wall-clock time the camera recorded.
    pub taken: Option<chrono::NaiveDateTime>,
}

const MAX_PEOPLE: usize = 6;

/// One photo as the engine sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub context: AssetContext,
}

impl Asset {
    /// True when the description is missing or only whitespace.
    pub fn needs_description(&self) -> bool {
        self.description
            .as_deref()
            .is_none_or(|description| description.trim().is_empty())
    }
}

/// One page of search results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub items: Vec<Asset>,
    pub next_page: Option<u32>,
}

#[derive(Clone)]
pub struct ImmichClient {
    http: reqwest::Client,
    base: String,
    api_key: String,
}

impl ImmichClient {
    /// `url` is the server root, with or without a trailing slash.
    pub fn new(url: &str, api_key: &str, timeout: Duration) -> Result<Self, ImmichError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| ImmichError::Fatal(format!("http client: {error}")))?;

        Ok(Self {
            http,
            base: format!("{}/api", url.trim_end_matches('/')),
            api_key: api_key.to_string(),
        })
    }

    /// Adds auth, sends, logs method, path, status, and duration, then maps the status.
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, ImmichError> {
        let req = req
            .header("x-api-key", &self.api_key)
            .build()
            .map_err(|error| ImmichError::Permanent(format!("bad request: {error}")))?;
        let method = req.method().clone();
        let path = req.url().path().to_string();
        let started = std::time::Instant::now();
        let resp = self.http.execute(req).await.map_err(transport)?;

        tracing::debug!(
            %method,
            %path,
            status = %resp.status(),
            ms = started.elapsed().as_millis() as u64,
            "immich"
        );

        check_status(resp).await
    }

    /// Server version as `vMAJOR.MINOR.PATCH`. Also proves the key works.
    pub async fn version(&self) -> Result<String, ImmichError> {
        #[derive(Deserialize)]
        struct Version {
            major: u32,
            minor: u32,
            patch: u32,
        }

        let resp = self
            .send(self.http.get(format!("{}/server/version", self.base)))
            .await?;
        let version: Version = resp.json().await.map_err(bad_body)?;
        Ok(format!(
            "v{}.{}.{}",
            version.major, version.minor, version.patch
        ))
    }

    /// One page of images, newest first, with EXIF so the description is present.
    pub async fn list_images(&self, page: u32, size: u32) -> Result<Page, ImmichError> {
        let body = serde_json::json!({
            "type": "IMAGE",
            "withExif": true,
            "withPeople": true,
            "size": size,
            "page": page,
            "order": "desc",
        });
        let resp = self
            .send(
                self.http
                    .post(format!("{}/search/metadata", self.base))
                    .json(&body),
            )
            .await?;
        let parsed: SearchResponse = resp.json().await.map_err(bad_body)?;
        let items = parsed
            .assets
            .items
            .into_iter()
            .map(|asset| {
                let place = place_parts(asset.exif_info.as_ref());
                let people = people_names(asset.people.unwrap_or_default());
                let taken = taken_at(asset.local_date_time.as_deref());
                Asset {
                    id: asset.id,
                    name: asset.original_file_name,
                    description: asset.exif_info.and_then(|exif| exif.description),
                    context: AssetContext {
                        people,
                        place,
                        taken,
                    },
                }
            })
            .collect();
        let next_page =
            match parsed.assets.next_page {
                Some(page) => Some(page.parse().map_err(|error| {
                    ImmichError::Permanent(format!("bad nextPage value: {error}"))
                })?),
                None => None,
            };

        Ok(Page { items, next_page })
    }

    /// The `preview` rendition as JPEG bytes.
    pub async fn preview_jpeg(&self, id: &str) -> Result<Vec<u8>, ImmichError> {
        let resp = self
            .send(
                self.http
                    .get(format!("{}/assets/{id}/thumbnail?size=preview", self.base)),
            )
            .await?;
        let bytes = resp.bytes().await.map_err(transport)?;
        if bytes.is_empty() {
            return Err(ImmichError::Permanent("empty preview body".into()));
        }

        Ok(bytes.to_vec())
    }

    /// Sets the asset description. Overwrites whatever is there.
    pub async fn set_description(&self, id: &str, text: &str) -> Result<(), ImmichError> {
        self.send(
            self.http
                .put(format!("{}/assets/{id}", self.base))
                .json(&serde_json::json!({ "description": text })),
        )
        .await
        .map(|_| ())
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    assets: SearchAssets,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchAssets {
    items: Vec<AssetDto>,
    next_page: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssetDto {
    id: String,
    original_file_name: String,
    exif_info: Option<ExifDto>,
    people: Option<Vec<PersonDto>>,
    local_date_time: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersonDto {
    name: Option<String>,
    is_hidden: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExifDto {
    description: Option<String>,
    city: Option<String>,
    state: Option<String>,
    country: Option<String>,
}

fn people_names(people: Vec<PersonDto>) -> Vec<String> {
    people
        .into_iter()
        .filter(|person| !person.is_hidden.unwrap_or(false))
        .filter_map(|person| {
            let name = person.name.unwrap_or_default().trim().to_string();
            (!name.is_empty()).then_some(name)
        })
        .take(MAX_PEOPLE)
        .collect()
}

fn place_parts(exif: Option<&ExifDto>) -> Vec<String> {
    let mut parts = Vec::new();
    let Some(exif) = exif else {
        return parts;
    };
    for value in [&exif.city, &exif.state, &exif.country] {
        let Some(value) = value.as_deref().map(str::trim) else {
            continue;
        };
        if value.is_empty()
            || parts
                .last()
                .is_some_and(|last: &String| last.eq_ignore_ascii_case(value))
        {
            continue;
        }
        parts.push(value.to_string());
    }
    parts
}

/// Parses Immich's local date-time string into a naive date and time.
fn taken_at(raw: Option<&str>) -> Option<chrono::NaiveDateTime> {
    let raw = raw?;
    // The offset is part of the wall-clock value Immich sends, so drop it
    // instead of converting to UTC.
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(parsed.naive_local());
    }
    chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f").ok()
}

fn transport(error: reqwest::Error) -> ImmichError {
    ImmichError::Transient(error.to_string())
}

fn bad_body(error: reqwest::Error) -> ImmichError {
    ImmichError::Permanent(format!("bad response body: {error}"))
}

async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, ImmichError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }

    let message = format!("HTTP {status}");
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            Err(ImmichError::Fatal(format!("{message}: check the API key")))
        }
        StatusCode::TOO_MANY_REQUESTS => Err(ImmichError::Transient(message)),
        server if server.is_server_error() => Err(ImmichError::Transient(message)),
        _ => Err(ImmichError::Permanent(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use serde_json::json;
    use wiremock::matchers::{body_json, body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn client(server: &MockServer) -> ImmichClient {
        ImmichClient::new(&server.uri(), "k", Duration::from_secs(5)).unwrap()
    }

    #[tokio::test]
    async fn version_reads_semver() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/server/version"))
            .and(header("x-api-key", "k"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "major": 3, "minor": 1, "patch": 0
            })))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(client(&server).await.version().await.unwrap(), "v3.1.0");
    }

    #[tokio::test]
    async fn trailing_slash_in_url_is_tolerated() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/server/version"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "major": 3, "minor": 2, "patch": 0
            })))
            .mount(&server)
            .await;
        let c =
            ImmichClient::new(&format!("{}/", server.uri()), "k", Duration::from_secs(5)).unwrap();
        assert_eq!(c.version().await.unwrap(), "v3.2.0");
    }

    #[tokio::test]
    async fn list_images_sends_search_body_and_parses_page() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/search/metadata"))
            .and(header("x-api-key", "k"))
            .and(body_json(json!({
                "type": "IMAGE", "withExif": true, "withPeople": true, "size": 2, "page": 1, "order": "desc"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "albums": { "count": 0, "items": [], "facets": [], "total": 0 },
                "assets": {
                    "count": 2, "total": 5, "facets": [], "nextPage": "2",
                    "items": [
                        { "id": "a1", "originalFileName": "IMG_1.HEIC", "type": "IMAGE",
                          "exifInfo": { "description": null } },
                        { "id": "a2", "originalFileName": "IMG_2.HEIC", "type": "IMAGE",
                          "exifInfo": { "description": "a dog" } }
                    ]
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let page = client(&server).await.list_images(1, 2).await.unwrap();
        assert_eq!(page.next_page, Some(2));
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].id, "a1");
        assert_eq!(page.items[0].name, "IMG_1.HEIC");
        assert!(page.items[0].needs_description());
        assert!(!page.items[1].needs_description());
    }

    #[tokio::test]
    async fn list_images_invalid_next_page_is_permanent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/search/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "assets": {
                    "count": 1, "total": 2, "facets": [], "nextPage": "nope",
                    "items": [
                        { "id": "a1", "originalFileName": "x.jpg", "type": "IMAGE",
                          "exifInfo": { "description": null } }
                    ]
                }
            })))
            .mount(&server)
            .await;

        let err = client(&server).await.list_images(1, 1).await.unwrap_err();
        assert!(matches!(err, ImmichError::Permanent(_)), "{err}");
        assert!(err.to_string().contains("nextPage"));
    }

    #[tokio::test]
    async fn list_images_handles_last_page_and_missing_exif() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/search/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "assets": { "count": 1, "total": 1, "facets": [], "nextPage": null,
                    "items": [ { "id": "a3", "originalFileName": "x.jpg", "type": "IMAGE" } ] }
            })))
            .mount(&server)
            .await;
        let page = client(&server).await.list_images(3, 1000).await.unwrap();
        assert_eq!(page.next_page, None);
        assert!(page.items[0].needs_description());
    }

    #[tokio::test]
    async fn blank_description_needs_a_new_one() {
        let a = Asset {
            id: "x".into(),
            name: "x".into(),
            description: Some("   ".into()),
            context: AssetContext::default(),
        };
        assert!(a.needs_description());
    }

    #[tokio::test]
    async fn preview_jpeg_requests_preview_size() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/assets/a1/thumbnail"))
            .and(query_param("size", "preview"))
            .and(header("x-api-key", "k"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xFF, 0xD8, 0xFF]))
            .expect(1)
            .mount(&server)
            .await;
        let bytes = client(&server).await.preview_jpeg("a1").await.unwrap();
        assert_eq!(bytes, vec![0xFF, 0xD8, 0xFF]);
    }

    #[tokio::test]
    async fn empty_preview_is_permanent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/assets/a1/thumbnail"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(Vec::<u8>::new()))
            .mount(&server)
            .await;
        let err = client(&server).await.preview_jpeg("a1").await.unwrap_err();
        assert!(matches!(err, ImmichError::Permanent(_)), "{err}");
    }

    #[tokio::test]
    async fn set_description_puts_json() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/assets/a1"))
            .and(header("x-api-key", "k"))
            .and(body_json(json!({ "description": "A dog on a dock." })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "a1" })))
            .expect(1)
            .mount(&server)
            .await;
        client(&server)
            .await
            .set_description("a1", "A dog on a dock.")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn unauthorized_is_fatal() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/server/version"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let err = client(&server).await.version().await.unwrap_err();
        assert!(matches!(err, ImmichError::Fatal(_)), "{err}");
        assert!(err.to_string().contains("401"));
    }

    #[tokio::test]
    async fn forbidden_is_fatal() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/server/version"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let err = client(&server).await.version().await.unwrap_err();
        assert!(matches!(err, ImmichError::Fatal(_)), "{err}");
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn rate_limited_is_transient() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/assets/a1"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let err = client(&server)
            .await
            .set_description("a1", "x")
            .await
            .unwrap_err();
        assert!(matches!(err, ImmichError::Transient(_)), "{err}");
        assert!(err.to_string().contains("429"));
    }

    #[tokio::test]
    async fn server_error_is_transient() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/assets/a1"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let err = client(&server)
            .await
            .set_description("a1", "x")
            .await
            .unwrap_err();
        assert!(matches!(err, ImmichError::Transient(_)), "{err}");
    }

    #[tokio::test]
    async fn not_found_is_permanent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/assets/gone/thumbnail"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let err = client(&server)
            .await
            .preview_jpeg("gone")
            .await
            .unwrap_err();
        assert!(matches!(err, ImmichError::Permanent(_)), "{err}");
    }

    #[tokio::test]
    async fn malformed_body_is_permanent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/search/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let err = client(&server).await.list_images(1, 10).await.unwrap_err();
        assert!(matches!(err, ImmichError::Permanent(_)), "{err}");
    }

    #[tokio::test]
    async fn request_timeout_is_transient() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/server/version"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(200)))
            .expect(1)
            .mount(&server)
            .await;
        let c = ImmichClient::new(&server.uri(), "k", Duration::from_millis(20)).unwrap();
        let err = c.version().await.unwrap_err();
        assert!(matches!(err, ImmichError::Transient(_)), "{err}");
    }

    #[tokio::test]
    async fn the_search_request_asks_for_people() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/search/metadata"))
            .and(body_partial_json(serde_json::json!({ "withPeople": true })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "assets": { "items": [], "nextPage": null }
            })))
            .mount(&server)
            .await;

        let client = ImmichClient::new(&server.uri(), "key", Duration::from_secs(5)).unwrap();
        client.list_images(1, 10).await.unwrap();
    }

    #[tokio::test]
    async fn a_page_fills_the_asset_context() {
        let server = MockServer::start().await;
        let items = serde_json::json!({
            "assets": {
                "items": [{
                    "id": "a1",
                    "originalFileName": "IMG_1.jpg",
                    "localDateTime": "2019-06-14T19:23:00.000Z",
                    "people": [
                        { "name": "Ana", "isHidden": false },
                        { "name": "Secret", "isHidden": true },
                        { "name": "   ", "isHidden": false },
                        { "name": "Marco", "isHidden": false }
                    ],
                    "exifInfo": {
                        "description": null,
                        "city": "Sintra",
                        "state": "Lisbon",
                        "country": "Portugal"
                    }
                }],
                "nextPage": null
            }
        });
        Mock::given(method("POST"))
            .and(path("/api/search/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(items))
            .mount(&server)
            .await;

        let client = ImmichClient::new(&server.uri(), "key", Duration::from_secs(5)).unwrap();
        let page = client.list_images(1, 10).await.unwrap();
        let context = &page.items[0].context;
        assert_eq!(context.people, vec!["Ana".to_string(), "Marco".to_string()]);
        assert_eq!(
            context.place,
            vec![
                "Sintra".to_string(),
                "Lisbon".to_string(),
                "Portugal".to_string()
            ]
        );
        assert_eq!(
            context.taken.map(|taken| taken.to_string()),
            Some("2019-06-14 19:23:00".to_string())
        );
    }

    #[tokio::test]
    async fn the_context_survives_missing_metadata() {
        let server = MockServer::start().await;
        let items = serde_json::json!({
            "assets": {
                "items": [{ "id": "a1", "originalFileName": "IMG_1.jpg" }],
                "nextPage": null
            }
        });
        Mock::given(method("POST"))
            .and(path("/api/search/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(items))
            .mount(&server)
            .await;

        let client = ImmichClient::new(&server.uri(), "key", Duration::from_secs(5)).unwrap();
        let page = client.list_images(1, 10).await.unwrap();
        assert_eq!(page.items[0].context, AssetContext::default());
    }

    #[tokio::test]
    async fn an_explicit_null_people_list_is_an_empty_context() {
        let server = MockServer::start().await;
        let items = serde_json::json!({
            "assets": {
                "items": [{ "id": "a1", "originalFileName": "IMG_1.jpg", "people": null }],
                "nextPage": null
            }
        });
        Mock::given(method("POST"))
            .and(path("/api/search/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(items))
            .mount(&server)
            .await;

        let client = ImmichClient::new(&server.uri(), "key", Duration::from_secs(5)).unwrap();
        let page = client.list_images(1, 10).await.unwrap();
        assert_eq!(page.items[0].context, AssetContext::default());
    }

    #[tokio::test]
    async fn an_explicit_null_is_hidden_keeps_the_person() {
        let server = MockServer::start().await;
        let items = serde_json::json!({
            "assets": {
                "items": [{
                    "id": "a1",
                    "originalFileName": "IMG_1.jpg",
                    "people": [{ "name": "Ana", "isHidden": null }]
                }],
                "nextPage": null
            }
        });
        Mock::given(method("POST"))
            .and(path("/api/search/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(items))
            .mount(&server)
            .await;

        let client = ImmichClient::new(&server.uri(), "key", Duration::from_secs(5)).unwrap();
        let page = client.list_images(1, 10).await.unwrap();
        assert_eq!(page.items[0].context.people, vec!["Ana".to_string()]);
    }

    #[test]
    fn seven_people_produce_six_names() {
        let people: Vec<PersonDto> = (0..7)
            .map(|i| PersonDto {
                name: Some(format!("P{i}")),
                is_hidden: Some(false),
            })
            .collect();
        assert_eq!(
            people_names(people),
            vec!["P0", "P1", "P2", "P3", "P4", "P5"]
        );
    }

    #[test]
    fn a_repeated_place_part_appears_once() {
        let exif = ExifDto {
            description: None,
            city: Some("Lisbon".into()),
            state: Some("Lisbon".into()),
            country: Some("Portugal".into()),
        };
        assert_eq!(
            place_parts(Some(&exif)),
            vec!["Lisbon".to_string(), "Portugal".to_string()]
        );
    }

    #[test]
    fn a_repeated_place_part_with_different_case_appears_once() {
        let exif = ExifDto {
            description: None,
            city: Some("Lisbon".into()),
            state: None,
            country: Some("lisbon".into()),
        };
        assert_eq!(place_parts(Some(&exif)), vec!["Lisbon".to_string()]);
    }

    #[test]
    fn an_unparsable_date_is_none() {
        assert!(taken_at(Some("not a date")).is_none());
        assert!(taken_at(None).is_none());
        assert_eq!(
            taken_at(Some("2019-06-14T19:23:00")).map(|t| t.to_string()),
            Some("2019-06-14 19:23:00".to_string())
        );
    }
}
