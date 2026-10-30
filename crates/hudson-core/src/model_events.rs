//! Bounded provisional output journal. IO remains outside store transactions;
//! the exact attempt is checked again whenever a buffered chunk is committed.
use crate::{
    adapters::{
        models::{ModelCallContext, ModelObserver},
        tools::ExecutionError,
    },
    models::{Actor, EventType, OperationStatus, RunStatus},
    storage::Store,
    Error,
};
use serde_json::json;
use std::time::{Duration, Instant};

const CHUNK_BYTES: usize = 4096;
const MAX_STREAM_BYTES: usize = 4 * 1024 * 1024;
const MAX_EVENTS: usize = 4096;
const FLUSH_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) struct RunModelObserver<'a> {
    store: &'a Store,
    actor: &'a Actor,
    context: ModelCallContext,
    buffer: String,
    bytes: usize,
    events: usize,
    limit: usize,
    last_flush: Option<Instant>,
}
fn unavailable() -> ExecutionError {
    ExecutionError::Unknown(
        "model event journal unavailable or attempt changed; no automatic retry".into(),
    )
}
impl<'a> RunModelObserver<'a> {
    pub fn new(
        store: &'a Store,
        actor: &'a Actor,
        context: ModelCallContext,
        limit: usize,
    ) -> Self {
        Self {
            store,
            actor,
            context,
            buffer: String::new(),
            bytes: 0,
            events: 0,
            limit: limit.min(MAX_STREAM_BYTES),
            last_flush: None,
        }
    }
    fn flush(&mut self) -> Result<(), ExecutionError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        if self.events >= MAX_EVENTS {
            return Err(ExecutionError::Failed(
                "model stream event limit exceeded".into(),
            ));
        }
        let text = std::mem::take(&mut self.buffer);
        let context = self.context;
        let committed = self
            .store
            .transact(|data| {
                let run = data.run(self.actor, context.run_id)?;
                if run.status.terminal() || run.status == RunStatus::Cancelling {
                    return Ok(false);
                }
                let operation = data
                    .operations
                    .get(&context.operation_id)
                    .ok_or(Error::NotFound)?;
                if operation.run_id != context.run_id
                    || operation.status != OperationStatus::Running
                    || operation.attempts.last().map(|a| a.id) != Some(context.attempt_id)
                {
                    return Err(Error::Conflict("model event attempt changed".into()));
                }
                data.event(
                    context.run_id,
                    Some(context.operation_id),
                    EventType::ModelOutputDelta,
                    None,
                    json!({"attempt_id":context.attempt_id,"text":text,"provisional":true}),
                );
                Ok(true)
            })
            .map_err(|_| unavailable())?;
        if committed {
            self.events += 1;
        }
        self.last_flush = Some(Instant::now());
        Ok(())
    }
    pub fn finish(&mut self) -> Result<(), ExecutionError> {
        self.flush()
    }
}
impl ModelObserver for RunModelObserver<'_> {
    fn runtime_context(
        &self,
        request: &hudson_harness::ModelRequest,
    ) -> Result<Option<crate::adapters::models::RuntimeCallContext>, ExecutionError> {
        use crate::adapters::models::{RuntimeCallContext, RuntimeCheckpoint, RuntimeHistoryMode};
        use crate::models::OperationRequest;
        self.store.read(|data| {
            let run=data.run(self.actor,self.context.run_id)?;
            let current=data.operations.get(&self.context.operation_id).ok_or(Error::NotFound)?;
            if run.status.terminal() || run.status==RunStatus::Cancelling || current.status!=OperationStatus::Running
                || current.attempts.last().map(|a|a.id)!=Some(self.context.attempt_id)
                || !matches!(&current.request,OperationRequest::Model {request:expected} if expected==request) {
                return Err(Error::Conflict("runtime invocation context changed".into()));
            }
            let previous_for=|id,step|data.events.get(&id).into_iter().flatten().rev().filter_map(|event|event.operation_id)
                .filter_map(|id|data.operations.get(&id)).find(|op|op.run_id==id && op.step_index<step && op.status==OperationStatus::Succeeded && matches!(op.request,OperationRequest::Model {..}));
            let previous=previous_for(self.context.run_id,current.step_index).or_else(|| {
                let source=data.events.get(&self.context.run_id)?.first()?.payload.get("source_run")?.as_str()?.parse().ok()?;
                let source_run=data.run(self.actor,source).ok()?;
                if source_run.status!=RunStatus::Completed || source_run.agent_ref!=run.agent_ref {return None;}
                previous_for(source,u64::MAX)
            });
            let mut mode=RuntimeHistoryMode::Replace;
            let previous=previous.map(|op| {
                let OperationRequest::Model {request:before}=&op.request else {unreachable!()};
                if before.model==request.model && before.instructions==request.instructions && before.tools==request.tools && request.messages.starts_with(&before.messages) {mode=RuntimeHistoryMode::Append;}
                Ok::<_,Error>(RuntimeCheckpoint {run_id:op.run_id,operation_id:op.meta.id,attempt_id:op.attempts.last().ok_or(Error::NotFound)?.id,
                    request_fingerprint:crate::definitions::digest(before)?,input_messages:before.messages.len()})
            }).transpose()?;
            Ok(Some(RuntimeCallContext {call:self.context,actor:self.actor.clone(),previous,history_mode:mode,request_fingerprint:crate::definitions::digest(request)?}))
        }).map_err(|_|unavailable())
    }
    fn context(&self) -> Option<ModelCallContext> {
        Some(self.context)
    }
    fn cancelled(&self) -> bool {
        self.store
            .read(|data| {
                let run = data.run(self.actor, self.context.run_id)?;
                let operation = data
                    .operations
                    .get(&self.context.operation_id)
                    .ok_or(Error::NotFound)?;
                Ok(run.status.terminal()
                    || run.status == RunStatus::Cancelling
                    || operation.run_id != self.context.run_id
                    || operation.status != OperationStatus::Running
                    || operation.attempts.last().map(|a| a.id) != Some(self.context.attempt_id))
            })
            .unwrap_or(true)
    }
    fn text_delta(&mut self, mut text: &str) -> Result<(), ExecutionError> {
        if text.len() > self.limit - self.bytes {
            return Err(ExecutionError::Failed(
                "model stream byte limit exceeded".into(),
            ));
        }
        self.bytes += text.len();
        while !text.is_empty() {
            let mut length = text.len().min(CHUNK_BYTES - self.buffer.len());
            while !text.is_char_boundary(length) {
                length -= 1;
            }
            if length == 0 {
                self.flush()?;
                continue;
            }
            self.buffer.push_str(&text[..length]);
            text = &text[length..];
            if self.buffer.len() == CHUNK_BYTES || self.last_flush.is_none() {
                self.flush()?;
            }
        }
        self.heartbeat()
    }
    fn heartbeat(&mut self) -> Result<(), ExecutionError> {
        if self
            .last_flush
            .is_none_or(|time| time.elapsed() >= FLUSH_INTERVAL)
        {
            self.flush()?;
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "fixtures"))]
mod tests {
    use super::*;
    use crate::{fixtures, models::Attempt};
    use uuid::Uuid;

    #[test]
    fn replaced_attempt_and_cancellation_cannot_flush_buffered_text() {
        for cancelled in [false, true] {
            let mut runtime = fixtures::runtime().unwrap();
            let actor = fixtures::actor();
            let run = runtime
                .submit(
                    &actor,
                    fixtures::agent_ref(),
                    json!({"order_id":"123","action":"lookup"}),
                    None,
                )
                .unwrap();
            runtime.tick(&actor, run).unwrap();
            let operation = runtime.store.operations(&actor, run).unwrap()[0].meta.id;
            let attempt = Uuid::new_v4();
            runtime
                .store
                .transact(|data| {
                    let op = data.operations.get_mut(&operation).unwrap();
                    op.status = OperationStatus::Running;
                    op.attempts.push(Attempt {
                        id: attempt,
                        number: 1,
                        started_at: 0,
                        finished_at: None,
                        status: OperationStatus::Running,
                        error: None,
                        token_usage: None,
                    });
                    Ok(())
                })
                .unwrap();
            let mut observer = RunModelObserver::new(
                &runtime.store,
                &actor,
                ModelCallContext {
                    run_id: run,
                    operation_id: operation,
                    attempt_id: attempt,
                },
                65536,
            );
            observer.text_delta("first").unwrap();
            observer.text_delta("buffered").unwrap();
            let before = runtime.store.events(&actor, run, 0).unwrap().len();
            runtime
                .store
                .transact(|data| {
                    if cancelled {
                        data.runs.get_mut(&run).unwrap().status = RunStatus::Cancelling;
                    } else {
                        data.operations
                            .get_mut(&operation)
                            .unwrap()
                            .attempts
                            .last_mut()
                            .unwrap()
                            .id = Uuid::new_v4();
                    }
                    Ok(())
                })
                .unwrap();
            assert!(observer.cancelled());
            assert_eq!(observer.finish().is_ok(), cancelled);
            assert_eq!(runtime.store.events(&actor, run, 0).unwrap().len(), before);
        }
    }

    #[test]
    fn stream_pages_bound_bytes_and_allow_one_large_event_to_progress() {
        let runtime = fixtures::runtime().unwrap();
        let actor = fixtures::actor();
        let run = runtime
            .submit(
                &actor,
                fixtures::agent_ref(),
                json!({"order_id":"123","action":"lookup"}),
                None,
            )
            .unwrap();
        runtime
            .store
            .transact(|data| {
                for _ in 0..40 {
                    data.event(
                        run,
                        None,
                        EventType::ModelOutputDelta,
                        None,
                        json!({"text":"x".repeat(8192)}),
                    );
                }
                data.event(
                    run,
                    None,
                    EventType::ModelOutputDelta,
                    None,
                    json!({"text":"x".repeat(200000)}),
                );
                Ok(())
            })
            .unwrap();
        let page = runtime.store.event_stream_page(&actor, run, 0).unwrap();
        assert!(!page.is_empty() && page.len() < 32);
        assert!(
            page.iter()
                .map(|e| serde_json::to_vec(e).unwrap().len())
                .sum::<usize>()
                <= 128 * 1024
        );
        let page = runtime.store.event_stream_page(&actor, run, 41).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].sequence, 42);
    }
}
