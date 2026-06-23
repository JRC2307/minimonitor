use fleet::agent_client::AgentClient;
use std::time::Duration;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FIXTURE: &str = include_str!("fixtures/snapshot.json");

fn client() -> AgentClient {
    AgentClient::new(Duration::from_secs(5))
}

/// 200 serving the fixture → Ok((raw, snap)) with raw non-empty + snap.ports populated.
#[tokio::test]
async fn test_200_returns_raw_and_snap() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/snapshot"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(FIXTURE.as_bytes().to_vec(), "application/json"),
        )
        .mount(&server)
        .await;

    let (raw, snap) = client()
        .fetch_snapshot(&server.uri(), None)
        .await
        .expect("expected Ok");

    assert!(!raw.is_empty(), "raw bytes must be non-empty");
    assert!(!snap.ports.is_empty(), "snap.ports must be populated");
}

/// 500 → Err; assert "HTTP 500" appears in the error string.
#[tokio::test]
async fn test_500_returns_err_with_http_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/snapshot"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let result = client().fetch_snapshot(&server.uri(), None).await;
    let err = result.err().expect("expected Err on 500");
    let msg = err.to_string();
    assert!(
        msg.contains("HTTP 500"),
        "error must mention HTTP 500, got: {msg}"
    );
}

/// token = Some("t") → Authorization: Bearer t header is required by the mock.
#[tokio::test]
async fn test_bearer_token_sent_when_some() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/snapshot"))
        .and(header("Authorization", "Bearer t"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(FIXTURE.as_bytes().to_vec(), "application/json"),
        )
        .mount(&server)
        .await;

    // With the token — the header-matched mock must be satisfied.
    client()
        .fetch_snapshot(&server.uri(), Some("t"))
        .await
        .expect("expected Ok with bearer token");
}

/// token = None → no Authorization header required; mock must still respond 200.
#[tokio::test]
async fn test_no_bearer_when_none() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/snapshot"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(FIXTURE.as_bytes().to_vec(), "application/json"),
        )
        .mount(&server)
        .await;

    client()
        .fetch_snapshot(&server.uri(), None)
        .await
        .expect("expected Ok with no token");
}

/// Malformed body → Err with decode context.
#[tokio::test]
async fn test_malformed_body_returns_decode_err() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/snapshot"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(b"not json".to_vec(), "application/json"),
        )
        .mount(&server)
        .await;

    let result = client().fetch_snapshot(&server.uri(), None).await;
    let err = result.err().expect("expected Err on malformed body");
    let msg = err.to_string();
    assert!(
        msg.contains("decoding MonitorSnapshot") || msg.contains("expected"),
        "error must contain decode context, got: {msg}"
    );
}

/// Trailing slash on base_url is trimmed — URL becomes /snapshot not //snapshot.
#[tokio::test]
async fn test_trailing_slash_trimmed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/snapshot"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(FIXTURE.as_bytes().to_vec(), "application/json"),
        )
        .mount(&server)
        .await;

    let base_with_slash = format!("{}/", server.uri());
    client()
        .fetch_snapshot(&base_with_slash, None)
        .await
        .expect("trailing slash must be trimmed");
}
