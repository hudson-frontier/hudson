use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

struct Model(Arc<AtomicUsize>);
impl ModelExecutor for Model {
    fn call(&mut self, _: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ModelResponse::Final { output: json!(42) })
    }
    fn take_usage(&mut self) -> Option<TokenUsage> {
        Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 0,
        })
    }
}
async fn request(app: &Router, method: &str, path: &str, value: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(value.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn model_spend_quote_is_visible_before_approval_and_receipts_remain_owner_scoped() {
    let (app,mut runtime,actor,id,calls)=tokio::task::spawn_blocking(|| {
        let path=std::env::temp_dir().join(format!("hudson-spend-{}.json",Uuid::new_v4()));
        std::fs::write(&path,json!({"name":"spend-api","instructions":"return 42","provider":"ollama","model":"fixture",
            "spend":{"allocation_nano_usd":100,"rates":{"input_nano_usd_per_million_tokens":1000000,"output_nano_usd_per_million_tokens":0},
                "quotas":[{"id":"api","scope":"workspace","limit_nano_usd":1000,"soft_nano_usd":1,"approvers":["developer"]}]}}).to_string()).unwrap();
        let config=hudson_core::configured::Configuration::load(&path).unwrap();std::fs::remove_file(path).unwrap();
        let store=Store::default();let actor=fixtures::actor();let tree=config.build_temporal_tree(store.clone(),&actor).unwrap();
        let target=hudson_core::scheduling::ScheduleTarget {scheduler:"temporal:spend-api".into(),task_queue:"spend-api".into()};
        let calls=Arc::new(AtomicUsize::new(0));let mut runtime=Runtime::new(store,AgentLoop,Model(calls.clone()),ToolRegistry::new());
        let id=runtime.submit_scheduled(&actor,tree.reference.clone(),json!("task"),Some("one".into()),None,Some(target.clone())).unwrap();
        fixtures::drive(&mut runtime,&actor,id).unwrap();
        let app=super::scheduled(tree,actor.clone(),target).unwrap();(app,runtime,actor,id,calls)
    }).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let (status, spend) = request(&app, "GET", &format!("/runs/{id}/spend"), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(spend["enabled"], true);
    assert_eq!(spend["entries"][0]["allocation_nano_usd"], "100");
    assert!(spend["entries"][0]["priced_usage_nano_usd"].is_null());
    let operation = spend["entries"][0]["operation_id"].as_str().unwrap();
    let (status, quote) = request(
        &app,
        "GET",
        &format!("/operations/{operation}/spend"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(quote["status"], "reserved");
    let (status, _) = request(
        &app,
        "POST",
        &format!("/operations/{operation}/approval"),
        json!({"approved":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let view =
        tokio::task::spawn_blocking(move || fixtures::drive(&mut runtime, &actor, id).unwrap())
            .await
            .unwrap();
    assert_eq!(view.status, RunStatus::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let (_, spend) = request(&app, "GET", &format!("/runs/{id}/spend"), Value::Null).await;
    assert_eq!(spend["entries"][0]["priced_usage_nano_usd"], "10");
}
