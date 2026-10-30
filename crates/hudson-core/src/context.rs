//! Bounded context with immutable, run-scoped source artifacts. No hidden model calls.
#[cfg(test)]
mod tests;
use crate::{
    adapters::tools::{ExecutionError, ToolRegistry},
    definitions::digest,
    models::*,
    storage::{memory::Data, MemoryStore},
    Error, Result,
};
use hudson_harness::{
    agent_loop, context::ArchiveIndex, Backend, Checkpoint, Content, Engine, HarnessError, Input,
    Message, ToolOutcome, Transition,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ContextPolicy {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reserve_bytes: usize,
    pub offload_bytes: usize,
    pub recent_exchanges: usize,
    pub excerpt_bytes: usize,
    pub page_bytes: usize,
}
impl Default for ContextPolicy {
    fn default() -> Self {
        Self {
            reserve_bytes: 0,
            offload_bytes: 8192,
            recent_exchanges: 2,
            excerpt_bytes: 1024,
            page_bytes: 1024,
        }
    }
}
impl ContextPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.reserve_bytes > 65_536
            || !(4096..=1048576).contains(&self.offload_bytes)
            || !(1..=32).contains(&self.recent_exchanges)
            || !(128..=4096).contains(&self.excerpt_bytes)
            || self.excerpt_bytes > self.offload_bytes / 4
            || self.page_bytes == 0
            || self.page_bytes > self.offload_bytes / 8
            || self.page_bytes > 4096
        {
            return Err(Error::Invalid("invalid context policy thresholds".into()));
        }
        Ok(())
    }
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextArtifact {
    pub id: String,
    pub workspace_id: String,
    pub run_id: Uuid,
    pub content: Value,
}
impl ContextArtifact {
    fn new(workspace: &str, run: Uuid, content: Value) -> Result<Self> {
        let id = digest(&(workspace, run, &content))?;
        Ok(Self {
            id,
            workspace_id: workspace.into(),
            run_id: run,
            content,
        })
    }
}

pub(crate) struct Prepared {
    pub transition: Transition,
    pub artifacts: Vec<ContextArtifact>,
    pub compaction: Option<Compaction>,
}

pub(crate) struct Control<'a> {
    pub policy: Option<&'a ContextPolicy>,
    pub compact: bool,
}

#[derive(Serialize)]
pub(crate) struct Compaction {
    history_bytes_before: usize,
    history_bytes_after: usize,
    archived_exchanges: usize,
    artifact_id: Option<String>,
}

fn archive_exchange(
    history: &mut hudson_harness::context::History,
    archived: Vec<Message>,
    artifacts: &mut Vec<ContextArtifact>,
    workspace: &str,
    run: Uuid,
    policy: &ContextPolicy,
) -> Result<()> {
    let excerpt = extract(&archived, policy.excerpt_bytes)?;
    let previous = history
        .archive
        .as_ref()
        .map(|index| index.artifact_id.clone());
    let artifact = ContextArtifact::new(
        workspace,
        run,
        json!({"kind":"conversation_exchange","previous_artifact":previous,"messages":archived,"excerpt":excerpt}),
    )?;
    history.archive = Some(ArchiveIndex {
        artifact_id: artifact.id.clone(),
        excerpt,
    });
    artifacts.push(artifact);
    Ok(())
}

/// Move each message at most once. A control archives at most 128 complete
/// exchanges; source messages, images, pending cycles and recent directions
/// retain the same protection as automatic context management.
fn compact_history(
    history: &mut hudson_harness::context::History,
    artifacts: &mut Vec<ContextArtifact>,
    workspace: &str,
    run: Uuid,
    policy: &ContextPolicy,
) -> Result<Compaction> {
    let before = serde_json::to_vec(history)?.len();
    let ranges = history.complete_exchanges();
    let count = ranges
        .len()
        .saturating_sub(policy.recent_exchanges)
        .min(128);
    let mut messages = std::mem::take(&mut history.messages).into_iter();
    let mut cursor = 0;
    for range in ranges.into_iter().take(count) {
        history
            .messages
            .extend(messages.by_ref().take(range.start - cursor));
        let archived = messages.by_ref().take(range.len()).collect();
        cursor = range.end;
        archive_exchange(history, archived, artifacts, workspace, run, policy)?;
    }
    history.messages.extend(messages);
    Ok(Compaction {
        history_bytes_before: before,
        history_bytes_after: serde_json::to_vec(history)?.len(),
        archived_exchanges: count,
        artifact_id: history
            .archive
            .as_ref()
            .map(|index| index.artifact_id.clone()),
    })
}

fn prefix(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub(crate) fn offload(
    value: &mut Value,
    artifacts: &mut Vec<ContextArtifact>,
    workspace: &str,
    run: Uuid,
    policy: &ContextPolicy,
    kind: &str,
) -> Result<()> {
    let encoded = serde_json::to_string(value)?;
    if encoded.len() <= policy.offload_bytes {
        return Ok(());
    }
    let artifact = ContextArtifact::new(workspace, run, json!({"kind":kind,"value":value}))?;
    *value = json!({"context_artifact":{"artifact_id":artifact.id,"bytes":encoded.len(),"preview":prefix(&encoded, policy.excerpt_bytes),"preview_is_partial":true,"retrieve_with":"read_context_artifact"}});
    artifacts.push(artifact);
    Ok(())
}

/// Deterministically prepare and advance the exact same loop. The host must commit
/// source artifacts and transition together; a failed/stale transition publishes neither.
pub(crate) fn advance<B: Backend>(
    engine: &Engine<B>,
    config: &hudson_harness::Config,
    checkpoint: &Checkpoint,
    mut input: Input,
    control: Control<'_>,
    workspace: &str,
    run: Uuid,
) -> Result<Prepared> {
    let Some(policy) = control.policy else {
        if control.compact {
            return Err(Error::Unsupported(
                "explicit compaction requires a context policy".into(),
            ));
        }
        return Ok(Prepared {
            transition: engine.advance(config, checkpoint, input)?,
            artifacts: vec![],
            compaction: None,
        });
    };
    policy.validate()?;
    let target = config
        .max_context_bytes
        .checked_sub(policy.reserve_bytes)
        .filter(|target| *target > 0)
        .ok_or_else(|| {
            Error::Invalid("context reserve must be smaller than the context limit".into())
        })?;
    let mut history = agent_loop::history(checkpoint)?;
    let mut artifacts = vec![];
    for message in &mut history.messages {
        for content in &mut message.content {
            if let Content::ToolResult { result } = content {
                if let ToolOutcome::Success { value } = &mut result.outcome {
                    offload(value, &mut artifacts, workspace, run, policy, "tool_output")?;
                }
            }
        }
    }
    let effective_input = match &mut input {
        Input::Queued { input, .. } => input.as_mut(),
        input => input,
    };
    if let Input::Tools { results } = effective_input {
        for result in results {
            if let ToolOutcome::Success { value } = &mut result.outcome {
                offload(value, &mut artifacts, workspace, run, policy, "tool_output")?;
            }
        }
    }
    let compaction = if control.compact {
        Some(compact_history(
            &mut history,
            &mut artifacts,
            workspace,
            run,
            policy,
        )?)
    } else {
        None
    };
    let mut complete = history.complete_exchanges().len();
    loop {
        let updated = agent_loop::with_history(checkpoint, history.clone())?;
        let fitting = match engine.advance(config, &updated, input.clone()) {
            Ok(transition)
                if serde_json::to_vec(&transition.checkpoint)?.len()
                    <= config.max_context_bytes
                    && serde_json::to_vec(&transition.action)?.len()
                        <= config.max_context_bytes =>
            {
                Some(transition)
            }
            Ok(_) | Err(HarnessError::ContextLimit) => None,
            Err(e) => return Err(e.into()),
        };
        if let Some(transition) = fitting {
            if policy.reserve_bytes == 0
                || complete <= policy.recent_exchanges
                || (serde_json::to_vec(&transition.checkpoint)?.len() <= target
                    && serde_json::to_vec(&transition.action)?.len() <= target)
            {
                return Ok(Prepared {
                    transition,
                    artifacts,
                    compaction,
                });
            }
        }
        if complete <= policy.recent_exchanges {
            return Err(HarnessError::ContextLimit.into());
        }
        let range = history
            .oldest_complete_exchange()
            .ok_or(HarnessError::ContextLimit)?;
        let archived: Vec<Message> = history.messages.drain(range).collect();
        complete -= 1;
        archive_exchange(
            &mut history,
            archived,
            &mut artifacts,
            workspace,
            run,
            policy,
        )?;
    }
}

fn extract(messages: &[Message], max: usize) -> Result<String> {
    let mut lines = Vec::new();
    for message in messages {
        for content in &message.content {
            lines.push(match content {
                Content::ToolCall { call } => format!(
                    "Tool {} (call {}), arguments excerpt: {}",
                    call.name,
                    call.call_id,
                    prefix(&serde_json::to_string(&call.arguments)?, max / 3)
                ),
                Content::ToolResult { result } => format!(
                    "Tool result {} excerpt: {}",
                    result.call_id,
                    prefix(&serde_json::to_string(&result.outcome)?, max / 3)
                ),
                Content::Text { text } => {
                    format!("{:?} excerpt: {}", message.role, prefix(text, max / 3))
                }
                Content::Json { value } => format!(
                    "{:?} excerpt: {}",
                    message.role,
                    prefix(&serde_json::to_string(value)?, max / 3)
                ),
                Content::Artifact { id, .. } => format!("Artifact {id}"),
                Content::Image { image } => format!(
                    "Image {} ({} encoded bytes)",
                    image.media_type,
                    image.data.len()
                ),
            });
        }
    }
    Ok(prefix(&lines.join("\n"), max).into())
}

pub(crate) fn commit(data: &mut Data, artifacts: &[ContextArtifact]) -> Result<()> {
    for artifact in artifacts {
        if let Some(existing) = data.context_artifacts.get(&artifact.id) {
            if existing.workspace_id != artifact.workspace_id
                || existing.run_id != artifact.run_id
                || existing.content != artifact.content
            {
                return Err(Error::Conflict("context artifact collision".into()));
            }
        } else {
            data.context_artifacts
                .insert(artifact.id.clone(), artifact.clone());
        }
    }
    Ok(())
}

impl MemoryStore {
    /// Pin enabled/disabled context behavior, just like other immutable agent configuration.
    pub fn bind_context_policy(
        &self,
        workspace: &str,
        agent: &VersionRef,
        policy: Option<&ContextPolicy>,
    ) -> Result<()> {
        if let Some(policy) = policy {
            policy.validate()?;
        }
        let expected_reader = policy
            .map(|policy| digest(&(workspace, policy)).map(|id| format!("hudson.context.{id}")))
            .transpose()?;
        self.transact(|data| {
            let definition = data.agents.get(&(workspace.to_owned(), agent.clone())).ok_or(Error::NotFound)?;
            if policy.is_some_and(|policy| policy.reserve_bytes >= definition.limits.max_context_bytes) {
                return Err(Error::Invalid("context reserve must be smaller than the context limit".into()));
            }
            if policy.is_some() && !definition.tools.iter().any(|tool| tool.alias == "read_context_artifact" && data.tools.get(&(workspace.to_owned(), tool.tool_ref.clone())).is_some_and(|tool| matches!(&tool.execution, Execution::Registered { key } if Some(key) == expected_reader.as_ref()))) {
                return Err(Error::Invalid("context management requires its registered archive reader".into()));
            }
            let key = (workspace.to_owned(), agent.clone());
            let owned = policy.cloned();
            match data.context_policies.get(&key) {
                Some(existing) if existing != &owned => Err(Error::Conflict("context policy changed; publish a new agent version".into())),
                Some(_) => Ok(()),
                None => {
                    if owned.is_some() && data.runs.values().any(|run| run.meta.workspace_id == workspace && &run.agent_ref == agent) {
                        return Err(Error::Conflict("cannot enable context after runs have started; publish a new agent version".into()));
                    }
                    data.context_policies.insert(key, owned); Ok(())
                }
            }
        })
    }

    /// Access requires the same actor, workspace AND run as the original source.
    pub fn read_context_artifact(
        &self,
        actor: &Actor,
        run: Uuid,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Value> {
        if limit == 0 || limit > 4096 {
            return Err(Error::Invalid("artifact page must be 1..4096 bytes".into()));
        }
        self.read(|data| {
            data.run(actor, run)?;
            let artifact = data.context_artifacts.get(id).filter(|artifact| artifact.workspace_id == actor.workspace_id && artifact.run_id == run).ok_or(Error::NotFound)?;
            let source = serde_json::to_string(&artifact.content)?;
            if offset > source.len() || !source.is_char_boundary(offset) { return Err(Error::Invalid("invalid artifact byte offset".into())); }
            let page = prefix(&source[offset..], limit);
            if page.is_empty() && offset < source.len() { return Err(Error::Invalid("page limit cannot fit next UTF-8 character".into())); }
            let end = offset + page.len();
            Ok(json!({"artifact_id":id,"media_type":"application/json","offset":offset,"next_offset":if end < source.len() { Some(end) } else { None },"total_bytes":source.len(),"text":page}))
        })
    }
}

/// The ordinary tool admission/permission path owns authorization. The invocation's
/// persisted operation supplies actor/run scope; neither can be chosen by model args.
pub fn register(
    registry: &mut ToolRegistry,
    store: MemoryStore,
    workspace: &str,
    policy_ref: &str,
    context: &ContextPolicy,
) -> Result<Tool> {
    context.validate()?;
    let key = format!("hudson.context.{}", digest(&(workspace, context))?);
    let workspace = workspace.to_owned();
    let limit = context.page_bytes;
    let tool = Tool { id:key.clone(), workspace_id:workspace.clone(), version:1, schema_version:SCHEMA_VERSION, name:"read_context_artifact".into(), description:"Read original archived conversation or large tool output as JSON text pages. Start at offset 0; concatenate pages using next_offset, and follow previous_artifact links for older exchanges. Excerpts are partial; retrieve source before relying on omitted details.".into(), input_schema:json!({"type":"object","properties":{"artifact_id":{"type":"string"},"offset":{"type":"integer","minimum":0}},"required":["artifact_id"],"additionalProperties":false}), output_schema:None, execution:Execution::Registered { key:key.clone() }, credential_ref:None, policy_ref:policy_ref.into(), effect:Effect::Read, created_at:now() };
    registry.register(key, move |call| {
        let read = || -> Result<Value> {
            let id = call.arguments["artifact_id"]
                .as_str()
                .ok_or_else(|| Error::Invalid("artifact ID missing".into()))?;
            let offset = match call.arguments.get("offset") {
                None => 0,
                Some(value) => value
                    .as_u64()
                    .and_then(|offset| usize::try_from(offset).ok())
                    .ok_or_else(|| Error::Invalid("invalid byte offset".into()))?,
            };
            let (actor, run) = store.read(|data| {
                let op = data
                    .operations
                    .get(&call.operation_id)
                    .ok_or(Error::NotFound)?;
                let run = data
                    .runs
                    .get(&op.run_id)
                    .filter(|run| run.meta.workspace_id == workspace)
                    .ok_or(Error::NotFound)?;
                Ok((
                    Actor {
                        id: run.actor_id.clone(),
                        workspace_id: workspace.clone(),
                    },
                    run.meta.id,
                ))
            })?;
            store.read_context_artifact(&actor, run, id, offset, limit)
        };
        read().map_err(|error| ExecutionError::Failed(error.to_string()))
    })?;
    Ok(tool)
}
