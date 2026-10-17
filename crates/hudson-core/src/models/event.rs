use super::Metadata;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    RunCreated,
    InputReceived,
    InputQueued,
    InputConsumed,
    InputQueueCleared,
    OperationRequested,
    ApprovalRequested,
    ApprovalDecided,
    OperationStarted,
    OperationSettled,
    OperationUnknown,
    RunWaiting,
    RunResumed,
    CancellationRequested,
    RunFinished,
    AssessmentRecorded,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub meta: Metadata,
    pub run_id: Uuid,
    pub operation_id: Option<Uuid>,
    pub sequence: u64,
    pub event_type: EventType,
    pub actor_id: Option<String>,
    pub payload: Value,
}
