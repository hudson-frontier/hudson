use crate::{
    adapters::{
        anthropic::AnthropicModel, chat::ChatModel, http_tools::HttpTools, models::ModelExecutor,
        tools::ToolRegistry,
    },
    models::*,
    runtime::Runtime,
    security::Policy,
    skills::{Skill, SkillCatalog, SkillPackage},
    storage::Store,
};
use hudson_harness::AgentLoop;
use serde::{Deserialize, Serialize};
mod experiments;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    publication_sources: Option<serde_json::Value>,
    name: String,
    instructions: String,
    #[serde(default)]
    coordination: crate::coordination::CoordinationPolicy,
    #[serde(default)]
    context: Option<crate::context::ContextPolicy>,
    #[serde(default)]
    memory: Option<crate::memory::MemoryConfig>,
    #[serde(default)]
    allow_user_input: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runtime: Option<crate::adapters::runtime_bridge::BridgeConfig>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default = "endpoint")]
    endpoint: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default = "output_tokens")]
    max_output_tokens: u32,
    #[serde(default)]
    legacy_token_limit: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning: Option<crate::adapters::reasoning::Reasoning>,
    #[serde(default = "version")]
    version: u32,
    #[serde(default)]
    skills: Vec<Skill>,
    #[serde(default)]
    skill_files: Vec<SkillFile>,
    #[serde(default)]
    skill_packages: Vec<std::path::PathBuf>,
    #[serde(skip)]
    loaded_packages: Vec<SkillPackage>,
    #[serde(default)]
    mcp_servers: Vec<ConfiguredMcpServer>,
    #[serde(default)]
    output_schema: Option<serde_json::Value>,
    #[serde(default)]
    input_schema: Option<serde_json::Value>,
    #[serde(default)]
    limits: Limits,
    #[serde(default)]
    goal: Option<Goal>,
    #[serde(default)]
    http_tools: Vec<HttpTool>,
    #[serde(default)]
    shared_model_budget: Option<SharedBudget>,
    #[serde(default)]
    subagents: Vec<Definition>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillFile {
    name: String,
    description: String,
    path: std::path::PathBuf,
}

fn load_skill_files(
    definition: &mut Definition,
    base: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    for source in std::mem::take(&mut definition.skill_files) {
        let path = if source.path.is_absolute() {
            source.path
        } else {
            base.join(source.path)
        };
        definition.skills.push(Skill::from_markdown(
            source.name,
            source.description,
            &path,
        )?);
    }
    for path in std::mem::take(&mut definition.skill_packages) {
        let path = if path.is_absolute() {
            path
        } else {
            base.join(path)
        };
        definition.loaded_packages.push(SkillPackage::load(&path)?);
    }
    if !definition.skills.is_empty() || !definition.loaded_packages.is_empty() {
        SkillCatalog::with_packages(
            definition.skills.clone(),
            definition.loaded_packages.clone(),
        )?;
    }
    for child in &mut definition.subagents {
        load_skill_files(child, base)?;
    }
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedBudget {
    group: String,
    limit: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpTool {
    name: String,
    description: String,
    endpoint: String,
    input_schema: serde_json::Value,
    #[serde(default)]
    output_schema: Option<serde_json::Value>,
    effect: Effect,
    #[serde(default)]
    token_env: Option<String>,
    #[serde(default)]
    require_approval: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredMcpServer {
    endpoint: String,
    #[serde(default)]
    bearer_env: Option<String>,
    #[serde(default = "mcp_timeout")]
    timeout_seconds: u64,
    #[serde(default = "mcp_output_limit")]
    max_response_bytes: usize,
    #[serde(default)]
    require_approval: Option<bool>,
    tools: Vec<ConfiguredMcpTool>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredMcpTool {
    name: String,
    remote_name: String,
    description: String,
    input_schema: serde_json::Value,
    effect: Effect,
    #[serde(default)]
    require_approval: Option<bool>,
}
fn mcp_timeout() -> u64 {
    60
}
fn mcp_output_limit() -> usize {
    1_048_576
}
impl ConfiguredMcpServer {
    fn adapter_config(&self) -> crate::adapters::mcp::McpServerConfig {
        crate::adapters::mcp::McpServerConfig {
            endpoint: self.endpoint.clone(),
            bearer_env: self.bearer_env.clone(),
            timeout_seconds: self.timeout_seconds,
            max_response_bytes: self.max_response_bytes,
            tools: self
                .tools
                .iter()
                .map(|tool| crate::adapters::mcp::McpToolBinding {
                    name: tool.name.clone(),
                    remote_name: tool.remote_name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                    effect: tool.effect.clone(),
                })
                .collect(),
        }
    }
}

fn output_tokens() -> u32 {
    1024
}

fn selected_model(definition: &Definition) -> Result<&str, Box<dyn std::error::Error>> {
    match definition.model.as_deref() {
        Some(model) if !model.trim().is_empty() => Ok(model),
        Some(_) => Err("model must not be empty".into()),
        None if definition.runtime.is_none()
            && matches!(definition.provider.as_str(), "" | "openai") =>
        {
            Ok("gpt-4.1-mini")
        }
        None => Err("set a model for the selected provider explicitly".into()),
    }
}
fn endpoint() -> String {
    "https://api.openai.com/v1/chat/completions".into()
}
fn version() -> u32 {
    1
}

fn model_key(name: &str, required: bool) -> Result<Option<String>, Box<dyn std::error::Error>> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        _ if required => Err("configured model credential is missing or empty".into()),
        _ => Ok(None),
    }
}

pub type AgentRuntime = Runtime<AgentLoop, Box<dyn ModelExecutor>, HttpTools>;
type ChildRuntimes =
    std::collections::BTreeMap<VersionRef, std::sync::Arc<std::sync::Mutex<AgentRuntime>>>;

/// One configured tree, preserving each agent's own model/tool executor on resume.
pub struct ConfiguredTree {
    pub runtime: AgentRuntime,
    pub reference: VersionRef,
    pub goal: Option<Goal>,
    pub execution_catalog: Vec<crate::adapters::runtime_bridge::ExecutionDescriptor>,
    children: ChildRuntimes,
}
impl ConfiguredTree {
    pub fn bindings(&self) -> std::collections::BTreeMap<VersionRef, Option<Goal>> {
        let mut bindings = self
            .children
            .keys()
            .cloned()
            .map(|reference| (reference, None))
            .collect::<std::collections::BTreeMap<_, _>>();
        bindings.insert(self.reference.clone(), self.goal.clone());
        bindings
    }
    pub fn tick(&mut self, actor: &Actor, id: uuid::Uuid) -> crate::Result<RunView> {
        let run = self.runtime.store.inspect(actor, id)?;
        if run.agent_ref == self.reference {
            self.runtime
                .store
                .validate_resume(actor, id, &self.reference, &self.goal)?;
            return self.runtime.tick(actor, id);
        }
        let child = self.children.get(&run.agent_ref).ok_or_else(|| {
            crate::Error::Conflict("agent version is not configured in this host".into())
        })?;
        self.runtime
            .store
            .validate_resume(actor, id, &run.agent_ref, &None)?;
        child
            .lock()
            .map_err(|_| crate::Error::Conflict("child runtime unavailable".into()))?
            .tick(actor, id)
    }
}

/// Independently locked agent executors for a durable scheduler. Different team
/// members can execute concurrently while each configured executor stays serial.
pub struct ScheduledTree {
    pub store: Store,
    runtimes: ChildRuntimes,
    bindings: std::collections::BTreeMap<VersionRef, Option<Goal>>,
}
impl ConfiguredTree {
    pub fn into_scheduled(self) -> ScheduledTree {
        let bindings = self.bindings();
        let store = self.runtime.store.clone();
        let mut runtimes = self.children;
        runtimes.insert(
            self.reference,
            std::sync::Arc::new(std::sync::Mutex::new(self.runtime)),
        );
        ScheduledTree {
            store,
            runtimes,
            bindings,
        }
    }
}
impl ScheduledTree {
    pub fn tick(&self, actor: &Actor, id: uuid::Uuid) -> crate::Result<RunView> {
        let run = self.store.inspect(actor, id)?;
        let goal = self
            .bindings
            .get(&run.agent_ref)
            .ok_or_else(|| crate::Error::Conflict("agent version is not configured".into()))?;
        self.store
            .validate_resume(actor, id, &run.agent_ref, goal)?;
        self.runtimes
            .get(&run.agent_ref)
            .ok_or(crate::Error::NotFound)?
            .lock()
            .map_err(|_| crate::Error::Conflict("agent runtime unavailable".into()))?
            .tick(actor, id)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BuildMode {
    Local,
    Experiment,
    Temporal,
    Admission,
}

struct AdmissionModel;
impl ModelExecutor for AdmissionModel {
    fn call(
        &mut self,
        _: &hudson_harness::ModelRequest,
    ) -> Result<hudson_harness::ModelResponse, crate::adapters::tools::ExecutionError> {
        Err(crate::adapters::tools::ExecutionError::Failed(
            "admission host cannot execute model calls".into(),
        ))
    }
}

fn build(
    definition: Definition,
    store: Store,
    actor: &Actor,
    budget: Option<&SharedBudget>,
    children: &mut ChildRuntimes,
    mode: BuildMode,
) -> Result<(AgentRuntime, VersionRef), Box<dyn std::error::Error>> {
    let deferred = matches!(mode, BuildMode::Temporal | BuildMode::Admission);
    let admission = mode == BuildMode::Admission;
    let credential = |name: &str, required: bool| {
        if admission || mode == BuildMode::Experiment {
            Ok(None)
        } else {
            model_key(name, required)
        }
    };
    let chat_model = |url: &str, key: Option<String>, max_tokens| {
        if mode == BuildMode::Experiment {
            ChatModel::fixture(url, max_tokens)
        } else {
            ChatModel::new(url, key, max_tokens)
        }
    };
    let selected_model = selected_model(&definition)?.to_owned();
    let is_team = !definition.subagents.is_empty();
    let transport_fingerprint = crate::definitions::digest(&(
        &definition.provider,
        &definition.endpoint,
        definition.max_output_tokens,
        definition.legacy_token_limit,
    ))?;
    let transport_fingerprint = match &definition.reasoning {
        Some(reasoning) => crate::definitions::digest(&(&transport_fingerprint, reasoning))?,
        None => transport_fingerprint,
    };
    let transport_fingerprint = match &definition.runtime {
        Some(runtime) => crate::definitions::digest(&(&transport_fingerprint, runtime))?,
        None => transport_fingerprint,
    };
    let transport_fingerprint = if mode == BuildMode::Experiment {
        crate::definitions::digest(&(&transport_fingerprint, "offline-fixture-v1"))?
    } else {
        transport_fingerprint
    };
    let model: Box<dyn ModelExecutor> = if let Some(runtime) = &definition.runtime {
        if mode == BuildMode::Experiment {
            return Err("external runtime bridges are unavailable in offline experiments".into());
        }
        if admission {
            Box::new(AdmissionModel)
        } else {
            Box::new(crate::adapters::runtime_bridge::BridgeModel::new(
                runtime.clone(),
                definition.max_output_tokens,
            )?)
        }
    } else {
        match definition.provider.as_str() {
            "" | "openai" => {
                let key = credential(
                    definition
                        .api_key_env
                        .as_deref()
                        .unwrap_or("OPENAI_API_KEY"),
                    definition.api_key_env.is_some() || definition.endpoint == endpoint(),
                )?;
                Box::new(
                    chat_model(&definition.endpoint, key, definition.max_output_tokens)?
                        .with_legacy_token_limit(definition.legacy_token_limit)
                        .with_reasoning(definition.reasoning.clone())?,
                )
            }
            "anthropic" => {
                let url = if definition.endpoint == endpoint() {
                    "https://api.anthropic.com/v1/messages"
                } else {
                    &definition.endpoint
                };
                let key = credential(
                    definition
                        .api_key_env
                        .as_deref()
                        .unwrap_or("ANTHROPIC_API_KEY"),
                    definition.api_key_env.is_some()
                        || url == "https://api.anthropic.com/v1/messages",
                )?;
                Box::new(
                    AnthropicModel::new(url, key, definition.max_output_tokens)?
                        .with_reasoning(definition.reasoning.clone())?,
                )
            }
            "gemini" | "ollama" => {
                let default_url = if definition.provider == "gemini" {
                    "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
                } else {
                    "http://127.0.0.1:11434/v1/chat/completions"
                };
                let url = if definition.endpoint == endpoint() {
                    default_url
                } else {
                    &definition.endpoint
                };
                let key_env =
                    definition
                        .api_key_env
                        .as_deref()
                        .or(if definition.provider == "gemini" {
                            Some("GEMINI_API_KEY")
                        } else {
                            None
                        });
                let key = key_env
                    .map(|name| credential(name, true))
                    .transpose()?
                    .flatten();
                Box::new(
                    chat_model(url, key, definition.max_output_tokens)?
                        .with_legacy_token_limit(true)
                        .with_reasoning(definition.reasoning.clone())?,
                )
            }
            _ => return Err("unsupported provider".into()),
        }
    };
    // Construct adapters above to retain their configuration validation, then discard
    // them without any network calls. Admission never retains an executable model.
    let model: Box<dyn ModelExecutor> = if admission {
        Box::new(AdmissionModel)
    } else {
        model
    };
    let model: Box<dyn ModelExecutor> = if let Some(budget) = budget {
        Box::new(crate::budgets::BudgetedModel::new(
            model,
            store.clone(),
            &actor.workspace_id,
            &budget.group,
            budget.limit,
        )?)
    } else {
        model
    };
    let mut registry = ToolRegistry::new();
    let mut bindings = Vec::new();
    if let Some(memory) = &definition.memory {
        let read_policy = format!("memory-read:{}:{}", definition.name, definition.version);
        let write_policy = format!("memory-write:{}:{}", definition.name, definition.version);
        for mut tool in crate::memory::register_tools(
            &mut registry,
            store.clone(),
            actor.clone(),
            memory.scope.clone(),
            &read_policy,
            &write_policy,
        )? {
            tool.id = format!("{}:memory:{}", definition.name, tool.name);
            tool.version = definition.version;
            tool.created_at = 0;
            bindings.push(AgentTool {
                tool_ref: tool.reference(),
                alias: tool.name.clone(),
            });
            store.ensure_tool(tool)?;
        }
        for (policy, require_approval) in [(&read_policy, false), (&write_policy, true)] {
            store.ensure_policy(
                &actor.workspace_id,
                policy,
                Policy {
                    actors: [actor.id.clone()].into(),
                    approvers: [actor.id.clone()].into(),
                    require_approval,
                },
            )?;
        }
    }

    if let Some(context) = &definition.context {
        let policy_ref = format!("context:{}:{}", definition.name, definition.version);
        let mut tool = crate::context::register(
            &mut registry,
            store.clone(),
            &actor.workspace_id,
            &policy_ref,
            context,
        )?;
        tool.id = format!("{}:context:{}", definition.name, tool.name);
        tool.version = definition.version;
        tool.created_at = 0;
        bindings.push(AgentTool {
            tool_ref: tool.reference(),
            alias: tool.name.clone(),
        });
        store.ensure_tool(tool)?;
        store.ensure_policy(
            &actor.workspace_id,
            &policy_ref,
            Policy {
                actors: [actor.id.clone()].into(),
                approvers: Default::default(),
                require_approval: false,
            },
        )?;
    }

    if !definition.skills.is_empty() || !definition.loaded_packages.is_empty() {
        let mut tool = SkillCatalog::with_packages(definition.skills, definition.loaded_packages)?
            .register(&mut registry, &actor.workspace_id, "skills")?;
        tool.created_at = 0;
        bindings.push(AgentTool {
            tool_ref: tool.reference(),
            alias: tool.name.clone(),
        });
        store.ensure_tool(tool)?;
        store.ensure_policy(
            &actor.workspace_id,
            "skills",
            Policy {
                actors: [actor.id.clone()].into(),
                approvers: Default::default(),
                require_approval: false,
            },
        )?;
    }
    for server in definition.mcp_servers {
        let config = server.adapter_config();
        for (mut tool, spec) in crate::adapters::mcp::register_tools(
            &config,
            &mut registry,
            &actor.workspace_id,
            "mcp-pending",
        )?
        .into_iter()
        .zip(&server.tools)
        {
            tool.id = format!("{}:mcp:{}", definition.name, spec.name);
            tool.version = definition.version;
            tool.created_at = 0;
            tool.policy_ref = format!(
                "mcp:{}:{}:{}",
                definition.name, definition.version, spec.name
            );
            store.ensure_policy(
                &actor.workspace_id,
                &tool.policy_ref,
                Policy {
                    actors: [actor.id.clone()].into(),
                    approvers: [actor.id.clone()].into(),
                    require_approval: spec
                        .require_approval
                        .or(server.require_approval)
                        .unwrap_or(spec.effect == Effect::Write),
                },
            )?;
            bindings.push(AgentTool {
                tool_ref: tool.reference(),
                alias: tool.name.clone(),
            });
            store.ensure_tool(tool)?;
        }
    }
    for child_definition in definition.subagents {
        let alias = format!("delegate_{}", child_definition.name);
        let description = format!("Delegate a task to specialist {}. Inspect its returned status before claiming completion.", child_definition.name);
        let (child, child_ref) = build(
            child_definition,
            store.clone(),
            actor,
            budget,
            children,
            mode,
        )?;
        let child = std::sync::Arc::new(std::sync::Mutex::new(child));
        children.insert(child_ref.clone(), child.clone());
        let delegation_tools = crate::subagents::register_shared(
            &mut registry,
            child,
            actor.clone(),
            child_ref,
            &alias,
            &description,
            "configured-subagents",
            deferred,
        )?;
        for tool in delegation_tools {
            bindings.push(AgentTool {
                tool_ref: tool.reference(),
                alias: tool.name.clone(),
            });
            store.ensure_tool(tool)?;
        }
    }
    store.ensure_policy(
        &actor.workspace_id,
        "configured-subagents",
        Policy {
            actors: [actor.id.clone()].into(),
            approvers: Default::default(),
            require_approval: false,
        },
    )?;
    let mut executor = if mode == BuildMode::Experiment {
        HttpTools::fixture(registry)?
    } else {
        HttpTools::new(registry)?
    };
    for spec in definition.http_tools {
        let token = if admission {
            None
        } else {
            spec.token_env
                .as_ref()
                .map(|name| {
                    std::env::var(name)
                        .map_err(|_| "HTTP tool credential environment variable missing")
                })
                .transpose()?
        };
        executor.bind(&spec.endpoint, token)?;
        let tool = Tool {
            id: format!("{}:{}", definition.name, spec.name),
            workspace_id: actor.workspace_id.clone(),
            version: definition.version,
            schema_version: SCHEMA_VERSION,
            name: spec.name.clone(),
            description: spec.description,
            input_schema: spec.input_schema,
            output_schema: spec.output_schema,
            execution: Execution::Http {
                endpoint: spec.endpoint,
            },
            credential_ref: spec.token_env,
            policy_ref: format!(
                "http:{}:{}:{}",
                definition.name, definition.version, spec.name
            ),
            effect: spec.effect,
            created_at: 0,
        };
        bindings.push(AgentTool {
            tool_ref: tool.reference(),
            alias: tool.name.clone(),
        });
        store.ensure_policy(
            &actor.workspace_id,
            &tool.policy_ref,
            Policy {
                actors: [actor.id.clone()].into(),
                approvers: [actor.id.clone()].into(),
                require_approval: spec.require_approval,
            },
        )?;
        store.ensure_tool(tool)?;
    }
    let agent = Agent {
        id: definition.name.clone(),
        workspace_id: actor.workspace_id.clone(),
        version: definition.version,
        schema_version: SCHEMA_VERSION,
        name: definition.name,
        instructions: if deferred && is_team {
            format!("{}\n\nDelegation submits durable child runs. The scheduler waits for the delegated team before your next model turn. Use each join_delegate tool with the returned child_run ID to retrieve the completed result; the original delegation receipt only reports submission. Do not claim success without inspecting each child result.", definition.instructions)
        } else {
            definition.instructions
        },
        allow_user_input: definition.allow_user_input,
        model: selected_model,
        tools: bindings,
        input_schema: definition.input_schema,
        output_schema: definition.output_schema,
        limits: definition.limits,
        created_at: 0,
    };
    let reference = agent.reference();
    store.ensure_agent(agent)?;
    if is_team {
        store.bind_coordination_policy(
            &actor.workspace_id,
            &reference,
            &definition.coordination,
        )?;
    }
    store.bind_memory(&actor.workspace_id, &reference, definition.memory)?;
    store.bind_context_policy(&actor.workspace_id, &reference, definition.context.as_ref())?;
    store.bind_model_transport(&actor.workspace_id, &reference, &transport_fingerprint)?;
    if admission {
        // Published definitions stay identical, but no HTTP or registered tool
        // implementation is retained in an admission-only runtime.
        executor = HttpTools::new(ToolRegistry::new())?;
    }
    Ok((Runtime::new(store, AgentLoop, model, executor), reference))
}

fn validate_tree(
    definition: &Definition,
    depth: usize,
    count: &mut usize,
    names: &mut std::collections::BTreeSet<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    *count += 1;
    if depth > 4 || *count > 32 {
        return Err("agent tree exceeds depth 4 or 32 total agents".into());
    }
    if !names.insert(definition.name.clone()) {
        return Err("agent names must be unique within the tree".into());
    }
    if depth > 0 && (definition.shared_model_budget.is_some() || definition.goal.is_some()) {
        return Err(
            "child configs inherit the root budget; put child acceptance criteria in output_schema"
                .into(),
        );
    }
    for child in &definition.subagents {
        validate_tree(child, depth + 1, count, names)?;
    }
    Ok(())
}

fn validate_provider_alias(name: &str, provider: &str) -> Result<(), Box<dyn std::error::Error>> {
    let limit = match provider {
        "" | "openai" => 64,
        "anthropic" => 128,
        // Other configured transports can have different naming extensions.
        _ => return Ok(()),
    };
    if name.is_empty()
        || name.len() > limit
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(format!(
            "model tool names require 1 to {limit} ASCII letters, digits, underscores or hyphens"
        )
        .into());
    }
    Ok(())
}

fn validate_contracts(definition: &Definition) -> Result<(), Box<dyn std::error::Error>> {
    if definition.name.trim().is_empty()
        || definition.instructions.trim().is_empty()
        || definition.version == 0
    {
        return Err("agent requires a name, instructions, positive version, and model".into());
    }
    if !matches!(
        definition.provider.as_str(),
        "" | "openai" | "anthropic" | "gemini" | "ollama"
    ) {
        return Err("unsupported provider".into());
    }
    selected_model(definition)?;
    if let Some(runtime) = &definition.runtime {
        runtime.validate()?;
        if !definition.provider.is_empty()
            || definition.endpoint != endpoint()
            || definition.api_key_env.is_some()
            || definition.reasoning.is_some()
            || definition.legacy_token_limit
        {
            return Err(
                "runtime bridges cannot be combined with provider transport or reasoning settings"
                    .into(),
            );
        }
    }
    definition.limits.validate()?;
    definition.coordination.validate()?;
    if let Some(memory) = &definition.memory {
        memory.validate()?;
    }
    if let Some(context) = &definition.context {
        context.validate()?;
        if context.reserve_bytes >= definition.limits.max_context_bytes {
            return Err("context reserve must be smaller than the context limit".into());
        }
    }
    if definition.max_output_tokens == 0 {
        return Err("max_output_tokens must be positive".into());
    }
    if let Some(reasoning) = &definition.reasoning {
        if definition.model.is_none() {
            return Err("reasoning requires an explicitly configured compatible model".into());
        }
        reasoning.validate(&definition.provider, definition.max_output_tokens)?;
    }
    if let Some(schema) = &definition.input_schema {
        crate::definitions::validate_schema(schema)?;
    }
    if let Some(schema) = &definition.output_schema {
        crate::definitions::validate_schema(schema)?;
    }
    if let Some(goal) = &definition.goal {
        if goal.objective.trim().is_empty() || goal.objective.len() > 16384 {
            return Err("invalid goal objective".into());
        }
        crate::definitions::validate_schema(&goal.success_schema)?;
        for criterion in &goal.criteria {
            criterion.validate()?;
        }
    }
    if let Some(budget) = &definition.shared_model_budget {
        if budget.group.trim().is_empty() || budget.group.len() > 256 || budget.limit == 0 {
            return Err("invalid shared budget".into());
        }
    }
    let alias_provider = if definition.runtime.is_some() {
        "bridge"
    } else {
        &definition.provider
    };
    let mut names = std::collections::BTreeSet::new();
    if definition.memory.is_some() {
        names.extend(["recall_memory", "retain_memory", "delete_memory"].map(str::to_owned));
    }
    if definition.context.is_some() {
        names.insert("read_context_artifact".to_owned());
    }
    if definition.allow_user_input {
        names.insert("ask_user".to_owned());
    }
    if !definition.skills.is_empty() || !definition.loaded_packages.is_empty() {
        names.insert("load_skill".to_owned());
    }
    for tool in &definition.http_tools {
        validate_provider_alias(&tool.name, alias_provider)?;
        if tool.name.trim().is_empty() || !names.insert(tool.name.clone()) {
            return Err("empty or duplicate tool name".into());
        }
        crate::definitions::validate_schema(&tool.input_schema)?;
        if let Some(schema) = &tool.output_schema {
            crate::definitions::validate_schema(schema)?;
        }
    }
    if definition.mcp_servers.len() > 32 {
        return Err("at most 32 MCP servers are allowed".into());
    }
    for server in &definition.mcp_servers {
        server.adapter_config().validate()?;
        for tool in &server.tools {
            if !names.insert(tool.name.clone()) {
                return Err("MCP tool name conflicts with another tool".into());
            }
        }
    }
    for child in &definition.subagents {
        for name in [
            format!("delegate_{}", child.name),
            format!("join_delegate_{}", child.name),
        ] {
            validate_provider_alias(&name, alias_provider)?;
            if !names.insert(name) {
                return Err("delegation tool name conflicts with another tool".into());
            }
        }
        validate_contracts(child)?;
    }
    Ok(())
}

/// Validated, immutable startup configuration shared by CLI and HTTP hosts.
#[derive(Clone)]
pub struct Configuration(Definition);
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenDefinition {
    definition: Definition,
    packages: Vec<SkillPackage>,
    children: Vec<FrozenDefinition>,
}
impl FrozenDefinition {
    fn freeze(mut definition: Definition) -> Self {
        let packages = std::mem::take(&mut definition.loaded_packages);
        let children = std::mem::take(&mut definition.subagents)
            .into_iter()
            .map(Self::freeze)
            .collect();
        Self {
            definition,
            packages,
            children,
        }
    }
    fn thaw(self) -> Result<Definition, Box<dyn std::error::Error>> {
        let mut definition = self.definition;
        if !definition.skill_files.is_empty()
            || !definition.skill_packages.is_empty()
            || !definition.subagents.is_empty()
        {
            return Err("published configuration cannot reference filesystem inputs".into());
        }
        if !definition.skills.is_empty() || !self.packages.is_empty() {
            SkillCatalog::with_packages(definition.skills.clone(), self.packages.clone())?;
        }
        definition.loaded_packages = self.packages;
        definition.subagents = self
            .children
            .into_iter()
            .map(Self::thaw)
            .collect::<Result<_, _>>()?;
        Ok(definition)
    }
}

impl Configuration {
    /// Declared common execution controls for the root and its specialists.
    /// Discovery never resolves credentials or launches an adapter.
    pub fn execution_catalog(&self) -> Vec<crate::adapters::runtime_bridge::ExecutionDescriptor> {
        fn visit(
            definition: &Definition,
            entries: &mut Vec<crate::adapters::runtime_bridge::ExecutionDescriptor>,
        ) {
            let entry = match &definition.runtime {
                Some(runtime) => runtime.descriptor(),
                None => crate::adapters::runtime_bridge::ExecutionDescriptor::new(
                    if definition.provider.is_empty() {
                        "openai".into()
                    } else {
                        definition.provider.clone()
                    },
                    1,
                    "provider_http",
                    false,
                ),
            };
            if !entries.contains(&entry) {
                entries.push(entry);
            }
            for child in &definition.subagents {
                visit(child, entries);
            }
        }
        let mut entries = Vec::new();
        visit(&self.0, &mut entries);
        entries
    }
    /// Customer publications get a separate shared budget for each idempotent
    /// root submission. Trusted startup configurations retain their explicit group.
    pub(crate) fn for_submission(&self, actor: &Actor, request_key: &str) -> crate::Result<Self> {
        let mut configuration = self.clone();
        if self
            .0
            .publication_sources
            .as_ref()
            .is_some_and(|sources| sources.get("customer_agent").is_some())
        {
            if request_key.is_empty() || request_key.len() > 256 {
                return Err(crate::Error::Invalid(
                    "customer submissions require a request key of 1 to 256 bytes".into(),
                ));
            }
            let budget = configuration
                .0
                .shared_model_budget
                .as_mut()
                .ok_or_else(|| {
                    crate::Error::Conflict("customer publication is missing its budget".into())
                })?;
            budget.group = format!(
                "run-{}",
                crate::definitions::digest(&(&budget.group, actor, request_key))?
            );
        }
        Ok(configuration)
    }
    pub(crate) fn from_effective_definition(value: serde_json::Value) -> crate::Result<Self> {
        let definition: Definition = serde_json::from_value(value)?;
        Self::restore_publication(serde_json::to_value(FrozenDefinition::freeze(definition))?)
            .map_err(|error| crate::Error::Invalid(error.to_string()))
    }
    pub(crate) fn publication_sources(&self) -> Option<&serde_json::Value> {
        self.0.publication_sources.as_ref()
    }
    pub(crate) fn freeze_publication(&self) -> crate::Result<serde_json::Value> {
        Ok(serde_json::to_value(FrozenDefinition::freeze(
            self.0.clone(),
        ))?)
    }
    pub(crate) fn restore_publication(
        value: serde_json::Value,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let definition = serde_json::from_value::<FrozenDefinition>(value)?.thaw()?;
        validate_tree(&definition, 0, &mut 0, &mut Default::default())?;
        if !definition.subagents.is_empty() && definition.shared_model_budget.is_none() {
            return Err("subagent trees require shared_model_budget on the root".into());
        }
        validate_contracts(&definition)?;
        Ok(Self(definition))
    }
    pub fn load(path: &std::path::Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut definition: Definition = serde_json::from_slice(&std::fs::read(path)?)?;
        validate_tree(&definition, 0, &mut 0, &mut Default::default())?;
        if !definition.subagents.is_empty() && definition.shared_model_budget.is_none() {
            return Err("subagent trees require shared_model_budget on the root".into());
        }
        load_skill_files(
            &mut definition,
            path.parent().unwrap_or(std::path::Path::new(".")),
        )?;
        validate_contracts(&definition)?;
        Ok(Self(definition))
    }

    pub fn build(
        self,
        store: Store,
        actor: &Actor,
    ) -> Result<(AgentRuntime, VersionRef, Option<Goal>), Box<dyn std::error::Error>> {
        let tree = self.build_tree(store, actor)?;
        Ok((tree.runtime, tree.reference, tree.goal))
    }
    pub fn build_tree(
        self,
        store: Store,
        actor: &Actor,
    ) -> Result<ConfiguredTree, Box<dyn std::error::Error>> {
        self.build_with_mode(store, actor, BuildMode::Local)
    }

    /// Build a tree whose child tools only submit/inspect runs. The caller must
    /// schedule children and wait for them before advancing the parent model.
    pub fn build_temporal_tree(
        self,
        store: Store,
        actor: &Actor,
    ) -> Result<ConfiguredTree, Box<dyn std::error::Error>> {
        self.build_with_mode(store, actor, BuildMode::Temporal)
    }

    /// Publish the same Temporal definitions without loading execution credentials.
    /// Returned runtimes cannot execute models or tools; a worker owns those effects.
    pub fn build_admission_tree(
        self,
        store: Store,
        actor: &Actor,
    ) -> Result<ConfiguredTree, Box<dyn std::error::Error>> {
        self.build_with_mode(store, actor, BuildMode::Admission)
    }

    fn build_with_mode(
        self,
        store: Store,
        actor: &Actor,
        mode: BuildMode,
    ) -> Result<ConfiguredTree, Box<dyn std::error::Error>> {
        let execution_catalog = self.execution_catalog();
        let mut definition = self.0;
        let goal = definition.goal.take();
        let budget = definition.shared_model_budget.take();
        let mut children = ChildRuntimes::new();
        let (runtime, reference) = build(
            definition,
            store,
            actor,
            budget.as_ref(),
            &mut children,
            mode,
        )?;
        Ok(ConfiguredTree {
            runtime,
            reference,
            goal,
            execution_catalog,
            children,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn definition(name: &str) -> Definition {
        serde_json::from_value(serde_json::json!({"name":name,"instructions":"help"})).unwrap()
    }

    #[test]
    fn provider_http_aliases_reject_invalid_names_before_model_io() {
        for name in [
            "lookup order".into(),
            "lookup.order".into(),
            "lookup/order".into(),
            "café".into(),
            "x".repeat(65),
        ] {
            let config: Definition = serde_json::from_value(serde_json::json!({
                "name":"root", "instructions":"help", "provider":"openai", "model":"fixture",
                "endpoint":"http://127.0.0.1:1/model",
                "http_tools":[{"name":name, "description":"lookup", "endpoint":"http://127.0.0.1:1/tool", "input_schema":{"type":"object"}, "effect":"read"}]
            })).unwrap();
            assert!(
                validate_contracts(&config).is_err(),
                "accepted unusable provider alias {name:?}"
            );
        }
    }

    #[test]
    fn provider_delegation_alias_limit_includes_join_prefix() {
        let mut config = definition("root");
        config.subagents = vec![definition(&"x".repeat(51))];
        // delegate_ + 51 characters fits OpenAI's 64-character limit; the longer
        // join_delegate_ alias does not. No model or tool endpoint is contacted.
        assert!(validate_contracts(&config).is_err());
    }

    #[test]
    fn provider_alias_boundaries_retain_valid_names_and_other_transports() {
        for (provider, limit) in [("", 64), ("openai", 64), ("anthropic", 128)] {
            let name = format!("A_{}-", "x".repeat(limit - 3));
            let mut config: Definition = serde_json::from_value(serde_json::json!({
                "name":"friendly root 🚀", "instructions":"help", "provider":provider, "model":"fixture",
                "http_tools":[{"name":name, "description":"lookup", "endpoint":"http://127.0.0.1:1/tool", "input_schema":{"type":"object"}, "effect":"read"}]
            })).unwrap();
            validate_contracts(&config).unwrap();
            config.http_tools[0].name.push('x');
            assert!(validate_contracts(&config).is_err());
            config.http_tools.clear();
            config.subagents = vec![definition(&"x".repeat(limit - "join_delegate_".len()))];
            validate_contracts(&config).unwrap();
            config.subagents[0].name.push('x');
            assert!(validate_contracts(&config).is_err());
        }
        for provider in ["gemini", "ollama"] {
            let config: Definition = serde_json::from_value(serde_json::json!({
                "name":"root", "instructions":"help", "provider":provider, "model":"fixture",
                "http_tools":[{"name":"custom.tool:name", "description":"lookup", "endpoint":"http://127.0.0.1:1/tool", "input_schema":{"type":"object"}, "effect":"read"}]
            })).unwrap();
            validate_contracts(&config).unwrap();
        }
    }
    #[test]
    fn reasoning_is_validated_and_pinned_without_changing_default_bindings() {
        use crate::adapters::reasoning::{Effort, Reasoning};
        let mut value = definition("reasoning-root");
        value.model = Some("fixture-model".into());
        value.endpoint = "http://127.0.0.1:1/model".into();
        let old_fingerprint = crate::definitions::digest(&(
            &value.provider,
            &value.endpoint,
            value.max_output_tokens,
            value.legacy_token_limit,
        ))
        .unwrap();
        let store = Store::default();
        let actor = Actor {
            workspace_id: "reasoning".into(),
            id: "operator".into(),
        };
        let tree = Configuration(value.clone())
            .build_admission_tree(store.clone(), &actor)
            .unwrap();
        let actual = store
            .read(|data| {
                Ok(
                    data.model_bindings[&(actor.workspace_id.clone(), tree.reference.clone())]
                        .clone(),
                )
            })
            .unwrap();
        assert_eq!(actual, old_fingerprint);
        value.reasoning = Some(Reasoning::Effort {
            effort: Effort::Low,
        });
        assert!(Configuration(value.clone())
            .build_admission_tree(store.clone(), &actor)
            .is_err());
        value.version += 1;
        Configuration(value.clone())
            .build_admission_tree(store, &actor)
            .unwrap();
        value.provider = "gemini".into();
        value.reasoning = Some(Reasoning::Budget { tokens: 1024 });
        assert!(validate_contracts(&value).is_err());
    }
    #[test]
    fn admission_preserves_worker_definitions_without_loading_secrets_or_executors() {
        use crate::adapters::tools::{Invocation, ToolExecutor};
        for provider in ["openai", "anthropic", "gemini"] {
            let key = format!("HUDSON_TEST_ADMISSION_{}", uuid::Uuid::new_v4().simple());
            assert!(std::env::var(&key).is_err());
            let config = || {
                Configuration(serde_json::from_value(serde_json::json!({
                "name":"admission-root", "instructions":"finish", "provider":provider, "model":"fixture-model",
                "endpoint":"http://127.0.0.1:1/model", "api_key_env":key,
                "shared_model_budget":{"group":"admission-budget", "limit":10},
                "http_tools":[{"name":"lookup", "description":"lookup", "endpoint":"http://127.0.0.1:1/tool",
                    "token_env":key, "input_schema":{"type":"object"}, "effect":"read"}],
                "subagents":[{"name":"child", "instructions":"finish", "provider":provider, "model":"fixture-model",
                    "endpoint":"http://127.0.0.1:1/model", "api_key_env":key}]
            })).unwrap())
            };
            let store = Store::default();
            let actor = Actor {
                workspace_id: "workspace".into(),
                id: "actor".into(),
            };
            let mut admission = config()
                .build_admission_tree(store.clone(), &actor)
                .unwrap();
            assert!(config().build_temporal_tree(store.clone(), &actor).is_err());
            // Only this unique environment name is modified; no shared provider key is used.
            std::env::set_var(&key, "test-only-secret");
            let worker = config().build_temporal_tree(store.clone(), &actor);
            std::env::remove_var(&key);
            let worker = worker.unwrap();
            assert_eq!(worker.reference, admission.reference);
            assert_eq!(worker.bindings(), admission.bindings());
            assert_eq!(
                worker.runtime.model_budget_binding(),
                admission.runtime.model_budget_binding()
            );
            let tools = store
                .read(|data| Ok(data.tools.values().cloned().collect::<Vec<_>>()))
                .unwrap();
            for tool in tools {
                let error = admission
                    .runtime
                    .tools
                    .execute(Invocation {
                        operation_id: uuid::Uuid::new_v4(),
                        tool: &tool,
                        arguments: &serde_json::json!({}),
                    })
                    .unwrap_err();
                assert!(matches!(
                    error,
                    crate::adapters::tools::ExecutionError::Failed(_)
                ));
            }
            let run = admission
                .runtime
                .submit(
                    &actor,
                    admission.reference.clone(),
                    serde_json::json!("finish"),
                    None,
                )
                .unwrap();
            let mut view = admission.tick(&actor, run).unwrap();
            for _ in 0..5 {
                if view.status.terminal() {
                    break;
                }
                view = admission.tick(&actor, run).unwrap();
            }
            assert_eq!(view.status, RunStatus::Failed);
            assert!(store
                .operations(&actor, run)
                .unwrap()
                .iter()
                .flat_map(|op| &op.attempts)
                .any(|attempt| attempt.error.as_deref()
                    == Some("admission host cannot execute model calls")));
        }
    }

    #[test]
    fn user_input_reserves_its_tool_name_only_when_enabled() {
        let mut config: Definition = serde_json::from_value(serde_json::json!({
            "name":"root","instructions":"help","allow_user_input":true,
            "http_tools":[{"name":"ask_user","description":"custom","endpoint":"http://127.0.0.1:9000/tool","input_schema":{"type":"object"},"effect":"read"}]
        })).unwrap();
        assert!(validate_contracts(&config).is_err());
        config.allow_user_input = false;
        validate_contracts(&config).unwrap();
    }
    #[test]
    fn rejects_duplicate_names_and_child_budget_overrides() {
        let mut root = definition("root");
        root.subagents.push(definition("root"));
        assert!(validate_tree(&root, 0, &mut 0, &mut Default::default()).is_err());
        root.subagents = vec![definition("child")];
        root.subagents[0].shared_model_budget = Some(SharedBudget {
            group: "escape".into(),
            limit: 100,
        });
        assert!(validate_tree(&root, 0, &mut 0, &mut Default::default()).is_err());
    }
    #[test]
    fn rejects_invalid_contracts_before_startup() {
        let mut config = definition("root");
        config.output_schema = Some(serde_json::json!({"type":"not-a-json-type"}));
        assert!(validate_contracts(&config).is_err());
        config.output_schema = None;
        config.input_schema = Some(serde_json::json!({"type":"invalid-type"}));
        assert!(validate_contracts(&config).is_err());
        config.input_schema = None;
        config.shared_model_budget = Some(SharedBudget {
            group: "test".into(),
            limit: 0,
        });
        assert!(validate_contracts(&config).is_err());
        config.shared_model_budget = None;
        config.provider = "unsupported".into();
        assert!(validate_contracts(&config).is_err());
    }

    #[test]
    fn rejects_excessive_delegation_depth() {
        let mut tree = definition("leaf");
        for index in 0..5 {
            let mut parent = definition(&format!("node-{index}"));
            parent.subagents.push(tree);
            tree = parent;
        }
        assert!(validate_tree(&tree, 0, &mut 0, &mut Default::default()).is_err());
    }
    #[test]
    fn configuration_and_publication_share_limit_validation() {
        for field in [
            "max_harness_steps",
            "max_model_calls",
            "max_operations",
            "max_batch_size",
            "max_context_bytes",
            "max_payload_bytes",
        ] {
            let mut limits = serde_json::to_value(Limits::default()).unwrap();
            limits[field] = serde_json::json!(0);
            let mut root = definition("root");
            root.limits = serde_json::from_value(limits).unwrap();
            assert!(validate_contracts(&root).is_err(), "accepted zero {field}");
            let mut parent = definition("parent");
            parent.subagents.push(root);
            assert!(
                validate_contracts(&parent).is_err(),
                "accepted invalid child {field}"
            );
        }
    }
    #[test]
    fn default_gpt_does_not_reserve_a_model_name_on_other_protocols() {
        let mut config = definition("root");
        assert_eq!(selected_model(&config).unwrap(), "gpt-4.1-mini");
        for provider in ["openai", "anthropic", "gemini", "ollama"] {
            config.provider = provider.into();
            config.model = Some("gpt-4.1-mini".into());
            assert_eq!(selected_model(&config).unwrap(), "gpt-4.1-mini");
            validate_contracts(&config).unwrap();
            config.model = Some("custom/model-alias:version".into());
            assert_eq!(
                selected_model(&config).unwrap(),
                "custom/model-alias:version"
            );
            config.model = Some(" ".into());
            assert!(validate_contracts(&config).is_err());
            config.model = None;
            assert_eq!(selected_model(&config).is_ok(), provider == "openai");
        }
    }
    #[test]
    fn mcp_aliases_cannot_shadow_any_enabled_capability() {
        for alias in [
            "ask_user",
            "load_skill",
            "recall_memory",
            "retain_memory",
            "delete_memory",
            "read_context_artifact",
            "delegate_child",
            "join_delegate_child",
            "http_custom",
        ] {
            let config: Definition = serde_json::from_value(serde_json::json!({
                "name":"root","instructions":"help","allow_user_input":true,"context":{},
                "memory":{"scope":{"name":"company"},"recall_limit":2},
                "skills":[{"name":"analysis","description":"Analyze","instructions":"Help"}],
                "http_tools":[{"name":"http_custom","description":"custom","endpoint":"http://127.0.0.1:9/tool","input_schema":{"type":"object"},"effect":"read"}],
                "subagents":[{"name":"child","instructions":"help"}],
                "mcp_servers":[{"endpoint":"http://127.0.0.1:9/mcp","tools":[{"name":alias,"remote_name":"remote","description":"tool","input_schema":{"type":"object"},"effect":"read"}]}]
            })).unwrap();
            assert!(
                validate_contracts(&config).is_err(),
                "allowed alias {alias}"
            );
        }
        let mut config: Definition = serde_json::from_value(serde_json::json!({"name":"root","instructions":"help","mcp_servers":[{"endpoint":"http://127.0.0.1:9/mcp","tools":[{"name":"ask_user","remote_name":"remote","description":"tool","input_schema":{"type":"object"},"effect":"read"}]}]})).unwrap();
        validate_contracts(&config).unwrap(); // Disabled built-in remains available as a custom alias.
        config.mcp_servers.push(serde_json::from_value(serde_json::json!({"endpoint":"http://127.0.0.1:10/mcp","tools":[{"name":"ask_user","remote_name":"remote","description":"tool","input_schema":{"type":"object"},"effect":"read"}]})).unwrap());
        assert!(validate_contracts(&config).is_err());
    }

    #[test]
    fn rebuilding_shared_memory_context_and_mcp_team_is_stable() {
        let source = serde_json::json!({
            "name":"root","instructions":"help","provider":"ollama","model":"fixture",
            "context":{},"memory":{"scope":{"name":"company"},"recall_limit":2},
            "shared_model_budget":{"group":"team","limit":20},
            "mcp_servers":[{"endpoint":"http://127.0.0.1:9/mcp","tools":[{"name":"custom","remote_name":"remote","description":"tool","input_schema":{"type":"object"},"effect":"read"}]}],
            "subagents":[{"name":"child","instructions":"help","provider":"ollama","model":"fixture","context":{},"memory":{"scope":{"name":"company"},"recall_limit":2},"mcp_servers":[{"endpoint":"http://127.0.0.1:9/mcp","tools":[{"name":"custom","remote_name":"remote","description":"tool","input_schema":{"type":"object"},"effect":"read"}]}]}]
        });
        let store = Store::default();
        let actor = Actor {
            workspace_id: "company".into(),
            id: "owner".into(),
        };
        for _ in 0..2 {
            let definition: Definition = serde_json::from_value(source.clone()).unwrap();
            validate_contracts(&definition).unwrap();
            let tree = Configuration(definition)
                .build_temporal_tree(store.clone(), &actor)
                .unwrap();
            assert_eq!(tree.bindings().len(), 2);
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
    }
    #[test]
    fn existing_single_agent_runs_rebuild_without_coordination_binding() {
        let source = serde_json::json!({"name":"single","instructions":"help","provider":"ollama","model":"fixture"});
        let store = Store::default();
        let actor = Actor {
            workspace_id: "company".into(),
            id: "owner".into(),
        };
        let tree = Configuration(serde_json::from_value(source.clone()).unwrap())
            .build_temporal_tree(store.clone(), &actor)
            .unwrap();
        let id = tree
            .runtime
            .submit(
                &actor,
                tree.reference.clone(),
                serde_json::json!("task"),
                None,
            )
            .unwrap();
        // Simulate storage written before optional capability bindings existed.
        store
            .transact(|data| {
                data.coordination_policies.clear();
                data.memory_bindings.clear();
                data.context_policies.clear();
                Ok(())
            })
            .unwrap();
        let rebuilt = Configuration(serde_json::from_value(source).unwrap())
            .build_temporal_tree(store.clone(), &actor)
            .unwrap();
        assert_eq!(rebuilt.reference, tree.reference);
        store
            .validate_resume(&actor, id, &rebuilt.reference, &None)
            .unwrap();
        assert!(store
            .read(|data| Ok(data.coordination_policies.is_empty()))
            .unwrap());
    }

    #[test]
    fn customer_mcp_example_loads_offline() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/customer-mcp.json");
        let config = Configuration::load(&path).unwrap();
        assert_eq!(config.0.loaded_packages.len(), 1);
        assert_eq!(config.0.mcp_servers.len(), 1);
    }
}
