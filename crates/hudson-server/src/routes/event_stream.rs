//! Cursor-based event delivery. No effects are tied to a connection's lifetime.
//! Bounded pages plus transport backpressure prevent slow clients growing queues.
use super::{ApiError, App};
use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, Query, State, WebSocketUpgrade,
    },
    http::{header, HeaderMap},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Extension,
};
use futures::{stream, SinkExt, StreamExt};
use hudson_core::{
    models::{Event as StoredEvent, RunStatus},
    Error,
};
use serde::Deserialize;
use std::{
    collections::VecDeque,
    convert::Infallible,
    time::{Duration, Instant},
};
use uuid::Uuid;

const POLL: Duration = Duration::from_millis(250);
const LIFETIME: Duration = Duration::from_secs(300);
#[cfg(test)]
mod tests;
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StreamCursor {
    #[serde(default)]
    after: u64,
}

struct Journal {
    app: App,
    run: Uuid,
    after: u64,
    events: VecDeque<StoredEvent>,
    started: Instant,
    last_poll: Option<Instant>,
    token: Option<String>,
    _permit: tokio::sync::OwnedSemaphorePermit,
    ended: bool,
}
impl Journal {
    async fn new(
        app: App,
        run: Uuid,
        mut after: u64,
        headers: &HeaderMap,
        authenticated: bool,
    ) -> Result<Self, ApiError> {
        if let Some(last) = headers.get("last-event-id") {
            let last = last
                .to_str()
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| ApiError(Error::Invalid("invalid Last-Event-ID".into())))?;
            if after != 0 && after != last {
                return Err(ApiError(Error::Invalid(
                    "conflicting stream cursors".into(),
                )));
            }
            after = last;
        }
        let permit = app.stream_slots.clone().try_acquire_owned().map_err(|_| {
            ApiError(Error::Conflict(
                "event stream connection limit reached".into(),
            ))
        })?;
        // Protected hosts have already validated the header. Recheck it while
        // the stream remains open so expiry and revocation close subscriptions.
        let token = if authenticated {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split_once(' '))
                .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
                .map(|(_, value)| value.to_owned())
        } else {
            None
        };
        if authenticated && token.is_none() {
            return Err(ApiError(Error::Denied));
        }
        let mut journal = Self {
            app,
            run,
            after,
            events: VecDeque::new(),
            started: Instant::now(),
            last_poll: None,
            token,
            _permit: permit,
            ended: false,
        };
        journal.poll().await?;
        Ok(journal)
    }
    async fn poll(&mut self) -> Result<(), ApiError> {
        let read_permit = self
            .app
            .stream_reads
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError(Error::Conflict("event stream reader limit reached".into())))?;
        let timeout = (LIFETIME.saturating_sub(self.started.elapsed())).min(Duration::from_secs(5));
        if timeout.is_zero() {
            return Err(ApiError(Error::Conflict(
                "event stream lease expired".into(),
            )));
        }
        let store = self.app.store.clone();
        let actor = self.app.actor.clone();
        let token = self.token.clone();
        let run = self.run;
        let after = self.after;
        let read = tokio::task::spawn_blocking(move || {
            let _permit = read_permit;
            if let Some(token) = token {
                if store.authenticate_api_token(&token)? != actor {
                    return Err(Error::Denied);
                }
            }
            let status = store.run_status(&actor, run)?;
            let page = store.event_stream_page(&actor, run, after)?;
            Ok::<_, Error>((
                page,
                matches!(
                    status,
                    RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
                ),
            ))
        });
        let (page, terminal) = tokio::time::timeout(timeout, read)
            .await
            .map_err(|_| ApiError(Error::Conflict("event stream read timed out".into())))?
            .map_err(|_| ApiError(Error::Conflict("event stream unavailable".into())))??;
        self.ended = page.is_empty() && terminal;
        self.events = page.into();
        self.last_poll = Some(Instant::now());
        Ok(())
    }
    async fn next(&mut self) -> Option<StoredEvent> {
        loop {
            if self.started.elapsed() >= LIFETIME || self.ended {
                return None;
            }
            if self.last_poll.is_some_and(|p| p.elapsed() >= POLL) && self.token.is_some() {
                // Revalidate authorization without losing a partially delivered page.
                let pending = std::mem::take(&mut self.events);
                let after = self.after;
                if self.poll().await.is_err() {
                    self.ended = true;
                    return None;
                }
                if !pending.is_empty() {
                    self.events = pending;
                    self.ended = false;
                    self.after = after;
                }
            }
            if let Some(event) = self.events.pop_front() {
                self.after = event.sequence;
                return Some(event);
            }
            if let Some(last) = self.last_poll {
                let wait = POLL.saturating_sub(last.elapsed());
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
            }
            if self.poll().await.is_err() {
                self.ended = true;
                return None;
            }
        }
    }
}
pub(super) async fn sse(
    State(app): State<App>,
    Path(run): Path<Uuid>,
    Query(cursor): Query<StreamCursor>,
    headers: HeaderMap,
    authenticated: Option<Extension<crate::auth::Authenticated>>,
) -> Result<Response, ApiError> {
    let journal = Journal::new(app, run, cursor.after, &headers, authenticated.is_some()).await?;
    let events = stream::unfold(journal, |mut journal| async move {
        let event = journal.next().await?;
        let frame = Event::default()
            .id(event.sequence.to_string())
            .event("event")
            .json_data(&event)
            .ok()?;
        Some((Ok::<_, Infallible>(frame), journal))
    });
    let mut response = Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static header"),
    );
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse().expect("static header"));
    Ok(response)
}
pub(super) async fn websocket(
    State(app): State<App>,
    Path(run): Path<Uuid>,
    Query(cursor): Query<StreamCursor>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
    authenticated: Option<Extension<crate::auth::Authenticated>>,
) -> Result<Response, ApiError> {
    let journal = Journal::new(app, run, cursor.after, &headers, authenticated.is_some()).await?;
    Ok(upgrade
        .max_message_size(4096)
        .max_frame_size(4096)
        .write_buffer_size(16384)
        .max_write_buffer_size(1048576)
        .on_upgrade(move |socket| serve_socket(socket, journal))
        .into_response())
}
async fn serve_socket(socket: WebSocket, mut journal: Journal) {
    let (mut sender, mut receiver) = socket.split();
    loop {
        tokio::select! {
            incoming=receiver.next()=>match incoming {
                Some(Ok(Message::Ping(bytes)))=>{
                    if !matches!(tokio::time::timeout(Duration::from_secs(5),sender.send(Message::Pong(bytes))).await,Ok(Ok(()))) { break; }
                },
                Some(Ok(Message::Pong(_)))=>{},
                // This is an observation channel. Mutations retain their normal
                // authenticated HTTP controls and explicit idempotency keys.
                _=>break,
            },
            next=journal.next()=>{
                let Some(event)=next else {break;};
                let Ok(text)=serde_json::to_string(&event) else {break;};
                if !matches!(tokio::time::timeout(Duration::from_secs(5),sender.send(Message::Text(text.into()))).await,Ok(Ok(()))) {break;}
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), sender.send(Message::Close(None))).await;
}
