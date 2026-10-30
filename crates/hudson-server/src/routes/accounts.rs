//! Sessions are transient operator control, never run state or model tools.
use super::*;
use hudson_core::adapters::runtime_bridge::accounts::{
    AccountAction, AccountDescriptor, AccountSession,
};
use std::time::Duration;

pub(super) type Sessions = Arc<Mutex<BTreeMap<Uuid, Entry>>>;
pub(super) struct Entry {
    account: String,
    key: String,
    expires_at: u64,
    session: AccountSession,
}
fn private<T: serde::Serialize>(value: T) -> impl IntoResponse {
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(value),
    )
}
pub(super) async fn catalog(State(app): State<App>) -> impl IntoResponse {
    private(
        app.catalog
            .as_ref()
            .map(|catalog| {
                catalog
                    .accounts
                    .iter()
                    .map(|account| account.descriptor())
                    .collect::<Vec<AccountDescriptor>>()
            })
            .unwrap_or_default(),
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Open {
    request_key: String,
}
pub(super) async fn open(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(body): Json<Open>,
) -> Result<impl IntoResponse, ApiError> {
    if body.request_key.trim().is_empty()
        || body.request_key.len() > 128
        || body.request_key.chars().any(char::is_control)
    {
        return Err(Error::Invalid(
            "account session requires a bounded explicit request key".into(),
        )
        .into());
    }
    let config = app
        .catalog
        .as_ref()
        .and_then(|catalog| catalog.accounts.iter().find(|config| config.id == id))
        .ok_or(Error::NotFound)?
        .clone();
    let mut sessions = app.account_sessions.lock().map_err(|_| unavailable())?;
    sessions.retain(|_, entry| !entry.session.expired());
    if let Some((session_id, entry)) = sessions
        .iter()
        .find(|(_, entry)| entry.key == body.request_key)
    {
        if entry.account != id {
            return Err(Error::Conflict(
                "account session key was reused for another account".into(),
            )
            .into());
        }
        return Ok(private(
            json!({"id":session_id,"account_id":id,"expires_at":entry.expires_at}),
        ));
    }
    if sessions.len() >= 8 {
        return Err(Error::Conflict("account session capacity reached".into()).into());
    }
    if sessions.values().any(|entry| entry.account == id) {
        return Err(Error::Conflict("account already has an active control session".into()).into());
    }
    let expires_at = now() + Duration::from_secs(config.lifetime_seconds).as_millis() as u64;
    let session = AccountSession::open(config, app.actor.clone())?;
    let session_id = Uuid::new_v4();
    sessions.insert(
        session_id,
        Entry {
            account: id.clone(),
            key: body.request_key,
            expires_at,
            session,
        },
    );
    Ok(private(
        json!({"id":session_id,"account_id":id,"expires_at":expires_at}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Action {
    request_key: String,
    action: AccountAction,
}
pub(super) async fn action(
    State(app): State<App>,
    Path(id): Path<Uuid>,
    Json(body): Json<Action>,
) -> Result<impl IntoResponse, ApiError> {
    let session = app
        .account_sessions
        .lock()
        .map_err(|_| unavailable())?
        .get(&id)
        .ok_or(Error::NotFound)?
        .session
        .clone();
    let result = session
        .action(&app.actor, &body.request_key, body.action)
        .await?;
    Ok(private(result))
}
pub(super) async fn close(
    State(app): State<App>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let entry = app
        .account_sessions
        .lock()
        .map_err(|_| unavailable())?
        .remove(&id)
        .ok_or(Error::NotFound)?;
    entry.session.close(&app.actor)?;
    Ok(private(json!({"id":id,"closed":true})))
}
