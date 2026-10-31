//! Synchronous embedded-agent facade. See `docs/rfcs/0001-embedded-agent-sdk.md`.
use crate::{
    adapters::{
        models::ModelExecutor,
        tools::{ExecutionError, Invocation, ToolRegistry},
    },
    inbox::{MessageKind, MessageReceipt},
    models::*,
    runtime::Runtime,
    security::Policy,
    storage::Store,
    Error, Result,
};
use hudson_harness::AgentLoop;
use serde_json::Value;
use uuid::Uuid;

/// Explicit assembly for one actor and immutable agent revision.
pub struct SessionBuilder<M> {
    actor: Actor,
    agent: Agent,
    model: M,
    store: Store,
    tools: ToolRegistry,
    definitions: Vec<(Tool, Policy)>,
}

impl<M: ModelExecutor> SessionBuilder<M> {
    pub fn new(
        actor: Actor,
        id: impl Into<String>,
        instructions: impl Into<String>,
        model_name: impl Into<String>,
        model: M,
    ) -> Self {
        let id = id.into();
        Self {
            agent: Agent {
                id: id.clone(),
                name: id,
                workspace_id: actor.workspace_id.clone(),
                version: 1,
                schema_version: SCHEMA_VERSION,
                instructions: instructions.into(),
                model: model_name.into(),
                tools: Vec::new(),
                allow_user_input: false,
                input_schema: None,
                output_schema: None,
                limits: Limits::default(),
                created_at: 0,
            },
            actor,
            model,
            store: Store::default(),
            tools: ToolRegistry::new(),
            definitions: Vec::new(),
        }
    }
    pub fn with_store(mut self, store: Store) -> Self {
        self.store = store;
        self
    }
    /// Change immutable agent fields before building, including limits and revision.
    pub fn definition_mut(&mut self) -> &mut Agent {
        &mut self.agent
    }

    /// Register trusted application code and an explicit policy; no implicit grants.
    pub fn register_tool<F>(&mut self, tool: Tool, policy: Policy, handler: F) -> Result<()>
    where
        F: for<'a> FnMut(Invocation<'a>) -> std::result::Result<Value, ExecutionError>
            + Send
            + 'static,
    {
        if tool.workspace_id != self.actor.workspace_id {
            return Err(Error::Invalid("tool belongs to another workspace".into()));
        }
        let Execution::Registered { key } = &tool.execution else {
            return Err(Error::Invalid(
                "SDK registry requires a registered tool".into(),
            ));
        };
        if self.definitions.len() >= 128 {
            return Err(Error::Invalid(
                "SDK supports at most 128 registered tools".into(),
            ));
        }
        if self.definitions.iter().any(|(existing, _)| {
            existing.name == tool.name || existing.reference() == tool.reference()
        }) {
            return Err(Error::Conflict(
                "duplicate SDK tool definition or alias".into(),
            ));
        }
        crate::definitions::validate_schema(&tool.input_schema)?;
        if let Some(schema) = &tool.output_schema {
            crate::definitions::validate_schema(schema)?;
        }
        self.tools.register(key.clone(), handler)?;
        self.agent.tools.push(AgentTool {
            tool_ref: tool.reference(),
            alias: tool.name.clone(),
        });
        self.definitions.push((tool, policy));
        Ok(())
    }

    pub fn build(self) -> Result<Session<M>> {
        if self.agent.workspace_id != self.actor.workspace_id {
            return Err(Error::Invalid("agent belongs to another workspace".into()));
        }
        self.agent.limits.validate()?;
        for (tool, policy) in self.definitions {
            self.store
                .ensure_policy(&tool.workspace_id, &tool.policy_ref, policy)?;
            self.store.ensure_tool(tool)?;
        }
        let reference = self.agent.reference();
        self.store.ensure_agent(self.agent)?;
        Ok(Session {
            actor: self.actor,
            reference,
            runtime: Runtime::new(self.store, AgentLoop, self.model, self.tools),
        })
    }
}

/// Host-driven session. Methods may block; no detached execution is started.
pub struct Session<M> {
    actor: Actor,
    reference: VersionRef,
    runtime: Runtime<AgentLoop, M, ToolRegistry>,
}
impl<M: ModelExecutor> Session<M> {
    pub fn submit(&self, input: Value, request_key: String, goal: Option<Goal>) -> Result<Uuid> {
        self.runtime.submit_with_goal(
            &self.actor,
            self.reference.clone(),
            input,
            Some(request_key),
            goal,
        )
    }
    pub fn inspect(&self, run: Uuid) -> Result<RunView> {
        let view = self.runtime.store.inspect(&self.actor, run)?;
        if view.agent_ref != self.reference {
            return Err(Error::NotFound);
        }
        Ok(view)
    }
    pub fn advance(&mut self, run: Uuid) -> Result<RunView> {
        self.inspect(run)?;
        self.runtime.tick(&self.actor, run)
    }
    pub fn continue_turn(&self, previous: Uuid, input: Value, request_key: String) -> Result<Uuid> {
        self.inspect(previous)?;
        self.runtime.submit_turn(
            &self.actor,
            self.reference.clone(),
            input,
            Some(request_key),
            previous,
        )
    }
    pub fn events(&self, run: Uuid, after: u64) -> Result<Vec<Event>> {
        self.inspect(run)?;
        self.runtime
            .store
            .event_stream_page(&self.actor, run, after)
    }
    pub fn steer(&self, run: Uuid, input: Value, request_key: &str) -> Result<MessageReceipt> {
        self.inspect(run)?;
        self.runtime
            .store
            .enqueue_message(&self.actor, run, MessageKind::Steer, input, request_key)
    }
    pub fn cancel(&self, run: Uuid) -> Result<()> {
        self.inspect(run)?;
        self.runtime.cancel(&self.actor, run)
    }
    pub fn approve(
        &self,
        run: Uuid,
        operation: Uuid,
        approved: bool,
        expires_at: u64,
    ) -> Result<()> {
        self.inspect(run)?;
        let view = self
            .runtime
            .store
            .inspect_operation(&self.actor, operation)?;
        if view.run_id != run {
            return Err(Error::NotFound);
        }
        self.runtime
            .approve(&self.actor, operation, approved, now(), expires_at)
    }
    pub fn reply(&self, run: Uuid, input: Value, request_key: String, question: u64) -> Result<()> {
        self.inspect(run)?;
        self.runtime
            .provide_input(&self.actor, run, question, &request_key, input)
    }
}
