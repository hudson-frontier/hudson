use crate::{models::*, storage::MemoryStore, Error, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// serde_json's default sorted object maps make object-key order irrelevant here.
/// This is an internal digest format, not an implementation of a public canonical-JSON standard.
pub fn digest(value: &impl serde::Serialize) -> Result<String> {
    let bytes = Sha256::digest(serde_json::to_vec(value)?);
    let mut encoded = String::with_capacity(bytes.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(encoded)
}

fn schema_allowed(value: &Value) -> Result<()> {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if matches!(key.as_str(), "$ref" | "$dynamicRef")
                    && value.as_str().is_some_and(|s| !s.starts_with('#'))
                {
                    return Err(Error::Invalid(
                        "external schema references are disabled".into(),
                    ));
                }
                schema_allowed(value)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                schema_allowed(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn validate_schema(schema: &Value) -> Result<()> {
    schema_allowed(schema)?;
    jsonschema::validator_for(schema).map_err(|_| Error::Invalid("invalid JSON Schema".into()))?;
    Ok(())
}

pub fn validate(schema: &Value, value: &Value) -> Result<()> {
    schema_allowed(schema)?;
    let validator = jsonschema::validator_for(schema)
        .map_err(|_| Error::Invalid("invalid JSON Schema".into()))?;
    if !validator.is_valid(value) {
        return Err(Error::Invalid("schema validation failed".into()));
    }
    Ok(())
}

fn identity(id: &str, workspace: &str, version: u32, schema_version: u32) -> Result<()> {
    if id.trim().is_empty()
        || workspace.trim().is_empty()
        || version == 0
        || schema_version != SCHEMA_VERSION
    {
        return Err(Error::Invalid("invalid definition identity/version".into()));
    }
    Ok(())
}

impl MemoryStore {
    /// Trusted administrative API. Remote callers require an authenticated authorization layer.
    pub fn publish_tool(&self, tool: Tool) -> Result<()> {
        self.store_tool(tool, false)
    }

    fn store_tool(&self, tool: Tool, accept_identical: bool) -> Result<()> {
        identity(
            &tool.id,
            &tool.workspace_id,
            tool.version,
            tool.schema_version,
        )?;
        if tool.name.trim().is_empty() || tool.policy_ref.trim().is_empty() {
            return Err(Error::Invalid("tool name and policy are required".into()));
        }
        validate_schema(&tool.input_schema)?;
        if let Some(schema) = &tool.output_schema {
            validate_schema(schema)?;
        }
        self.transact(|d| {
            let key = (tool.workspace_id.clone(), tool.reference());
            if let Some(existing) = d.tools.get(&key) {
                if accept_identical && existing == &tool {
                    return Ok(());
                }
                return Err(Error::Conflict(
                    "tool version already published with immutable content".into(),
                ));
            }
            d.tools.insert(key, tool);
            Ok(())
        })
    }

    pub fn publish_agent(&self, agent: Agent) -> Result<()> {
        self.store_agent(agent, false)
    }

    fn store_agent(&self, agent: Agent, accept_identical: bool) -> Result<()> {
        identity(
            &agent.id,
            &agent.workspace_id,
            agent.version,
            agent.schema_version,
        )?;
        if agent.instructions.trim().is_empty() || agent.model.trim().is_empty() {
            return Err(Error::Invalid(
                "agent instructions and model are required".into(),
            ));
        }
        agent.limits.validate()?;
        if let Some(s) = &agent.input_schema {
            validate_schema(s)?;
        }
        if let Some(s) = &agent.output_schema {
            validate_schema(s)?;
        }
        let mut aliases = BTreeSet::new();
        for binding in &agent.tools {
            if binding.alias.trim().is_empty()
                || !aliases.insert(&binding.alias)
                || (agent.allow_user_input && binding.alias == "ask_user")
            {
                return Err(Error::Invalid(
                    "tool aliases must be nonempty, unique, and not reserved by user input".into(),
                ));
            }
        }
        self.transact(|d| {
            let key = (agent.workspace_id.clone(), agent.reference());
            if let Some(existing) = d.agents.get(&key) {
                if accept_identical && existing == &agent {
                    return Ok(());
                }
                return Err(Error::Conflict(
                    "agent version already published with immutable content".into(),
                ));
            }
            for binding in &agent.tools {
                if !d
                    .tools
                    .contains_key(&(agent.workspace_id.clone(), binding.tool_ref.clone()))
                {
                    return Err(Error::NotFound);
                }
            }
            d.agents.insert(key, agent);
            Ok(())
        })
    }
}

impl MemoryStore {
    /// Publish once, or accept identical content in the same atomic transaction.
    /// Concurrent startup cannot race a separate read against another publisher.
    pub fn ensure_agent(&self, agent: Agent) -> Result<()> {
        self.store_agent(agent, true)
    }
    pub fn ensure_tool(&self, tool: Tool) -> Result<()> {
        self.store_tool(tool, true)
    }
}
