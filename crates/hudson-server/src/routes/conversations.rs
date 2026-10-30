use super::*;
use hudson_core::conversations::{ImportedTurn, Transcript, MAX_TRANSCRIPT_BYTES};
pub(super) const MAX_IMPORT_BYTES: usize = 5 * 1024 * 1024;

pub(super) async fn gate(
    State(slots): State<Arc<tokio::sync::Semaphore>>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Ok(permit) = slots.try_acquire_owned() else {
        return ApiError(Error::Conflict(
            "conversation import concurrency limit reached".into(),
        ))
        .into_response();
    };
    request.extensions_mut().insert(Arc::new(permit));
    match tokio::time::timeout(std::time::Duration::from_secs(30), next.run(request)).await {
        Ok(response) => response,
        Err(_) => ApiError(Error::Conflict(
            "conversation import timed out; retain the request key before retrying".into(),
        ))
        .into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Import {
    agent_ref: Option<hudson_core::customer::Reference>,
    input: Value,
    request_key: String,
    transcript: Transcript,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Fork {
    agent_ref: Option<hudson_core::customer::Reference>,
    input: Value,
    request_key: String,
}
pub(super) async fn export(
    State(app): State<App>,
    Path(id): Path<Uuid>,
) -> Result<Json<Transcript>, ApiError> {
    blocking(move || Ok(Json(app.store.export_transcript(&app.actor, id)?))).await
}
fn import(
    app: &App,
    body: Import,
    source_run: Option<Uuid>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if app.fixture {
        return Err(ApiError(Error::Unsupported(
            "conversation transfer requires a configured agent".into(),
        )));
    }
    body.transcript.validate(MAX_TRANSCRIPT_BYTES)?;
    let turn = ImportedTurn {
        transcript: body.transcript,
        input: body.input,
        request_key: body.request_key,
        goal: app.goal.clone(),
        source_run,
    };
    let id = if app.catalog.is_some() {
        let reference = body
            .agent_ref
            .ok_or_else(|| ApiError(Error::Invalid("agent_ref is required".into())))?;
        app.store.import_published_turn(
            &app.actor,
            &reference.version_ref(),
            turn,
            app.schedule.clone().ok_or_else(unavailable)?,
        )?
    } else {
        if body.agent_ref.is_some() {
            return Err(ApiError(Error::Invalid(
                "agent_ref requires published mode".into(),
            )));
        }
        app.lock()?.import_turn(
            &app.actor,
            app.agent.clone().ok_or_else(unavailable)?,
            turn,
            app.schedule.clone(),
        )?
    };
    app.drive(id)?;
    Ok((StatusCode::ACCEPTED, Json(json!({"run_id":id}))))
}
pub(super) async fn import_turn(
    State(app): State<App>,
    Extension(permit): Extension<Arc<tokio::sync::OwnedSemaphorePermit>>,
    Json(body): Json<Import>,
) -> Result<impl IntoResponse, ApiError> {
    blocking(move || {
        let _permit = permit;
        import(&app, body, None)
    })
    .await
}
pub(super) async fn fork(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    Json(body): Json<Fork>,
) -> Result<impl IntoResponse, ApiError> {
    blocking(move || {
        let transcript = app.store.export_transcript(&app.actor, id)?;
        import(
            &app,
            Import {
                agent_ref: body.agent_ref,
                input: body.input,
                request_key: body.request_key,
                transcript,
            },
            Some(id),
        )
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    struct Final;
    impl ModelExecutor for Final {
        fn call(&mut self, _: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
            Ok(ModelResponse::Final { output: json!(42) })
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
        let bytes = to_bytes(response.into_body(), 6 * 1024 * 1024)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    #[tokio::test]
    async fn api_exports_forks_imports_and_schedules_without_replaying_source_work() {
        let (app,store,actor,source,foreign)=tokio::task::spawn_blocking(|| {
            let path=std::env::temp_dir().join(format!("hudson-conversation-{}.json",Uuid::new_v4()));
            std::fs::write(&path,json!({"name":"conversation-api","instructions":"answer","provider":"ollama","model":"fixture"}).to_string()).unwrap();
            let config=hudson_core::configured::Configuration::load(&path).unwrap();
            std::fs::remove_file(path).unwrap();
            let store=Store::default();
            let actor=fixtures::actor();
            let tree=config.build_temporal_tree(store.clone(),&actor).unwrap();
            let mut runtime=Runtime::new(store.clone(),AgentLoop,Final,ToolRegistry::new());
            let source=runtime.submit(&actor,tree.reference.clone(),json!("source task"),None).unwrap();
            fixtures::drive(&mut runtime,&actor,source).unwrap();
            let foreign_actor=Actor {id:"other-owner".into(),..actor.clone()};
            let foreign=runtime.submit(&foreign_actor,tree.reference.clone(),json!("foreign task"),None).unwrap();
            fixtures::drive(&mut runtime,&foreign_actor,foreign).unwrap();
            let app=crate::routes::scheduled(tree,actor.clone(),hudson_core::scheduling::ScheduleTarget {scheduler:"temporal:conversation-test".into(),task_queue:"conversation-test".into()}).unwrap();
            (app,store,actor,source,foreign)
        }).await.unwrap();
        let (status, transcript) = request(
            &app,
            "GET",
            &format!("/runs/{source}/conversation"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(transcript["format_version"], 1);
        let (status, _) = request(
            &app,
            "GET",
            &format!("/runs/{foreign}/conversation"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let path = format!("/runs/{source}/fork");
        let body = json!({"input":"new task","request_key":"fork-once"});
        let (status, receipt) = request(&app, "POST", &path, body.clone()).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(request(&app, "POST", &path, body).await.1, receipt);
        let id = Uuid::parse_str(receipt["run_id"].as_str().unwrap()).unwrap();
        let view = store.inspect(&actor, id).unwrap();
        assert_eq!(view.status, RunStatus::Queued);
        assert_eq!(view.usage.model_calls, 0);
        assert!(store.operations(&actor, id).unwrap().is_empty());
        assert_eq!(
            store.events(&actor, id, 0).unwrap()[0].payload["source_run"],
            json!(source)
        );
        let (status, _) = request(
            &app,
            "POST",
            &path,
            json!({"input":"different","request_key":"fork-once"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status,receipt)=request(&app,"POST","/runs/import",json!({"input":"imported task","request_key":"external-import","transcript":transcript})).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let imported = Uuid::parse_str(receipt["run_id"].as_str().unwrap()).unwrap();
        assert_ne!(imported, id);
        assert_eq!(
            store.inspect(&actor, imported).unwrap().status,
            RunStatus::Queued
        );
        assert_eq!(
            store.events(&actor, imported, 0).unwrap()[0].payload["source_run"],
            Value::Null
        );
    }
}
