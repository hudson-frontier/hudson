use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Json, Router,
};
use hudson_core::{models::Actor, storage::Store, Error};
use serde_json::json;

#[derive(Clone)]
pub(crate) struct Authenticated;

#[derive(Clone)]
struct Authentication {
    store: Store,
    actor: Actor,
}

pub fn protect(router: Router, store: Store, actor: Actor) -> Router {
    router.layer(middleware::from_fn_with_state(
        Authentication { store, actor },
        authenticate,
    ))
}

fn rejected(status: StatusCode, message: &'static str) -> Response {
    let mut response = (status, Json(json!({"error":message}))).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    if status == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, "Bearer".parse().unwrap());
    }
    response
}

async fn authenticate(
    State(auth): State<Authentication>,
    mut request: Request,
    next: Next,
) -> Response {
    let mut headers = request.headers().get_all(header::AUTHORIZATION).iter();
    let bearer = headers
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
        .map(|(_, token)| token.to_owned());
    let Some(bearer) = bearer.filter(|_| headers.next().is_none()) else {
        return rejected(StatusCode::UNAUTHORIZED, "authentication required");
    };
    let expected = auth.actor;
    let result =
        tokio::task::spawn_blocking(move || auth.store.authenticate_api_token(&bearer)).await;
    match result {
        Ok(Ok(actor)) if actor.workspace_id == expected.workspace_id && actor.id == expected.id => {
            request.extensions_mut().insert(Authenticated);
            next.run(request).await
        }
        Ok(Ok(_)) => rejected(
            StatusCode::FORBIDDEN,
            "credential scope does not permit access",
        ),
        Ok(Err(Error::Denied)) => rejected(StatusCode::UNAUTHORIZED, "invalid credential"),
        _ => rejected(
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication unavailable",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, routing::post};
    use hudson_core::models::now;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn live_openapi_requires_bearer_authentication_on_a_protected_instance() {
        let store = Store::default();
        let actor = Actor {
            workspace_id: "project".into(),
            id: "backend".into(),
        };
        let token = store
            .issue_api_token(actor.clone(), "backend".into(), now() + 60_000)
            .unwrap();
        let router = protect(crate::routes::router().unwrap(), store, actor);
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/openapi.json")
                    .header(header::AUTHORIZATION, format!("Bearer {}", token.bearer()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .unwrap();
        let specification: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(specification["security"], json!([{"HudsonBearer":[]}]));
    }

    #[tokio::test]
    async fn scheduling_inspection_requires_a_live_scoped_token() {
        let store = Store::default();
        let actor = hudson_core::fixtures::actor();
        let token = store
            .issue_api_token(actor.clone(), "schedule-reader".into(), now() + 60_000)
            .unwrap();
        let router = protect(
            crate::routes::router().unwrap(),
            store.clone(),
            actor.clone(),
        );
        let path = format!("/runs/{}/schedule", uuid::Uuid::new_v4());
        let send = |bearer: Option<String>| {
            let router = router.clone();
            let path = path.clone();
            async move {
                let mut builder = Request::builder().uri(path);
                if let Some(bearer) = bearer {
                    builder = builder.header(header::AUTHORIZATION, bearer);
                }
                router
                    .oneshot(builder.body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status()
            }
        };
        assert_eq!(send(None).await, StatusCode::UNAUTHORIZED);
        let bearer = format!("Bearer {}", token.bearer());
        assert_eq!(send(Some(bearer.clone())).await, StatusCode::NOT_FOUND);
        store.revoke_api_token(&actor, token.metadata.id).unwrap();
        assert_eq!(send(Some(bearer)).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authentication_precedes_effects_and_checks_revocation_on_every_request() {
        let store = Store::default();
        let actor = Actor {
            workspace_id: "project-one".into(),
            id: "backend".into(),
        };
        let token = store
            .issue_api_token(actor.clone(), "backend".into(), now() + 60_000)
            .unwrap();
        let other = store
            .issue_api_token(
                Actor {
                    workspace_id: "project-two".into(),
                    ..actor.clone()
                },
                "other".into(),
                now() + 60_000,
            )
            .unwrap();
        let effects = Arc::new(AtomicUsize::new(0));
        let calls = effects.clone();
        let router = protect(
            Router::new().route(
                "/runs",
                post(move || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    StatusCode::ACCEPTED
                }),
            ),
            store.clone(),
            actor.clone(),
        );
        let send = |values: Vec<String>| {
            let router = router.clone();
            async move {
                let mut builder = Request::builder().method("POST").uri("/runs");
                for value in values {
                    builder = builder.header(header::AUTHORIZATION, value);
                }
                router
                    .oneshot(builder.body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status()
            }
        };
        let valid = format!("Bearer {}", token.bearer());
        assert_eq!(send(vec![]).await, StatusCode::UNAUTHORIZED);
        assert_eq!(
            send(vec!["Bearer invalid".into()]).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            send(vec![format!("Bearer {}", other.bearer())]).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(vec![valid.clone(), valid.clone()]).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        assert_eq!(send(vec![valid.clone()]).await, StatusCode::ACCEPTED);
        store.revoke_api_token(&actor, token.metadata.id).unwrap();
        assert_eq!(send(vec![valid]).await, StatusCode::UNAUTHORIZED);
        assert_eq!(effects.load(Ordering::SeqCst), 1);
    }
}
