use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use hudson_core::{fixtures, models::now, storage::Store};
use serde_json::{json, Value};
use tower::ServiceExt;

fn fixture() -> (axum::Router, Store) {
    let runtime = fixtures::runtime().unwrap();
    let store = runtime.store.clone();
    let app = crate::routes::assemble(
        crate::routes::Driver::Demo(runtime),
        store.clone(),
        fixtures::actor(),
        Some(fixtures::agent_ref()),
        None,
        false,
        "fixture",
    );
    (app, store)
}
async fn completed(app: &axum::Router) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/runs")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"input":{"order_id":"123","action":"lookup"}}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    let id = value["run_id"].as_str().unwrap().to_owned();
    for _ in 0..100 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/runs/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let view: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        if view["status"] == "completed" {
            return id;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("fixture did not complete");
}
#[tokio::test]
async fn streams_committed_events_and_resumes_after_last_event_id() {
    let (app, store) = fixture();
    let id = completed(&app).await;
    let path = format!("/runs/{id}/events/stream");
    let response = app
        .clone()
        .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(response.headers()[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    let bytes = tokio::time::timeout(
        Duration::from_secs(3),
        to_bytes(response.into_body(), 1024 * 1024),
    )
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let ids = text
        .lines()
        .filter_map(|s| s.strip_prefix("id:"))
        .map(|s| s.trim().parse::<u64>().unwrap())
        .collect::<Vec<_>>();
    let events = store
        .event_page(&fixtures::actor(), id.parse().unwrap(), 0, 1000)
        .unwrap();
    assert_eq!(ids, events.iter().map(|e| e.sequence).collect::<Vec<_>>());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&path)
                .header("last-event-id", "2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = tokio::time::timeout(
        Duration::from_secs(3),
        to_bytes(response.into_body(), 1024 * 1024),
    )
    .await
    .unwrap()
    .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.lines().filter_map(|s| s.strip_prefix("id:")).all(|s| s
        .trim()
        .parse::<u64>()
        .unwrap()
        > 2));
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("{path}?after=3"))
                .header("last-event-id", "2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
}
#[tokio::test]
async fn stream_rechecks_expiry_and_revocation_before_delivering_buffered_events() {
    let (app, store) = fixture();
    let id = completed(&app).await;
    let actor = fixtures::actor();
    let token = store
        .issue_api_token(actor.clone(), "stream-test".into(), now() + 60_000)
        .unwrap();
    let app = crate::auth::protect(app, store.clone(), actor.clone());
    let path = format!("/runs/{id}/events/stream");
    let response = app
        .clone()
        .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    let response = app
        .oneshot(
            Request::builder()
                .uri(&path)
                .header(header::AUTHORIZATION, format!("Bearer {}", token.bearer()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    store.revoke_api_token(&actor, token.metadata.id).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let bytes = tokio::time::timeout(
        Duration::from_secs(2),
        to_bytes(response.into_body(), 65536),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(bytes.is_empty());
}

#[tokio::test]
async fn websocket_delivers_journal_over_a_real_connection_and_resumes() {
    let (app, store) = fixture();
    let id = completed(&app).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    struct Server(tokio::task::JoinHandle<()>);
    impl Drop for Server {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _server = Server(server);
    let expected = store
        .event_page(&fixtures::actor(), id.parse().unwrap(), 2, 1000)
        .unwrap();
    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{address}/runs/{id}/events/ws?after=2"))
            .await
            .unwrap();
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(message) = socket.next().await {
            match message.unwrap() {
                tokio_tungstenite::tungstenite::Message::Text(text) => {
                    received.push(serde_json::from_str::<StoredEvent>(&text).unwrap().sequence);
                }
                tokio_tungstenite::tungstenite::Message::Close(_) => break,
                other => panic!("unexpected websocket message: {other:?}"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        received,
        expected.iter().map(|e| e.sequence).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn connection_limits_release_on_drop_and_do_not_grant_run_access() {
    let (app, _) = fixture();
    let id = completed(&app).await;
    let path = format!("/runs/{id}/events/stream");
    let foreign = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/runs/{}/events/stream", Uuid::new_v4()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(foreign.status(), axum::http::StatusCode::NOT_FOUND);
    let mut open = Vec::new();
    for _ in 0..32 {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        open.push(response);
    }
    let response = app
        .clone()
        .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    drop(open.pop());
    let response = app
        .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
}
