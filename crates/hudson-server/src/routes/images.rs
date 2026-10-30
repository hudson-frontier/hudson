use super::*;
use hudson_core::multimodal::{Image, ImageTurn};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Submission {
    agent_ref: Option<hudson_core::customer::Reference>,
    input: Value,
    images: Vec<Image>,
    request_key: String,
}
pub(super) async fn submit(
    State(app): State<App>,
    Extension(permit): Extension<Arc<tokio::sync::OwnedSemaphorePermit>>,
    Json(body): Json<Submission>,
) -> Result<impl IntoResponse, ApiError> {
    blocking(move || {
        let _permit = permit;
        if app.fixture {
            return Err(ApiError(Error::Unsupported(
                "image input requires a configured agent".into(),
            )));
        }
        let turn = ImageTurn {
            input: body.input,
            images: body.images,
            request_key: body.request_key,
            goal: app.goal.clone(),
        };
        let id = if app.catalog.is_some() {
            let reference = body
                .agent_ref
                .ok_or_else(|| ApiError(Error::Invalid("agent_ref is required".into())))?;
            app.store.submit_published_images(
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
            app.lock()?.submit_images(
                &app.actor,
                app.agent.clone().ok_or_else(unavailable)?,
                turn,
                app.schedule.clone(),
            )?
        };
        app.drive(id)?;
        Ok((StatusCode::ACCEPTED, Json(json!({"run_id":id}))))
    })
    .await
}
