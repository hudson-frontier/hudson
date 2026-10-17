use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use hudson_core::{
    adapters::{
        models::ModelExecutor,
        tools::{ExecutionError, ToolRegistry},
    },
    configured::ConfiguredTree,
    fixtures::{self, DemoRuntime},
    models::*,
    runtime::Runtime,
    storage::Store,
    Error,
};
use hudson_harness::{AgentLoop, ModelRequest, ModelResponse};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

struct NoModel(Option<hudson_core::budgets::ModelBudgetBinding>);
impl ModelExecutor for NoModel {
    fn budget_binding(&self) -> Option<hudson_core::budgets::ModelBudgetBinding> {
        self.0.clone()
    }
    fn call(&mut self, _: &ModelRequest) -> Result<ModelResponse, ExecutionError> {
        Err(ExecutionError::Failed("control cannot call a model".into()))
    }
}
type Control = Runtime<AgentLoop, NoModel, ToolRegistry>;
enum Driver {
    Published(
        Arc<hudson_core::customer::HostCatalog>,
        hudson_core::scheduling::ScheduleTarget,
    ),
    Demo(DemoRuntime),
    Configured(
        Box<ConfiguredTree>,
        Option<hudson_core::scheduling::ScheduleTarget>,
    ),
}
impl Driver {
    fn tick(&mut self, actor: &Actor, id: Uuid) -> Result<RunView, Error> {
        match self {
            Self::Demo(runtime) => runtime.tick(actor, id),
            Self::Configured(runtime, None) => runtime.tick(actor, id),
            Self::Configured(_, Some(_)) | Self::Published(_, _) => Err(Error::Unsupported(
                "scheduled host cannot execute locally".into(),
            )),
        }
    }
}
#[derive(Clone)]
struct App {
    driver: Arc<Mutex<Driver>>,
    control: Arc<Mutex<Control>>,
    store: Store,
    active: Arc<Mutex<BTreeMap<Uuid, bool>>>,
    actor: Actor,
    agent: Option<VersionRef>,
    catalog: Option<Arc<hudson_core::customer::HostCatalog>>,
    goal: Option<Goal>,
    bindings: BTreeMap<VersionRef, Option<Goal>>,
    model_budget: Option<hudson_core::budgets::ModelBudgetBinding>,
    fixture: bool,
    schedule: Option<hudson_core::scheduling::ScheduleTarget>,
}
struct ApiError(Error);
impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        Self(e)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            Error::NotFound => StatusCode::NOT_FOUND,
            Error::Denied => StatusCode::FORBIDDEN,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::Invalid(_) | Error::Unsupported(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({"error":self.0.to_string()}))).into_response()
    }
}
fn unavailable() -> ApiError {
    ApiError(Error::Conflict("runtime unavailable".into()))
}
impl App {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Control>, ApiError> {
        self.control.lock().map_err(|_| unavailable())
    }
    fn validate_binding(&self, id: Uuid) -> Result<(), ApiError> {
        self.store
            .validate_schedule(&self.actor, id, self.schedule.as_ref())?;
        let run = self.store.inspect(&self.actor, id)?;
        if self.catalog.is_some() {
            let (_, configuration) = self.store.published_run_configuration(&self.actor, id)?;
            let tree = configuration
                .build_admission_tree(self.store.clone(), &self.actor)
                .map_err(|error| ApiError(Error::Invalid(error.to_string())))?;
            let bindings = tree.bindings();
            let goal = bindings
                .get(&run.agent_ref)
                .ok_or(ApiError(Error::NotFound))?;
            self.store
                .validate_resume(&self.actor, id, &run.agent_ref, goal)?;
            self.store.validate_model_budget(
                &self.actor,
                id,
                &tree.runtime.model_budget_binding(),
            )?;
            return Ok(());
        }
        let goal = self.bindings.get(&run.agent_ref).ok_or_else(|| {
            ApiError(Error::Conflict(
                "agent version is not configured in this host".into(),
            ))
        })?;
        self.store
            .validate_resume(&self.actor, id, &run.agent_ref, goal)?;
        self.store
            .validate_model_budget(&self.actor, id, &self.model_budget)?;
        Ok(())
    }
    fn drive(&self, id: Uuid) -> Result<(), ApiError> {
        self.validate_binding(id)?;
        if self.schedule.is_some() {
            // A separate Temporal worker publishes intent and observes saved controls.
            return Ok(());
        }
        // Duplicate submissions/resumes never enqueue another copy of an active run.
        {
            let mut active = self.active.lock().map_err(|_| unavailable())?;
            if let Some(wake) = active.get_mut(&id) {
                *wake = true;
                return Ok(());
            }
            active.insert(id, false);
        }
        let app = self.clone();
        tokio::task::spawn_blocking(move || {
            let result = (|| -> Result<(), ApiError> {
                let mut driver = app.driver.lock().map_err(|_| unavailable())?;
                loop {
                    let view = driver.tick(&app.actor, id)?;
                    if view.status.terminal()
                        || matches!(view.status, RunStatus::Waiting | RunStatus::Cancelling)
                    {
                        break;
                    }
                    // A previous executor may still own this operation. Never replay it.
                    if app
                        .store
                        .operations(&app.actor, id)?
                        .iter()
                        .any(|op| op.status == OperationStatus::Running)
                    {
                        break;
                    }
                }
                Ok(())
            })();
            let wake = app
                .active
                .lock()
                .ok()
                .and_then(|mut active| active.remove(&id))
                .unwrap_or(false);
            if wake {
                let _ = app.drive(id);
            }
            if let Err(error) = result {
                eprintln!("worker error: {}", error.0);
            }
        });
        Ok(())
    }
}
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| unavailable())?
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Submission {
    agent_ref: Option<hudson_core::customer::Reference>,
    input: Value,
    request_key: Option<String>,
}
async fn submit(
    State(app): State<App>,
    Json(body): Json<Submission>,
) -> Result<impl IntoResponse, ApiError> {
    blocking(move || {
        let id = if app.catalog.is_some() {
            let reference = body
                .agent_ref
                .ok_or_else(|| ApiError(Error::Invalid("agent_ref is required".into())))?;
            let key = body
                .request_key
                .ok_or_else(|| ApiError(Error::Invalid("request_key is required".into())))?;
            app.store.submit_published(
                &app.actor,
                &reference.version_ref(),
                body.input,
                &key,
                app.schedule.clone().ok_or_else(unavailable)?,
            )?
        } else {
            if body.agent_ref.is_some() {
                return Err(ApiError(Error::Invalid(
                    "agent_ref requires published mode".into(),
                )));
            }
            let agent = app.agent.clone().ok_or_else(unavailable)?;
            if app.fixture {
                // Submission pins the backend checkpoint format, even before its first tick.
                Runtime::new(
                    app.store.clone(),
                    fixtures::FixtureBackend,
                    NoModel(None),
                    ToolRegistry::new(),
                )
                .submit(&app.actor, agent, body.input, body.request_key)?
            } else {
                app.lock()?.submit_scheduled(
                    &app.actor,
                    agent,
                    body.input,
                    body.request_key,
                    app.goal.clone(),
                    app.schedule.clone(),
                )?
            }
        };
        app.drive(id)?;
        Ok((StatusCode::ACCEPTED, Json(json!({"run_id":id}))))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserReply {
    question_id: u64,
    input: Value,
    request_key: String,
}
async fn reply(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    Json(body): Json<UserReply>,
) -> Result<Json<Value>, ApiError> {
    blocking(move || {
        app.validate_binding(id)?;
        app.lock()?.provide_input(
            &app.actor,
            id,
            body.question_id,
            &body.request_key,
            body.input,
        )?;
        app.drive(id)?;
        Ok(Json(json!({"run_id":id})))
    })
    .await
}
async fn resume(State(app): State<App>, Path(id): Path<Uuid>) -> Result<Json<Value>, ApiError> {
    blocking(move || {
        app.validate_binding(id)?;
        app.drive(id)?;
        Ok(Json(json!({"run_id":id})))
    })
    .await
}
async fn inspect(State(app): State<App>, Path(id): Path<Uuid>) -> Result<Json<Value>, ApiError> {
    blocking(move || {
        Ok(Json(
            serde_json::to_value(app.store.inspect(&app.actor, id)?).map_err(Error::from)?,
        ))
    })
    .await
}
async fn inspect_schedule(
    State(app): State<App>,
    Path(id): Path<Uuid>,
) -> Result<Json<hudson_core::scheduling::ScheduleView>, ApiError> {
    blocking(move || Ok(Json(app.store.inspect_schedule(&app.actor, id)?))).await
}
async fn children(State(app): State<App>, Path(id): Path<Uuid>) -> Result<Json<Value>, ApiError> {
    blocking(move || {
        Ok(Json(
            serde_json::to_value(app.store.children(&app.actor, id)?).map_err(Error::from)?,
        ))
    })
    .await
}
async fn inspect_operation(
    State(app): State<App>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    blocking(move || {
        Ok(Json(
            serde_json::to_value(app.store.inspect_operation(&app.actor, id)?)
                .map_err(Error::from)?,
        ))
    })
    .await
}
#[derive(Deserialize)]
struct Cursor {
    #[serde(default)]
    after: u64,
    #[serde(default = "default_event_limit")]
    limit: usize,
}
fn default_event_limit() -> usize {
    100
}
async fn events(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    Query(cursor): Query<Cursor>,
) -> Result<Json<Value>, ApiError> {
    if !(1..=1000).contains(&cursor.limit) {
        return Err(Error::Invalid("event page limit must be between 1 and 1000".into()).into());
    }
    blocking(move || {
        Ok(Json(
            serde_json::to_value(app.store.event_page(
                &app.actor,
                id,
                cursor.after,
                cursor.limit,
            )?)
            .map_err(Error::from)?,
        ))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    approved: bool,
}
async fn approve(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    Json(body): Json<Decision>,
) -> Result<Json<Value>, ApiError> {
    blocking(move || {
        let run_id = {
            let runtime = app.lock()?;
            let run_id = runtime.store.operation_run(&app.actor, id)?;
            app.validate_binding(run_id)?;
            runtime.approve(&app.actor, id, body.approved, now(), now() + 60_000)?;
            run_id
        };
        app.drive(run_id)?;
        Ok(Json(json!({"run_id":run_id})))
    })
    .await
}
async fn cancel(State(app): State<App>, Path(id): Path<Uuid>) -> Result<Json<Value>, ApiError> {
    blocking(move || {
        if let Some(target) = &app.schedule {
            app.store.validate_schedule(&app.actor, id, Some(target))?;
        }
        app.lock()?.cancel(&app.actor, id)?;
        Ok(Json(json!({"run_id":id})))
    })
    .await
}
fn assemble(
    driver: Driver,
    store: Store,
    actor: Actor,
    agent: Option<VersionRef>,
    goal: Option<Goal>,
    durable: bool,
    mode: &'static str,
) -> Router {
    let bindings = match &driver {
        Driver::Configured(tree, _) => tree.bindings(),
        Driver::Demo(_) => [(agent.clone().expect("fixture agent"), goal.clone())].into(),
        Driver::Published(_, _) => Default::default(),
    };
    let model_budget = match &driver {
        Driver::Configured(tree, _) => tree.runtime.model_budget_binding(),
        Driver::Demo(runtime) => runtime.model_budget_binding(),
        Driver::Published(_, _) => None,
    };
    let schedule = match &driver {
        Driver::Configured(_, target) => target.clone(),
        Driver::Demo(_) => None,
        Driver::Published(_, target) => Some(target.clone()),
    };
    let catalog = match &driver {
        Driver::Published(catalog, _) => Some(catalog.clone()),
        _ => None,
    };
    let publishing = catalog.is_some();
    let app = App {
        catalog,
        schedule,
        model_budget: model_budget.clone(),
        bindings,
        driver: Arc::new(Mutex::new(driver)),
        control: Arc::new(Mutex::new(Runtime::new(
            store.clone(),
            AgentLoop,
            NoModel(model_budget),
            ToolRegistry::new(),
        ))),
        store,
        active: Default::default(),
        actor,
        agent,
        goal,
        fixture: mode == "fixture",
    };
    let router = Router::new()
        .route(
            "/openapi.json",
            get(
                move |authenticated: Option<Extension<crate::auth::Authenticated>>| async move {
                    let mut specification: Value =
                        serde_json::from_str(include_str!("../../../docs/openapi.json"))
                            .expect("checked OpenAPI document");
                    specification["security"] = if authenticated.is_some() {
                        json!([{"HudsonBearer":[]}])
                    } else {
                        json!([])
                    };
                    if mode == "published" {
                        specification["paths"]["/runs"]["post"]["requestBody"]["content"]
                            ["application/json"]["schema"] =
                            json!({"$ref":"#/components/schemas/PublishedSubmission"});
                    } else if let Some(paths) = specification["paths"].as_object_mut() {
                        for path in [
                            "/capabilities",
                            "/tools",
                            "/tools/{name}/versions/{version}",
                            "/agents",
                            "/agents/{name}/versions/{version}",
                        ] {
                            paths.remove(path);
                        }
                    }
                    Json(specification)
                },
            ),
        )
        .route(
            "/health",
            get(move || async move { Json(json!({"mode":mode,"durable":durable})) }),
        )
        .route("/runs", post(submit))
        .route("/runs/{id}", get(inspect))
        .route("/runs/{id}/events", get(events))
        .route("/runs/{id}/schedule", get(inspect_schedule))
        .route("/runs/{id}/children", get(children))
        .route("/runs/{id}/resume", post(resume))
        .route("/runs/{id}/input", post(reply))
        .route("/runs/{id}/cancel", post(cancel))
        .route("/operations/{id}", get(inspect_operation))
        .route("/operations/{id}/approval", post(approve));
    let router = if publishing {
        router
            .route("/capabilities", get(capabilities))
            .route("/tools", post(publish_tool))
            .route("/tools/{name}/versions/{version}", get(get_tool))
            .route("/agents", post(publish_agent))
            .route("/agents/{name}/versions/{version}", get(get_agent))
    } else {
        router
    };
    router
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .with_state(app)
}
pub fn router() -> Result<Router, Error> {
    let runtime = fixtures::runtime()?;
    let store = runtime.store.clone();
    Ok(assemble(
        Driver::Demo(runtime),
        store,
        fixtures::actor(),
        Some(fixtures::agent_ref()),
        None,
        false,
        "fixture",
    ))
}
pub fn configured(tree: ConfiguredTree, actor: Actor, durable: bool) -> Router {
    let store = tree.runtime.store.clone();
    let agent = tree.reference.clone();
    let goal = tree.goal.clone();
    assemble(
        Driver::Configured(Box::new(tree), None),
        store,
        actor,
        Some(agent),
        goal,
        durable,
        "configured",
    )
}

pub fn scheduled(
    tree: ConfiguredTree,
    actor: Actor,
    target: hudson_core::scheduling::ScheduleTarget,
) -> Result<Router, Error> {
    target.validate()?;
    let store = tree.runtime.store.clone();
    let agent = tree.reference.clone();
    let goal = tree.goal.clone();
    Ok(assemble(
        Driver::Configured(Box::new(tree), Some(target)),
        store,
        actor,
        Some(agent),
        goal,
        true,
        "temporal",
    ))
}

pub fn published(
    store: Store,
    actor: Actor,
    target: hudson_core::scheduling::ScheduleTarget,
    catalog: hudson_core::customer::HostCatalog,
) -> Result<Router, Error> {
    target.validate()?;
    catalog.install(&store, &actor)?;
    Ok(assemble(
        Driver::Published(Arc::new(catalog), target),
        store,
        actor,
        None,
        None,
        true,
        "published",
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolPublication {
    request_key: String,
    tool: hudson_core::customer::ToolDefinition,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentPublication {
    request_key: String,
    agent: hudson_core::customer::AgentDefinition,
}
async fn capabilities(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        app.catalog
            .as_ref()
            .ok_or_else(unavailable)?
            .capabilities(&app.actor)?,
    ))
}
async fn publish_tool(
    State(app): State<App>,
    Json(body): Json<ToolPublication>,
) -> Result<impl IntoResponse, ApiError> {
    blocking(move || {
        let receipt = app.store.publish_customer_tool(
            &app.actor,
            app.catalog.as_ref().ok_or_else(unavailable)?,
            body.tool,
            &body.request_key,
        )?;
        Ok((StatusCode::CREATED, Json(receipt)))
    })
    .await
}
async fn publish_agent(
    State(app): State<App>,
    Json(body): Json<AgentPublication>,
) -> Result<impl IntoResponse, ApiError> {
    blocking(move || {
        let receipt = app.store.publish_customer_agent(
            &app.actor,
            app.catalog.as_ref().ok_or_else(unavailable)?,
            body.agent,
            &body.request_key,
        )?;
        Ok((StatusCode::CREATED, Json(receipt)))
    })
    .await
}
async fn get_tool(
    State(app): State<App>,
    Path((name, version)): Path<(String, u32)>,
) -> Result<Json<hudson_core::customer::ToolDefinition>, ApiError> {
    blocking(move || {
        Ok(Json(app.store.customer_tool(
            &app.actor,
            &hudson_core::customer::Reference { id: name, version },
        )?))
    })
    .await
}
async fn get_agent(
    State(app): State<App>,
    Path((name, version)): Path<(String, u32)>,
) -> Result<Json<hudson_core::customer::AgentDefinition>, ApiError> {
    blocking(move || {
        Ok(Json(app.store.customer_agent(
            &app.actor,
            &hudson_core::customer::Reference { id: name, version },
        )?))
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

    async fn request(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
        let value = serde_json::from_slice(&bytes).unwrap();
        let specification: Value =
            serde_json::from_str(include_str!("../../../docs/openapi.json")).unwrap();
        let raw_path = path.split('?').next().unwrap();
        let mut segments = raw_path.split('/').collect::<Vec<_>>();
        if segments.len() >= 3 && matches!(segments[1], "runs" | "operations") {
            segments[2] = "{id}";
        }
        if segments.len() == 5 && matches!(segments[1], "agents" | "tools") {
            segments[2] = "{name}";
            segments[4] = "{version}";
        }
        let responses =
            &specification["paths"][segments.join("/")][method.to_lowercase()]["responses"];
        let response = responses
            .get(status.as_str())
            .unwrap_or(&responses["default"]);
        let schema = &response["content"]["application/json"]["schema"];
        assert!(
            schema.is_object(),
            "missing response contract for {method} {path} {status}"
        );
        let validator = jsonschema::validator_for(
            &json!({"allOf":[schema], "components":specification["components"]}),
        )
        .unwrap();
        assert!(
            validator.is_valid(&value),
            "response violates contract: {method} {path}: {value}"
        );
        (status, value)
    }
    async fn until(app: &Router, id: &str, status: &str) -> Value {
        for _ in 0..100 {
            let (_, view) = request(app, "GET", &format!("/runs/{id}"), Value::Null).await;
            if view["status"] == status {
                return view;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("run did not reach {status}");
    }

    #[tokio::test]
    async fn publication_routes_match_the_live_contract_without_execution() {
        let app = tokio::task::spawn_blocking(|| {
            let actor = fixtures::actor();
            let catalog = serde_json::from_value(json!({"owner":actor,
                "models":[{"reference":{"id":"standard","version":1},"provider":"openai","model":"fixture","endpoint":"http://127.0.0.1:9/model","api_key_env":"HUDSON_MISSING_PUBLICATION_KEY"}],
                "connections":[{"reference":{"id":"crm","version":1},"transport":{"type":"http","endpoint":"http://127.0.0.1:9/tool","token_env":"HUDSON_MISSING_PUBLICATION_KEY"}}]})).unwrap();
            published(Store::default(),actor,hudson_core::scheduling::ScheduleTarget { scheduler:"temporal:test".into(),task_queue:"queue".into() },catalog).unwrap()
        }).await.unwrap();
        let (_, spec) = request(&app, "GET", "/openapi.json", Value::Null).await;
        assert_eq!(
            spec["paths"]["/runs"]["post"]["requestBody"]["content"]["application/json"]["schema"]
                ["$ref"],
            "#/components/schemas/PublishedSubmission"
        );
        request(&app, "GET", "/capabilities", Value::Null).await;
        request(&app,"POST","/tools",json!({"request_key":"tool","tool":{"name":"lookup","version":1,"description":"lookup","connection":{"id":"crm","version":1},"input_schema":{"type":"object"}}})).await;
        request(&app, "GET", "/tools/lookup/versions/1", Value::Null).await;
        let (status, receipt) = request(&app,"POST","/agents",json!({"request_key":"agent","agent":{"name":"assistant","version":1,"instructions":"help","model_profile":{"id":"standard","version":1},"tools":[{"id":"lookup","version":1}]}})).await;
        assert_eq!(status, StatusCode::CREATED);
        request(&app, "GET", "/agents/assistant/versions/1", Value::Null).await;
        let (status, run) = request(
            &app,
            "POST",
            "/runs",
            json!({"agent_ref":receipt["agent_ref"],"request_key":"task","input":"help"}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let (_, view) = request(
            &app,
            "GET",
            &format!("/runs/{}", run["run_id"].as_str().unwrap()),
            Value::Null,
        )
        .await;
        assert_eq!(view["status"], "queued");
    }

    #[tokio::test]
    async fn scheduled_api_persists_without_local_execution_and_fences_other_queues() {
        let (app, other, local, store, actor, agent, target) = tokio::task::spawn_blocking(|| {
            let path = std::env::temp_dir().join(format!("hudson-api-{}.json", Uuid::new_v4()));
            std::fs::write(
                &path,
                json!({"name":"scheduled-test", "instructions":"finish",
                "endpoint":"http://127.0.0.1:9/chat", "api_key_env":null})
                .to_string(),
            )
            .unwrap();
            let store = Store::default();
            let actor = fixtures::actor();
            let build = || {
                hudson_core::configured::Configuration::load(&path)
                    .unwrap()
                    .build_temporal_tree(store.clone(), &actor)
                    .unwrap()
            };
            let tree = build();
            let agent = tree.reference.clone();
            let target = hudson_core::scheduling::ScheduleTarget {
                scheduler: "temporal:api-test".into(),
                task_queue: "first".into(),
            };
            let app = scheduled(tree, actor.clone(), target.clone()).unwrap();
            let other = scheduled(
                build(),
                actor.clone(),
                hudson_core::scheduling::ScheduleTarget {
                    task_queue: "other".into(),
                    ..target.clone()
                },
            )
            .unwrap();
            let local = configured(
                hudson_core::configured::Configuration::load(&path)
                    .unwrap()
                    .build_tree(store.clone(), &actor)
                    .unwrap(),
                actor.clone(),
                true,
            );
            std::fs::remove_file(path).unwrap();
            (app, other, local, store, actor, agent, target)
        })
        .await
        .unwrap();
        let (_, health) = request(&app, "GET", "/health", Value::Null).await;
        assert_eq!(health["mode"], "temporal");
        let body = json!({"input":"finish", "request_key":"one"});
        let (status, receipt) = request(&app, "POST", "/runs", body.clone()).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let id = receipt["run_id"].as_str().unwrap();
        let (_, repeated) = request(&app, "POST", "/runs", body).await;
        assert_eq!(receipt, repeated);
        let (_, view) = request(&app, "GET", &format!("/runs/{id}"), Value::Null).await;
        assert_eq!(view["status"], "queued");
        let parsed = Uuid::parse_str(id).unwrap();
        assert!(store.operations(&actor, parsed).unwrap().is_empty());
        assert_eq!(
            store
                .pending_schedules(&actor, &agent, &target, 100)
                .unwrap(),
            vec![parsed]
        );
        let (status, pending) =
            request(&app, "GET", &format!("/runs/{id}/schedule"), Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            pending,
            json!({"root_run_id":id,"status":"pending","last_attempt_at":null,"published_at":null})
        );
        store
            .record_schedule_attempt(&actor, parsed, &target)
            .unwrap();
        store.acknowledge_schedule(&actor, parsed, &target).unwrap();
        let (_, published) =
            request(&app, "GET", &format!("/runs/{id}/schedule"), Value::Null).await;
        assert_eq!(published["status"], "published");
        assert!(published["published_at"].is_u64());
        let (_, queued) = request(&app, "GET", &format!("/runs/{id}"), Value::Null).await;
        assert_eq!(queued["status"], "queued");
        for action in ["resume", "cancel"] {
            let (status, _) =
                request(&other, "POST", &format!("/runs/{id}/{action}"), json!({})).await;
            assert_eq!(status, StatusCode::CONFLICT);
        }
        let (status, _) = request(&local, "POST", &format!("/runs/{id}/resume"), json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = request(&app, "POST", &format!("/runs/{id}/resume"), json!({})).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = request(&app, "POST", &format!("/runs/{id}/cancel"), json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(store
            .pending_schedules(&actor, &agent, &target, 100)
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn event_pages_reconstruct_history_and_reject_invalid_limits() {
        let app = router().unwrap();
        let (_, start) = request(
            &app,
            "POST",
            "/runs",
            json!({"input":{"order_id":"123","action":"lookup"}}),
        )
        .await;
        let id = start["run_id"].as_str().unwrap();
        until(&app, id, "completed").await;
        let defaults: Cursor = serde_json::from_value(json!({})).unwrap();
        assert_eq!(defaults.limit, 100);
        assert_eq!(defaults.after, 0);
        let (status, all) = request(
            &app,
            "GET",
            &format!("/runs/{id}/events?limit=1000"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(all.as_array().unwrap().len() > 2);
        let (_, default_page) =
            request(&app, "GET", &format!("/runs/{id}/events"), Value::Null).await;
        assert_eq!(default_page, all);
        let mut collected = Vec::new();
        let mut after = 0;
        loop {
            let (status, page) = request(
                &app,
                "GET",
                &format!("/runs/{id}/events?after={after}&limit=2"),
                Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let page = page.as_array().unwrap();
            assert!(page.len() <= 2);
            if page.is_empty() {
                break;
            }
            for event in page {
                let sequence = event["sequence"].as_u64().unwrap();
                assert!(sequence > after);
                after = sequence;
                collected.push(event.clone());
            }
        }
        assert_eq!(Value::Array(collected), all);
        for limit in [0, 1001] {
            let (status, _) = request(
                &app,
                "GET",
                &format!("/runs/{id}/events?limit={limit}"),
                Value::Null,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        let (status, _) = request(
            &app,
            "GET",
            &format!("/runs/{}/events?after={}&limit=1", Uuid::new_v4(), u64::MAX),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (_, page) = request(
            &app,
            "GET",
            &format!("/runs/{id}/events?after={}&limit=1", u64::MAX),
            Value::Null,
        )
        .await;
        assert_eq!(page, json!([]));
    }

    #[tokio::test]
    async fn scheduling_receipts_for_local_and_missing_runs() {
        let app = router().unwrap();
        let (_, submitted) = request(
            &app,
            "POST",
            "/runs",
            json!({"input":{"order_id":"123","action":"lookup"}}),
        )
        .await;
        let id = submitted["run_id"].as_str().unwrap();
        let (status, receipt) =
            request(&app, "GET", &format!("/runs/{id}/schedule"), Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            receipt,
            json!({"root_run_id":id,"status":"unscheduled","last_attempt_at":null,"published_at":null})
        );
        let (status, _) = request(
            &app,
            "GET",
            &format!("/runs/{}/schedule", Uuid::new_v4()),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (_, spec) = request(&app, "GET", "/openapi.json", Value::Null).await;
        let schema =
            json!({"$ref":"#/components/schemas/ScheduleView","components":spec["components"]});
        assert!(jsonschema::validator_for(&schema)
            .unwrap()
            .is_valid(&receipt));
    }

    #[tokio::test]
    async fn serves_openapi_and_compiles_all_public_schemas() {
        let app = router().unwrap();
        let (status, spec) = request(&app, "GET", "/openapi.json", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(spec["openapi"], "3.1.0");
        assert_eq!(spec["security"], json!([]));
        for name in spec["components"]["schemas"].as_object().unwrap().keys() {
            jsonschema::validator_for(&json!({"$ref":format!("#/components/schemas/{name}"), "components":spec["components"]})).unwrap();
        }
        let (_, health) = request(&app, "GET", "/health", Value::Null).await;
        assert_eq!(health["mode"], "fixture");
        let (_, start) = request(
            &app,
            "POST",
            "/runs",
            json!({"input":{"order_id":"123","action":"lookup"}}),
        )
        .await;
        let id = start["run_id"].as_str().unwrap();
        until(&app, id, "completed").await;
        let (_, children) =
            request(&app, "GET", &format!("/runs/{id}/children"), Value::Null).await;
        assert_eq!(children, json!([]));
        request(&app, "POST", &format!("/runs/{id}/resume"), Value::Null).await;
        let (status, _) = request(
            &app,
            "POST",
            &format!("/runs/{id}/input"),
            json!({"input":"late", "request_key":"answer", "question_id":1}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn preview_api_shows_exact_approval_and_resumes_the_same_run() {
        let app = router().unwrap();
        let (status, start) = request(
            &app,
            "POST",
            "/runs",
            json!({
                "input":{"order_id":"123","action":"refund"}, "request_key":"refund-1"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let id = start["run_id"].as_str().unwrap();
        let waiting = until(&app, id, "waiting").await;
        assert!(waiting.get("state").is_none());
        let operation_id = waiting["wait"]["operation_id"].as_str().unwrap();
        let (status, operation) = request(
            &app,
            "GET",
            &format!("/operations/{operation_id}"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(operation["tool_name"], "refund_order");
        assert_eq!(
            operation["arguments"],
            json!({"order_id":"123","amount_cents":3000})
        );
        assert!(operation.get("request").is_none());
        assert!(operation.get("attempts").is_none());
        let (status, _) = request(
            &app,
            "POST",
            &format!("/operations/{operation_id}/approval"),
            json!({"approved":true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let completed = until(&app, id, "completed").await;
        assert_eq!(completed["assessment"]["passed"], true);
        let (_, events) = request(
            &app,
            "GET",
            &format!("/runs/{id}/events?after=2"),
            Value::Null,
        )
        .await;
        assert!(events
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["sequence"].as_u64().unwrap() > 2));
        let (_, duplicate) = request(
            &app,
            "POST",
            "/runs",
            json!({
                "input":{"order_id":"123","action":"refund"}, "request_key":"refund-1"
            }),
        )
        .await;
        assert_eq!(start, duplicate);
    }

    #[tokio::test]
    async fn preview_api_rejects_changed_submission_and_unknown_run() {
        let app = router().unwrap();
        let input = json!({"input":{"order_id":"123","action":"lookup"},"request_key":"same"});
        assert_eq!(
            request(&app, "POST", "/runs", input).await.0,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/runs",
                json!({
                    "input":{"order_id":"456","action":"lookup"},"request_key":"same"
                })
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            request(
                &app,
                "GET",
                &format!("/runs/{}", Uuid::new_v4()),
                Value::Null
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
}
