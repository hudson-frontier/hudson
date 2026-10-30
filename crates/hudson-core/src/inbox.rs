//! Durable user messages consumed only at completed harness boundaries.
use crate::{
    definitions::digest,
    models::*,
    storage::{memory::Data, Store},
    Error, Result,
};
use hudson_harness::Input;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use uuid::Uuid;

const MAX_PENDING: usize = 16;
const MAX_RECEIPTS: usize = 128;
const MAX_BYTES: usize = 65_536;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Steer,
    FollowUp,
    Compact,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageReceipt {
    pub id: Uuid,
    pub kind: MessageKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueuedMessage {
    pub receipt: MessageReceipt,
    pub value: Value,
}

#[derive(Clone, Serialize, Deserialize)]
struct Submission {
    digest: String,
    receipt: MessageReceipt,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Inbox {
    revision: u64,
    pending: VecDeque<QueuedMessage>,
    submissions: BTreeMap<String, Submission>,
}

impl Store {
    /// The key is scoped to this actor-owned run, and remains valid after consumption.
    pub fn enqueue_message(
        &self,
        actor: &Actor,
        run_id: Uuid,
        kind: MessageKind,
        value: Value,
        request_key: &str,
    ) -> Result<MessageReceipt> {
        if kind == MessageKind::Compact && !value.is_null() {
            return Err(Error::Invalid("compaction input must be null".into()));
        }
        if request_key.is_empty()
            || request_key.len() > 128
            || request_key.chars().any(char::is_control)
        {
            return Err(Error::Invalid(
                "message key must be 1..128 bytes without control characters".into(),
            ));
        }
        self.transact(|data| {
            let run = data.run(actor, run_id)?;
            if data.experiment_runs.contains_key(&run_id) {
                return Err(Error::Unsupported(
                    "experiment stage inputs are frozen; cancel or start another experiment".into(),
                ));
            }
            if run.state.backend != "hudson" || run.state.backend_version != 1 {
                return Err(Error::Unsupported(
                    "queued input requires the Hudson agent loop".into(),
                ));
            }
            crate::runtime::bounded(&value, run.limits.max_payload_bytes.min(MAX_BYTES))?;
            let fingerprint = digest(&(kind, &value))?;
            if let Some(previous) = data
                .inboxes
                .get(&run_id)
                .and_then(|inbox| inbox.submissions.get(request_key))
            {
                return if previous.digest == fingerprint {
                    Ok(previous.receipt.clone())
                } else {
                    Err(Error::Conflict(
                        "message key already names different input".into(),
                    ))
                };
            }
            if run.status.terminal() || run.status == RunStatus::Cancelling {
                return Err(Error::Conflict(
                    "cannot queue input on a finished or cancelling run".into(),
                ));
            }
            if kind == MessageKind::Compact
                && data
                    .context_policies
                    .get(&(actor.workspace_id.clone(), run.agent_ref.clone()))
                    .and_then(Option::as_ref)
                    .is_none()
            {
                return Err(Error::Unsupported(
                    "explicit compaction requires a configured context policy and archive reader"
                        .into(),
                ));
            }
            let inbox = data.inboxes.entry(run_id).or_default();
            if inbox.pending.len() >= MAX_PENDING || inbox.submissions.len() >= MAX_RECEIPTS {
                return Err(Error::Invalid(
                    "run input queue or receipt budget exhausted".into(),
                ));
            }
            let receipt = MessageReceipt {
                id: Uuid::new_v4(),
                kind,
            };
            inbox.pending.push_back(QueuedMessage {
                receipt: receipt.clone(),
                value,
            });
            crate::runtime::bounded(&inbox.pending, MAX_BYTES)?;
            inbox.submissions.insert(
                request_key.into(),
                Submission {
                    digest: fingerprint,
                    receipt: receipt.clone(),
                },
            );
            inbox.revision = inbox
                .revision
                .checked_add(1)
                .ok_or_else(|| Error::Invalid("input queue revision exhausted".into()))?;
            data.event(
                run_id,
                None,
                EventType::InputQueued,
                Some(actor.id.clone()),
                json!(receipt),
            );
            Ok(receipt)
        })
    }

    pub fn queued_messages(&self, actor: &Actor, run_id: Uuid) -> Result<Vec<QueuedMessage>> {
        self.read(|data| {
            data.run(actor, run_id)?;
            Ok(data
                .inboxes
                .get(&run_id)
                .map(|inbox| inbox.pending.iter().cloned().collect())
                .unwrap_or_default())
        })
    }

    /// Withdrawal preserves receipts, so delayed retries cannot re-enqueue dropped input.
    pub fn clear_queued_messages(&self, actor: &Actor, run_id: Uuid) -> Result<usize> {
        self.transact(|data| {
            data.run(actor, run_id)?;
            Ok(discard(data, run_id))
        })
    }
}

pub(crate) struct Prepared {
    revision: u64,
    message_ids: [Option<Uuid>; 2],
    pub compact: bool,
}

pub(crate) fn prepare(data: &Data, run_id: Uuid, input: Input) -> (Prepared, Input) {
    let inbox = data.inboxes.get(&run_id);
    let revision = inbox.map_or(0, |inbox| inbox.revision);
    let steer = matches!(
        input,
        Input::Start { .. } | Input::Tools { .. } | Input::User { .. } | Input::Verification { .. }
    );
    let follow = matches!(input, Input::Verification { passed: true, .. });
    let pending = inbox.map(|inbox| &inbox.pending);
    let selected = pending.and_then(|pending| {
        pending
            .iter()
            .find(|message| steer && message.receipt.kind == MessageKind::Steer)
            .or_else(|| {
                pending
                    .iter()
                    .find(|message| follow && message.receipt.kind == MessageKind::FollowUp)
            })
    });
    let compaction = pending.and_then(|pending| {
        pending
            .iter()
            .find(|message| steer && message.receipt.kind == MessageKind::Compact)
    });
    let message_ids = [
        selected.map(|message| message.receipt.id),
        compaction.map(|message| message.receipt.id),
    ];
    let input = if let Some(message) = selected {
        Input::Queued {
            input: Box::new(input),
            messages: vec![message.value.clone()],
        }
    } else {
        input
    };
    (
        Prepared {
            revision,
            message_ids,
            compact: compaction.is_some(),
        },
        input,
    )
}

/// Queue and checkpoint are committed together. A concurrent enqueue/withdrawal
/// retries preparation, including the final boundary, rather than losing input.
pub(crate) fn unchanged(data: &Data, run_id: Uuid, prepared: &Prepared) -> bool {
    data.inboxes.get(&run_id).map_or(0, |inbox| inbox.revision) == prepared.revision
}

pub(crate) fn consume(data: &mut Data, run_id: Uuid, prepared: &Prepared) {
    if prepared.message_ids.iter().all(Option::is_none) {
        return;
    }
    let inbox = data.inboxes.get_mut(&run_id).expect("prepared inbox");
    inbox
        .pending
        .retain(|message| !prepared.message_ids.contains(&Some(message.receipt.id)));
    // All callers validate the revision first, and at most 128 submissions plus
    // their consumption/withdrawal can increment it for a run.
    inbox.revision += 1;
    data.event(
        run_id,
        None,
        EventType::InputConsumed,
        None,
        json!({"message_ids":prepared.message_ids.iter().flatten().collect::<Vec<_>>()}),
    );
}

pub(crate) fn discard(data: &mut Data, run_id: Uuid) -> usize {
    let Some(inbox) = data.inboxes.get_mut(&run_id) else {
        return 0;
    };
    let count = inbox.pending.len();
    if count > 0 {
        inbox.pending.clear();
        inbox.revision += 1;
        data.event(
            run_id,
            None,
            EventType::InputQueueCleared,
            None,
            json!({"count":count}),
        );
    }
    count
}
