//! Restricted customer definitions resolved against installation-owned capabilities.
//! HTTP callers never supply credential names, endpoints, filesystem paths or actors.
use crate::{
    configured::Configuration,
    definitions::digest,
    models::{now, Actor, Effect, Limits, VersionRef},
    publication::PublicationReceipt,
    storage::Store,
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub id: String,
    pub version: u32,
}
impl Reference {
    fn validate(&self) -> Result<()> {
        name(&self.id)?;
        if self.version == 0 {
            return Err(Error::Invalid("version must be positive".into()));
        }
        Ok(())
    }
    pub fn version_ref(&self) -> VersionRef {
        VersionRef {
            id: self.id.clone(),
            version: self.version,
        }
    }
}
fn name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(Error::Invalid(
            "names require 1 to 64 ASCII letters, digits, hyphens or underscores".into(),
        ));
    }
    Ok(())
}
fn yes() -> bool {
    true
}
fn write() -> Effect {
    Effect::Write
}
fn total_budget() -> u32 {
    32
}
fn output_tokens() -> u32 {
    1024
}

/// Trusted installation file. Its owner is checked against the authenticated host.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCatalog {
    pub owner: Actor,
    pub models: Vec<ModelProfile>,
    #[serde(default)]
    pub connections: Vec<ToolConnection>,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default = "total_budget")]
    pub max_total_model_calls: u32,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    pub reference: Reference,
    #[serde(default)]
    pub provider: String,
    pub model: String,
    #[serde(default = "profile_endpoint")]
    pub endpoint: String,
    pub api_key_env: Option<String>,
    #[serde(default = "output_tokens")]
    pub max_output_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<crate::adapters::reasoning::Reasoning>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<crate::adapters::runtime_bridge::BridgeConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend: Option<crate::spending::SpendPolicy>,
}
fn profile_endpoint() -> String {
    "https://api.openai.com/v1/chat/completions".into()
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolConnection {
    pub reference: Reference,
    pub transport: ConnectionTransport,
    #[serde(default = "write")]
    pub effect: Effect,
    #[serde(default = "yes")]
    pub require_approval: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConnectionTransport {
    Http {
        endpoint: String,
        token_env: Option<String>,
    },
    Mcp {
        endpoint: String,
        bearer_env: Option<String>,
        remote_name: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    pub name: String,
    pub version: u32,
    pub description: String,
    pub connection: Reference,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub effect: Option<Effect>,
    pub require_approval: Option<bool>,
}
impl ToolDefinition {
    pub fn reference(&self) -> Reference {
        Reference {
            id: self.name.clone(),
            version: self.version,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDefinition {
    pub name: String,
    pub version: u32,
    pub instructions: String,
    pub model_profile: Reference,
    #[serde(default)]
    pub tools: Vec<Reference>,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub skills: Vec<InlineSkill>,
    pub limits: Option<Limits>,
    pub model_budget: Option<u32>,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub subagents: Vec<AgentDefinition>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Capabilities {
    pub context: bool,
    pub memory: bool,
    pub user_input: bool,
}
impl Default for Capabilities {
    fn default() -> Self {
        Self {
            context: true,
            memory: false,
            user_input: true,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InlineSkill {
    pub name: String,
    pub description: String,
    pub instructions: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DefinitionReceipt {
    pub reference: Reference,
    pub digest: String,
    pub created_at: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RegistryEntry {
    pub receipt: DefinitionReceipt,
    pub value: Value,
}

impl HostCatalog {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).map_err(|_| Error::Invalid("cannot read host catalog".into()))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
    fn authorize(&self, actor: &Actor) -> Result<()> {
        if actor != &self.owner
            || actor.id.trim().is_empty()
            || actor.workspace_id.trim().is_empty()
        {
            return Err(Error::Denied);
        }
        Ok(())
    }
    /// Public descriptions expose references and policy constraints, never endpoints or credentials.
    pub fn capabilities(&self, actor: &Actor) -> Result<Value> {
        self.authorize(actor)?;
        Ok(
            json!({"models":self.models.iter().map(|m| json!({"reference":m.reference,"provider":m.provider,"model":m.model})).collect::<Vec<_>>(),
            "connections":self.connections.iter().map(|c| json!({"reference":c.reference,"effect":c.effect,"require_approval":c.require_approval})).collect::<Vec<_>>(),
            "limits":self.limits,"max_total_model_calls":self.max_total_model_calls}),
        )
    }
    /// Install immutable host capability versions in one transaction. Validation is offline.
    pub fn install(&self, store: &Store, actor: &Actor) -> Result<()> {
        self.authorize(actor)?;
        self.limits.validate()?;
        if self.models.is_empty()
            || self.models.len() > 32
            || self.connections.len() > 256
            || self.max_total_model_calls == 0
        {
            return Err(Error::Invalid("invalid host capability limits".into()));
        }
        let mut entries = Vec::new();
        for model in &self.models {
            model.reference.validate()?;
            let definition = model.apply(
                json!({"name":"catalog-validation","instructions":"Validate configuration"}),
            );
            Configuration::from_effective_definition(definition)?
                .build_admission_tree(Store::default(), actor)
                .map_err(invalid)?;
            entries.push((
                "model",
                model.reference.clone(),
                serde_json::to_value(model)?,
            ));
        }
        for connection in &self.connections {
            connection.reference.validate()?;
            let tool = ToolDefinition {
                name: "catalog_tool".into(),
                version: 1,
                description: "Validate connection".into(),
                connection: connection.reference.clone(),
                input_schema: json!({"type":"object"}),
                output_schema: None,
                effect: None,
                require_approval: None,
            };
            let effective = connection.compile(&tool)?;
            let mut definition = self.models[0].apply(
                json!({"name":"connection-validation","instructions":"Validate connection"}),
            );
            attach_tool(&mut definition, effective);
            Configuration::from_effective_definition(definition)?
                .build_admission_tree(Store::default(), actor)
                .map_err(invalid)?;
            entries.push((
                "connection",
                connection.reference.clone(),
                serde_json::to_value(connection)?,
            ));
        }
        store.transact(|data| {
            let mut seen = std::collections::BTreeSet::new();
            for (kind, reference, value) in entries {
                let key = (
                    actor.workspace_id.clone(),
                    actor.id.clone(),
                    kind.to_owned(),
                    reference.version_ref(),
                );
                if !seen.insert(key.clone()) {
                    return Err(Error::Invalid("duplicate host capability reference".into()));
                }
                let expected = digest(&value)?;
                if let Some(existing) = data.customer_registry.get(&key) {
                    if existing.receipt.digest != expected {
                        return Err(Error::Conflict(
                            "host capability version is immutable".into(),
                        ));
                    }
                } else {
                    data.customer_registry.insert(
                        key,
                        RegistryEntry {
                            receipt: DefinitionReceipt {
                                reference,
                                digest: expected,
                                created_at: now(),
                            },
                            value,
                        },
                    );
                }
            }
            Ok(())
        })
    }
    fn compile_agent(
        &self,
        store: &Store,
        actor: &Actor,
        agent: &AgentDefinition,
        root: &AgentDefinition,
        depth: usize,
        count: &mut usize,
    ) -> Result<Value> {
        *count += 1;
        if depth > 8 || *count > 32 || agent.tools.len() > 64 || agent.skills.len() > 32 {
            return Err(Error::Invalid(
                "agent tree exceeds publication limits".into(),
            ));
        }
        Reference {
            id: agent.name.clone(),
            version: agent.version,
        }
        .validate()?;
        agent.model_profile.validate()?;
        if agent.instructions.trim().is_empty() || agent.instructions.len() > 32768 {
            return Err(Error::Invalid(
                "instructions require 1 to 32768 bytes".into(),
            ));
        }
        if depth > 0 && agent.model_budget.is_some() {
            return Err(Error::Invalid(
                "shared model budget belongs to the root".into(),
            ));
        }
        let model = self
            .models
            .iter()
            .find(|m| m.reference == agent.model_profile)
            .ok_or(Error::NotFound)?;
        let limits = agent.limits.as_ref().unwrap_or(&self.limits);
        limits.validate()?;
        let requested = serde_json::to_value(limits)?;
        let ceiling = serde_json::to_value(&self.limits)?;
        if requested
            .as_object()
            .unwrap()
            .iter()
            .any(|(k, v)| v.as_u64().unwrap() > ceiling[k].as_u64().unwrap())
        {
            return Err(Error::Denied);
        }
        let mut definition = model.apply(json!({"name":agent.name,"version":agent.version,"instructions":agent.instructions,
            "limits":limits,"allow_user_input":agent.capabilities.user_input,"input_schema":agent.input_schema,"output_schema":agent.output_schema,"skills":agent.skills}));
        if agent.capabilities.context {
            definition["context"] = json!({});
        }
        if agent.capabilities.memory {
            definition["memory"] = json!({"scope":{"name":format!("customer-{}", digest(&(actor,&root.name))?)},"recall_limit":5,"retain_pointer":null});
        }
        for reference in &agent.tools {
            reference.validate()?;
            let record = store.registry_value(actor, "tool", reference)?;
            // Only tools on this installation's currently approved connections can
            // be selected for a new agent. Existing revisions retain their snapshot.
            let tool: ToolDefinition = serde_json::from_value(record["definition"].clone())?;
            let connection = self
                .connections
                .iter()
                .find(|c| c.reference == tool.connection)
                .ok_or(Error::NotFound)?;
            if store.registry_value(actor, "connection", &tool.connection)?
                != serde_json::to_value(connection)?
            {
                return Err(Error::Conflict("host connection is not installed".into()));
            }
            let effective = connection.compile(&tool)?;
            if record["effective"] != effective {
                return Err(Error::Conflict("connection binding changed".into()));
            }
            attach_tool(&mut definition, effective);
        }
        let children = agent
            .subagents
            .iter()
            .map(|child| self.compile_agent(store, actor, child, root, depth + 1, count))
            .collect::<Result<Vec<_>>>()?;
        definition["subagents"] = json!(children);
        if depth == 0 {
            let budget = agent.model_budget.unwrap_or(self.max_total_model_calls);
            if budget == 0 || budget > self.max_total_model_calls {
                return Err(Error::Denied);
            }
            definition["shared_model_budget"] = json!({"group":format!("publication-{}",digest(&(actor,&root.name,root.version))?),"limit":budget});
            definition["publication_sources"] = json!({"customer_agent":agent});
        }
        Ok(definition)
    }
}
impl ModelProfile {
    pub fn execution_descriptor(&self) -> crate::adapters::runtime_bridge::ExecutionDescriptor {
        match &self.runtime {
            Some(runtime) => runtime.descriptor(),
            None => crate::adapters::runtime_bridge::ExecutionDescriptor::new(
                if self.provider.is_empty() {
                    "openai".into()
                } else {
                    self.provider.clone()
                },
                1,
                "provider_http",
                false,
            ),
        }
    }
    fn apply(&self, mut value: Value) -> Value {
        value["provider"] = json!(self.provider);
        value["model"] = json!(self.model);
        value["endpoint"] = json!(self.endpoint);
        value["api_key_env"] = json!(self.api_key_env);
        value["max_output_tokens"] = json!(self.max_output_tokens);
        if let Some(runtime) = &self.runtime {
            value["runtime"] = json!(runtime);
        }
        if let Some(reasoning) = &self.reasoning {
            value["reasoning"] = json!(reasoning);
        }
        if let Some(spend) = &self.spend {
            value["spend"] = json!(spend);
        }
        value
    }
}
impl ToolConnection {
    fn compile(&self, tool: &ToolDefinition) -> Result<Value> {
        tool.reference().validate()?;
        if tool.description.trim().is_empty() || tool.description.len() > 4096 {
            return Err(Error::Invalid(
                "tool description requires 1 to 4096 bytes".into(),
            ));
        }
        crate::definitions::validate_schema(&tool.input_schema)?;
        if let Some(schema) = &tool.output_schema {
            crate::definitions::validate_schema(schema)?;
        }
        let effect = tool.effect.as_ref().unwrap_or(&self.effect);
        let approval = tool.require_approval.unwrap_or(self.require_approval);
        if (self.effect == Effect::Write && *effect == Effect::Read)
            || (self.require_approval && !approval)
        {
            return Err(Error::Denied);
        }
        let mut effective = json!({"name":tool.name,"description":tool.description,"input_schema":tool.input_schema,"effect":effect,"require_approval":approval});
        match &self.transport {
            ConnectionTransport::Http {
                endpoint,
                token_env,
            } => {
                effective["endpoint"] = json!(endpoint);
                effective["token_env"] = json!(token_env);
                effective["output_schema"] = json!(tool.output_schema);
                Ok(json!({"http":effective}))
            }
            ConnectionTransport::Mcp {
                endpoint,
                bearer_env,
                remote_name,
            } => {
                if tool.output_schema.is_some() {
                    return Err(Error::Unsupported(
                        "MCP output schemas are not supported by this adapter".into(),
                    ));
                }
                effective["remote_name"] = json!(remote_name);
                Ok(json!({"mcp":{"endpoint":endpoint,"bearer_env":bearer_env,"tools":[effective]}}))
            }
        }
    }
}
fn attach_tool(definition: &mut Value, effective: Value) {
    let (key, value) = if effective.get("http").is_some() {
        ("http_tools", effective["http"].clone())
    } else {
        ("mcp_servers", effective["mcp"].clone())
    };
    if definition.get(key).is_none() {
        definition[key] = json!([]);
    }
    definition[key].as_array_mut().unwrap().push(value);
}
fn invalid(error: Box<dyn std::error::Error>) -> Error {
    Error::Invalid(error.to_string())
}

impl Store {
    fn registry_value(&self, actor: &Actor, kind: &str, reference: &Reference) -> Result<Value> {
        self.read(|data| {
            data.customer_registry
                .get(&(
                    actor.workspace_id.clone(),
                    actor.id.clone(),
                    kind.to_owned(),
                    reference.version_ref(),
                ))
                .map(|entry| entry.value.clone())
                .ok_or(Error::NotFound)
        })
    }
    fn replay_customer_tool(
        &self,
        actor: &Actor,
        tool: &ToolDefinition,
        request_key: &str,
    ) -> Result<Option<DefinitionReceipt>> {
        let requested = serde_json::to_value(tool)?;
        self.transact(|data| {
            let retry = (
                actor.workspace_id.clone(),
                actor.id.clone(),
                request_key.to_owned(),
            );
            let reference = data
                .customer_tool_keys
                .get(&retry)
                .map(|receipt| receipt.reference.clone())
                .unwrap_or_else(|| tool.reference());
            let key = (
                actor.workspace_id.clone(),
                actor.id.clone(),
                "tool".into(),
                reference.version_ref(),
            );
            let Some(record) = data.customer_registry.get(&key) else {
                return Ok(None);
            };
            if record.value["definition"] != requested {
                return Err(Error::Conflict(
                    "tool revision or retry key is immutable".into(),
                ));
            }
            let receipt = record.receipt.clone();
            data.customer_tool_keys.insert(retry, receipt.clone());
            Ok(Some(receipt))
        })
    }
    fn replay_customer_agent(
        &self,
        actor: &Actor,
        agent: &AgentDefinition,
        request_key: &str,
    ) -> Result<Option<PublicationReceipt>> {
        let requested = serde_json::to_value(agent)?;
        self.transact(|data| {
            let retry = (
                actor.workspace_id.clone(),
                actor.id.clone(),
                request_key.to_owned(),
            );
            let reference = data
                .publication_keys
                .get(&retry)
                .map(|receipt| receipt.agent_ref.clone())
                .unwrap_or_else(|| VersionRef {
                    id: agent.name.clone(),
                    version: agent.version,
                });
            let Some(record) = data
                .publications
                .get(&(actor.workspace_id.clone(), reference))
            else {
                return Ok(None);
            };
            if record.actor_id != actor.id
                || record.frozen["definition"]["publication_sources"]["customer_agent"] != requested
            {
                return Err(Error::Conflict(
                    "agent revision or retry key is immutable".into(),
                ));
            }
            let receipt = record.receipt.clone();
            data.publication_keys.insert(retry, receipt.clone());
            Ok(Some(receipt))
        })
    }
    pub fn publish_customer_tool(
        &self,
        actor: &Actor,
        catalog: &HostCatalog,
        tool: ToolDefinition,
        request_key: &str,
    ) -> Result<DefinitionReceipt> {
        catalog.authorize(actor)?;
        if request_key.is_empty() || request_key.len() > 256 {
            return Err(Error::Invalid("publication request key length".into()));
        }
        tool.reference().validate()?;
        if let Some(receipt) = self.replay_customer_tool(actor, &tool, request_key)? {
            return Ok(receipt);
        }
        let connection = catalog
            .connections
            .iter()
            .find(|c| c.reference == tool.connection)
            .ok_or(Error::NotFound)?;
        if self.registry_value(actor, "connection", &tool.connection)?
            != serde_json::to_value(connection)?
        {
            return Err(Error::Conflict("host connection is not installed".into()));
        }
        let effective = connection.compile(&tool)?;
        let value = json!({"definition":tool,"effective":effective});
        let expected = digest(&value)?;
        let reference = tool.reference();
        self.transact(|data| {
            let retry = (
                actor.workspace_id.clone(),
                actor.id.clone(),
                request_key.to_owned(),
            );
            if let Some(receipt) = data.customer_tool_keys.get(&retry) {
                return if receipt.digest == expected {
                    Ok(receipt.clone())
                } else {
                    Err(Error::Conflict(
                        "tool publication key reused with changed content".into(),
                    ))
                };
            }
            let key = (
                actor.workspace_id.clone(),
                actor.id.clone(),
                "tool".into(),
                reference.version_ref(),
            );
            let receipt = if let Some(entry) = data.customer_registry.get(&key) {
                if entry.receipt.digest != expected {
                    return Err(Error::Conflict("tool revision is immutable".into()));
                }
                entry.receipt.clone()
            } else {
                let receipt = DefinitionReceipt {
                    reference,
                    digest: expected,
                    created_at: now(),
                };
                data.customer_registry.insert(
                    key,
                    RegistryEntry {
                        receipt: receipt.clone(),
                        value,
                    },
                );
                receipt
            };
            data.customer_tool_keys.insert(retry, receipt.clone());
            Ok(receipt)
        })
    }
    pub fn customer_tool(&self, actor: &Actor, reference: &Reference) -> Result<ToolDefinition> {
        Ok(serde_json::from_value(
            self.registry_value(actor, "tool", reference)?["definition"].clone(),
        )?)
    }
    pub fn publish_customer_agent(
        &self,
        actor: &Actor,
        catalog: &HostCatalog,
        agent: AgentDefinition,
        request_key: &str,
    ) -> Result<PublicationReceipt> {
        catalog.authorize(actor)?;
        if request_key.is_empty() || request_key.len() > 256 {
            return Err(Error::Invalid("publication request key length".into()));
        }
        Reference {
            id: agent.name.clone(),
            version: agent.version,
        }
        .validate()?;
        if let Some(receipt) = self.replay_customer_agent(actor, &agent, request_key)? {
            return Ok(receipt);
        }
        // Verify the exact installed catalog versions before selecting capabilities.
        for model in &catalog.models {
            if self.registry_value(actor, "model", &model.reference)?
                != serde_json::to_value(model)?
            {
                return Err(Error::Conflict("host model is not installed".into()));
            }
        }
        let definition = catalog.compile_agent(self, actor, &agent, &agent, 0, &mut 0)?;
        match self.publish_configuration(
            actor,
            &Configuration::from_effective_definition(definition)?,
            request_key,
        ) {
            Ok(receipt) => Ok(receipt),
            Err(error) => self
                .replay_customer_agent(actor, &agent, request_key)?
                .ok_or(error),
        }
    }
    pub fn customer_agent(&self, actor: &Actor, reference: &Reference) -> Result<AgentDefinition> {
        let config = self.published_configuration(actor, &reference.version_ref())?;
        let sources = config.publication_sources().ok_or(Error::NotFound)?;
        Ok(serde_json::from_value(
            sources
                .get("customer_agent")
                .cloned()
                .ok_or(Error::NotFound)?,
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn operator_profiles_carry_reasoning_without_changing_omitted_defaults() {
        let mut profile = catalog().models.remove(0);
        assert!(serde_json::to_value(&profile)
            .unwrap()
            .get("reasoning")
            .is_none());
        let plain = profile.apply(json!({"name":"profile-agent","instructions":"finish"}));
        assert!(plain.get("reasoning").is_none());
        profile.reasoning = Some(crate::adapters::reasoning::Reasoning::Effort {
            effort: crate::adapters::reasoning::Effort::Low,
        });
        let configured = profile.apply(json!({"name":"profile-agent","instructions":"finish"}));
        assert_eq!(
            configured["reasoning"],
            json!({"type":"effort","effort":"low"})
        );
        Configuration::from_effective_definition(configured).unwrap();
    }
    fn catalog() -> HostCatalog {
        serde_json::from_value(json!({"owner":{"workspace_id":"company","id":"owner"},
            "models":[{"reference":{"id":"standard","version":1},"provider":"openai","model":"fixture","endpoint":"http://127.0.0.1:9/model","api_key_env":"HUDSON_CUSTOMER_UNAVAILABLE_KEY"}],
            "connections":[{"reference":{"id":"crm","version":1},"transport":{"type":"http","endpoint":"http://127.0.0.1:9/tool","token_env":"HUDSON_CUSTOMER_UNAVAILABLE_KEY"}}],
            "max_total_model_calls":2})).unwrap()
    }
    fn tool() -> ToolDefinition {
        serde_json::from_value(json!({"name":"lookup","version":1,"description":"Lookup a customer","connection":{"id":"crm","version":1},"input_schema":{"type":"object"}})).unwrap()
    }
    fn agent() -> AgentDefinition {
        serde_json::from_value(json!({"name":"assistant","version":1,"instructions":"help","model_profile":{"id":"standard","version":1},"tools":[{"id":"lookup","version":1}],"input_schema":{"type":"object"}})).unwrap()
    }
    fn snapshot(store: &Store) -> Value {
        store.read(|data| Ok(serde_json::to_value(data)?)).unwrap()
    }

    #[test]
    fn public_definitions_reject_host_fields_at_every_boundary() {
        for field in [
            "workspace_id",
            "actor_id",
            "endpoint",
            "api_key_env",
            "skill_files",
            "shared_model_budget",
            "publication_sources",
        ] {
            let mut value = serde_json::to_value(agent()).unwrap();
            value[field] = json!("injected");
            assert!(
                serde_json::from_value::<AgentDefinition>(value).is_err(),
                "accepted {field}"
            );
        }
        let mut value = serde_json::to_value(tool()).unwrap();
        value["connection"]["workspace_id"] = json!("other");
        assert!(serde_json::from_value::<ToolDefinition>(value).is_err());
        let mut value = serde_json::to_value(agent()).unwrap();
        value["subagents"] = json!([{"name":"child","version":1,"instructions":"help","model_profile":{"id":"standard","version":1},"endpoint":"http://attacker"}]);
        assert!(serde_json::from_value::<AgentDefinition>(value).is_err());
    }

    #[test]
    fn capability_versions_are_atomic_immutable_and_owner_bound() {
        let store = Store::default();
        let catalog = catalog();
        let actor = catalog.owner.clone();
        catalog.install(&store, &actor).unwrap();
        let before = snapshot(&store);
        let mut changed = catalog.clone();
        changed.models[0].model = "changed".into();
        assert!(changed.install(&store, &actor).is_err());
        let mut other = actor.clone();
        other.workspace_id = "other".into();
        assert!(catalog.install(&store, &other).is_err());
        assert!(store
            .publish_customer_tool(&other, &catalog, tool(), "one")
            .is_err());
        assert_eq!(before, snapshot(&store));
        let capabilities = catalog.capabilities(&actor).unwrap().to_string();
        assert!(!capabilities.contains("HUDSON_CUSTOMER_UNAVAILABLE_KEY"));
        assert!(!capabilities.contains("127.0.0.1"));
    }

    #[test]
    fn tool_policy_floors_and_agent_ceiling_cannot_be_weakened() {
        let store = Store::default();
        let catalog = catalog();
        let actor = catalog.owner.clone();
        catalog.install(&store, &actor).unwrap();
        let before = snapshot(&store);
        let mut denied = tool();
        denied.effect = Some(Effect::Read);
        assert!(matches!(
            store.publish_customer_tool(&actor, &catalog, denied, "read"),
            Err(Error::Denied)
        ));
        let mut denied = tool();
        denied.require_approval = Some(false);
        assert!(matches!(
            store.publish_customer_tool(&actor, &catalog, denied, "approval"),
            Err(Error::Denied)
        ));
        assert_eq!(before, snapshot(&store));
        let receipt = store
            .publish_customer_tool(&actor, &catalog, tool(), "tool")
            .unwrap();
        assert_eq!(
            receipt,
            store
                .publish_customer_tool(&actor, &catalog, tool(), "tool")
                .unwrap()
        );
        let mut changed = tool();
        changed.description = "changed".into();
        assert!(store
            .publish_customer_tool(&actor, &catalog, changed, "different-key")
            .is_err());
        let before = snapshot(&store);
        let mut denied = agent();
        denied.limits = Some(Limits {
            max_model_calls: catalog.limits.max_model_calls + 1,
            ..catalog.limits.clone()
        });
        assert!(matches!(
            store.publish_customer_agent(&actor, &catalog, denied, "agent"),
            Err(Error::Denied)
        ));
        let mut denied = agent();
        denied.subagents = vec![AgentDefinition {
            instructions: "".into(),
            tools: vec![],
            ..agent()
        }];
        assert!(store
            .publish_customer_agent(&actor, &catalog, denied, "invalid-child")
            .is_err());
        assert_eq!(before, snapshot(&store));
        let receipt = store
            .publish_customer_agent(&actor, &catalog, agent(), "agent")
            .unwrap();
        assert_eq!(
            receipt,
            store
                .publish_customer_agent(&actor, &catalog, agent(), "agent")
                .unwrap()
        );
        let restored = store
            .published_configuration(&actor, &receipt.agent_ref)
            .unwrap();
        let frozen = restored.freeze_publication().unwrap();
        assert_eq!(
            frozen["definition"]["http_tools"][0]["require_approval"],
            true
        );
        assert_eq!(frozen["definition"]["http_tools"][0]["effect"], "write");
    }

    #[test]
    fn root_submissions_have_independent_budgets_and_invalid_input_is_atomic() {
        let store = Store::default();
        let catalog = catalog();
        let actor = catalog.owner.clone();
        catalog.install(&store, &actor).unwrap();
        store
            .publish_customer_tool(&actor, &catalog, tool(), "tool")
            .unwrap();
        let receipt = store
            .publish_customer_agent(&actor, &catalog, agent(), "agent")
            .unwrap();
        let target = crate::scheduling::ScheduleTarget {
            scheduler: "temporal:test".into(),
            task_queue: "queue".into(),
        };
        let before = snapshot(&store);
        assert!(store
            .submit_published(
                &actor,
                &receipt.agent_ref,
                json!("not an object"),
                "invalid",
                target.clone()
            )
            .is_err());
        assert_eq!(before, snapshot(&store));
        let first = store
            .submit_published(
                &actor,
                &receipt.agent_ref,
                json!({}),
                "first",
                target.clone(),
            )
            .unwrap();
        let binding = store
            .read(|data| Ok(data.runs[&first].model_budget.clone().unwrap()))
            .unwrap();
        store
            .transact(|data| {
                data.model_budgets
                    .get_mut(&(actor.workspace_id.clone(), binding.group.clone()))
                    .unwrap()
                    .admitted = 2;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            first,
            store
                .submit_published(
                    &actor,
                    &receipt.agent_ref,
                    json!({}),
                    "first",
                    target.clone()
                )
                .unwrap()
        );
        let second = store
            .submit_published(&actor, &receipt.agent_ref, json!({}), "second", target)
            .unwrap();
        let second_binding = store
            .read(|data| Ok(data.runs[&second].model_budget.clone().unwrap()))
            .unwrap();
        assert_ne!(binding, second_binding);
        assert_eq!(
            store
                .read(|data| Ok(data.model_budgets
                    [&(actor.workspace_id.clone(), second_binding.group.clone())]
                    .admitted))
                .unwrap(),
            0
        );
        let (_, config) = store.published_run_configuration(&actor, second).unwrap();
        let tree = config.build_admission_tree(store.clone(), &actor).unwrap();
        store
            .validate_model_budget(&actor, second, &tree.runtime.model_budget_binding())
            .unwrap();
        assert_eq!(
            store
                .customer_agent(
                    &actor,
                    &Reference {
                        id: receipt.agent_ref.id,
                        version: 1
                    }
                )
                .unwrap()
                .name,
            "assistant"
        );
    }
    #[test]
    fn identical_customer_retries_keep_original_defaults_after_catalog_changes() {
        let store = Store::default();
        let original = catalog();
        let actor = original.owner.clone();
        original.install(&store, &actor).unwrap();
        let tool_receipt = store
            .publish_customer_tool(&actor, &original, tool(), "tool")
            .unwrap();
        let agent_receipt = store
            .publish_customer_agent(&actor, &original, agent(), "agent")
            .unwrap();
        let mut changed = original.clone();
        changed.limits.max_model_calls = 1;
        changed.connections.clear();
        changed.install(&store, &actor).unwrap();
        assert_eq!(
            tool_receipt,
            store
                .publish_customer_tool(&actor, &changed, tool(), "tool-retry")
                .unwrap()
        );
        assert_eq!(
            agent_receipt,
            store
                .publish_customer_agent(&actor, &changed, agent(), "agent-retry")
                .unwrap()
        );
        let mut next = agent();
        next.version = 2;
        assert!(store
            .publish_customer_agent(&actor, &changed, next, "new-revision")
            .is_err());
        let saved = store
            .published_configuration(&actor, &agent_receipt.agent_ref)
            .unwrap()
            .freeze_publication()
            .unwrap();
        assert_eq!(
            saved["definition"]["limits"]["max_model_calls"],
            original.limits.max_model_calls
        );
    }
    #[test]
    fn customer_walkthrough_publishes_offline() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/customer-api");
        let catalog = HostCatalog::load(&root.join("host.json")).unwrap();
        let store = Store::default();
        let actor = &catalog.owner;
        catalog.install(&store, actor).unwrap();
        let tool: Value =
            serde_json::from_slice(&std::fs::read(root.join("tool.json")).unwrap()).unwrap();
        store
            .publish_customer_tool(
                actor,
                &catalog,
                serde_json::from_value(tool["tool"].clone()).unwrap(),
                tool["request_key"].as_str().unwrap(),
            )
            .unwrap();
        let agent: Value =
            serde_json::from_slice(&std::fs::read(root.join("agent.json")).unwrap()).unwrap();
        store
            .publish_customer_agent(
                actor,
                &catalog,
                serde_json::from_value(agent["agent"].clone()).unwrap(),
                agent["request_key"].as_str().unwrap(),
            )
            .unwrap();
    }
}
