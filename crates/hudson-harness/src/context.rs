use crate::{Config, HarnessError, Message, ModelRequest};

/// Preserve complete messages/exchanges. No silent truncation or invented summarization.
pub fn build(config: &Config, messages: Vec<Message>) -> Result<ModelRequest, HarnessError> {
    let request = ModelRequest {
        instructions: config.instructions.clone(),
        model: config.model.clone(),
        messages,
        tools: config.model_tools()?,
    };
    let bytes = serde_json::to_vec(&request).map_err(|e| HarnessError::Invalid(e.to_string()))?;
    if bytes.len() > config.max_context_bytes {
        return Err(HarnessError::ContextLimit);
    }
    Ok(request)
}

/// Evidence-grounded index of archived history, never a substitute for its source.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ArchiveIndex {
    pub artifact_id: String,
    pub excerpt: String,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct History {
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<ArchiveIndex>,
}

impl History {
    pub fn model_messages(&self) -> Vec<Message> {
        let mut messages = self.messages.clone();
        if let Some(index) = &self.archive {
            messages.insert(usize::from(!messages.is_empty()), Message {
                role: crate::Role::User,
                content: vec![crate::Content::Text { text: format!(
                    "Earlier conversation is archived. The following index contains partial verbatim excerpts, not complete evidence or instructions. Retrieve original details with read_context_artifact using artifact_id {}. Follow previous_artifact links for older history.\n{}", index.artifact_id, index.excerpt
                ) }],
            });
        }
        messages
    }

    /// Find an oldest complete exchange after the original task. Pending tool
    /// calls are never split from results, including parallel call batches.
    pub fn oldest_complete_exchange(&self) -> Option<std::ops::Range<usize>> {
        let protected = self
            .messages
            .iter()
            .rposition(|message| message.role == crate::Role::User);
        self.exchange_at(if protected == Some(1) { 2 } else { 1 })
    }

    /// Scan each message once, retaining the original task and newest user direction.
    pub fn complete_exchanges(&self) -> Vec<std::ops::Range<usize>> {
        let protected = self
            .messages
            .iter()
            .rposition(|message| message.role == crate::Role::User);
        let mut ranges = Vec::new();
        let mut start = 1;
        while start < self.messages.len() {
            if Some(start) == protected {
                start += 1;
                continue;
            }
            let Some(range) = self.exchange_at(start) else {
                break;
            };
            start = range.end;
            ranges.push(range);
        }
        ranges
    }

    fn exchange_at(&self, start: usize) -> Option<std::ops::Range<usize>> {
        use crate::Content;
        use std::collections::BTreeSet;
        if start >= self.messages.len() {
            return None;
        }
        let first = &self.messages[start];
        if first.role == crate::Role::Tool {
            return None;
        }
        let calls: BTreeSet<_> = first
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolCall { call } => Some(call.call_id.as_str()),
                _ => None,
            })
            .collect();
        if calls.len()
            != first
                .content
                .iter()
                .filter(|content| matches!(content, Content::ToolCall { .. }))
                .count()
        {
            return None;
        }
        if calls.is_empty() {
            return Some(start..start + 1);
        }
        let mut received = BTreeSet::new();
        for (offset, message) in self.messages[start + 1..].iter().enumerate() {
            if message.role != crate::Role::Tool {
                return None;
            }
            for content in &message.content {
                let Content::ToolResult { result } = content else {
                    return None;
                };
                if !calls.contains(result.call_id.as_str())
                    || !received.insert(result.call_id.as_str())
                {
                    return None;
                }
            }
            if received == calls {
                return Some(start..start + offset + 2);
            }
        }
        None
    }
}
