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
