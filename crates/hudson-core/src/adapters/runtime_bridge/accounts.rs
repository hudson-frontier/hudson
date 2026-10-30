//! Explicit account control sessions, separate from immutable model execution.
//! Login challenges live only in bounded session memory, never in checkpoints.
use super::{bounded_read, process::ContainedChild, BridgeConfig, BridgeTransport};
use crate::{models::Actor, Error, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

const RESPONSE_BYTES: usize = 64 * 1024;
const MAX_ACTIONS: usize = 32;

/// Installation-owned command. Validation and catalog discovery perform no IO.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    pub id: String,
    pub runtime_id: String,
    pub runtime_version: u32,
    pub transport: BridgeTransport,
    /// Readiness-only references may be absent. Present values remain bounded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub optional_env: Vec<String>,
    #[serde(default = "read_only")]
    pub actions: Vec<AccountAction>,
    #[serde(default = "lifetime")]
    pub lifetime_seconds: u64,
}
fn lifetime() -> u64 {
    300
}
fn read_only() -> Vec<AccountAction> {
    vec![AccountAction::Status]
}
impl AccountConfig {
    pub fn validate(&self) -> Result<()> {
        if !identifier(&self.id)
            || !(1..=300).contains(&self.lifetime_seconds)
            || !matches!(self.transport, BridgeTransport::Stdio { .. })
            || self.actions.is_empty()
            || self.actions.len() > 6
            || self
                .actions
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.actions.len()
        {
            return Err(Error::Invalid(
                "account controls require a bounded contained stdio command".into(),
            ));
        }
        self.bridge().validate()?;
        let BridgeTransport::Stdio { env, .. } = &self.transport else {
            unreachable!("validated stdio")
        };
        let names = env.iter().chain(&self.optional_env).collect::<Vec<_>>();
        if names.len() > 32
            || names.iter().any(|name| !super::env_name(name))
            || names
                .iter()
                .map(|name| {
                    if cfg!(windows) {
                        name.to_ascii_uppercase()
                    } else {
                        (*name).clone()
                    }
                })
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != names.len()
        {
            return Err(Error::Invalid(
                "account environment references must be bounded and unique".into(),
            ));
        }
        Ok(())
    }
    fn bridge(&self) -> BridgeConfig {
        BridgeConfig {
            id: self.runtime_id.clone(),
            version: self.runtime_version,
            transport: self.transport.clone(),
            timeout_seconds: self.lifetime_seconds,
            max_request_bytes: 8192,
            max_response_bytes: RESPONSE_BYTES,
            stream: false,
            images: false,
            native_sessions: false,
            reasoning_effort: None,
        }
    }
    pub fn descriptor(&self) -> AccountDescriptor {
        AccountDescriptor {
            id: self.id.clone(),
            runtime_id: self.runtime_id.clone(),
            runtime_version: self.runtime_version,
            lifetime_seconds: self.lifetime_seconds,
            actions: self.actions.clone(),
        }
    }
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountDescriptor {
    pub id: String,
    pub runtime_id: String,
    pub runtime_version: u32,
    pub lifetime_seconds: u64,
    pub actions: Vec<AccountAction>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum AccountAction {
    Status,
    Login,
    Renew,
    CancelLogin,
    Logout,
    Models,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialState {
    Required,
    Configured,
    NotRequired,
    Unknown,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LoginState {
    None,
    Pending,
    Completed,
    Failed,
    Cancelled,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountStatus {
    pub expected_version: String,
    pub installed_version: Option<String>,
    pub protocol_ready: bool,
    pub credential: CredentialState,
    pub login: LoginState,
}
/// Deliberately no Debug: the authorization URL is a transient login challenge.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountOutcome {
    Status { status: AccountStatus },
    Login { authorization_url: String },
    Cancelled,
    LoggedOut,
    Models { models: Vec<String> },
    Unsupported,
}
impl AccountOutcome {
    fn validate(&self, action: AccountAction) -> bool {
        match (self, action) {
            (Self::Status { status }, AccountAction::Status | AccountAction::Renew) => {
                let valid = |s: &str| {
                    !s.is_empty()
                        && s.len() <= 64
                        && s.bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
                };
                valid(&status.expected_version)
                    && status.installed_version.as_deref().is_none_or(valid)
            }
            (Self::Login { authorization_url }, AccountAction::Login) => {
                reqwest::Url::parse(authorization_url).is_ok_and(|url| {
                    authorization_url.len() <= 4096
                        && url.scheme() == "https"
                        && url.host_str().is_some()
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.fragment().is_none()
                })
            }
            (Self::Cancelled, AccountAction::CancelLogin)
            | (Self::LoggedOut, AccountAction::Logout) => true,
            (Self::Models { models }, AccountAction::Models) => {
                models.len() <= 128
                    && models.iter().all(|m| {
                        !m.is_empty()
                            && m.len() <= 128
                            && m.bytes()
                                .all(|c| c.is_ascii_alphanumeric() || b"._:/-".contains(&c))
                    })
            }
            (Self::Unsupported, _) => true,
            _ => false,
        }
    }
}
#[derive(Serialize)]
struct WireRequest<'a> {
    protocol_version: u32,
    account_id: &'a str,
    runtime_id: &'a str,
    runtime_version: u32,
    actor: &'a Actor,
    request_key: &'a str,
    action: AccountAction,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireResponse {
    protocol_version: u32,
    account_id: String,
    runtime_id: String,
    runtime_version: u32,
    request_key: String,
    response: AccountOutcome,
}
struct Command {
    key: String,
    action: AccountAction,
    reply: oneshot::Sender<Result<AccountOutcome>>,
}
struct Task(tokio::task::JoinHandle<()>);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}
/// One actor, one process tree, at most one queued command and 32 distinct actions.
/// Dropping the last handle, expiry or a failed exchange terminates the tree.
#[derive(Clone)]
pub struct AccountSession {
    actor: Actor,
    actions: Vec<AccountAction>,
    sender: mpsc::Sender<Command>,
    expires: tokio::time::Instant,
    _task: Arc<Task>,
}
impl AccountSession {
    /// Explicit session creation resolves only the installation's environment names.
    /// No model request, login or renewal is initiated here.
    pub fn open(config: AccountConfig, actor: Actor) -> Result<Self> {
        config.validate()?;
        let host = tokio::runtime::Handle::try_current().map_err(|_| {
            Error::Invalid("account sessions require an active asynchronous host".into())
        })?;
        if actor.id.trim().is_empty()
            || actor.workspace_id.trim().is_empty()
            || actor.id.len() > 256
            || actor.workspace_id.len() > 256
        {
            return Err(Error::Denied);
        }
        let (executable, args, cwd, env) = match &config.transport {
            BridgeTransport::Stdio {
                executable,
                args,
                cwd,
                env,
            } => {
                let mut values = env
                    .iter()
                    .map(|name| {
                        std::env::var(name)
                            .ok()
                            .filter(|value| value.len() <= 32768 && !value.contains('\0'))
                            .map(|value| (name.clone(), value))
                            .ok_or_else(|| {
                                Error::Invalid(
                                    "account environment reference is missing or invalid".into(),
                                )
                            })
                    })
                    .collect::<Result<Vec<_>>>()?;
                for name in &config.optional_env {
                    match std::env::var(name) {
                        Ok(value) if value.len() <= 32768 && !value.contains('\0') => {
                            values.push((name.clone(), value))
                        }
                        Err(std::env::VarError::NotPresent) => {}
                        _ => {
                            return Err(Error::Invalid(
                                "optional account environment reference is invalid".into(),
                            ))
                        }
                    }
                }
                (executable.clone(), args.clone(), cwd.clone(), values)
            }
            _ => unreachable!("validated stdio"),
        };
        let (sender, mut receiver) = mpsc::channel::<Command>(1);
        let actions = config.actions.clone();
        let owner = actor.clone();
        let expires = tokio::time::Instant::now() + Duration::from_secs(config.lifetime_seconds);
        let task = host.spawn(async move {
            let work = async {
                let mut child =
                    ContainedChild::spawn(&executable, &args, &cwd, &env).map_err(|_| ())?;
                let mut stdin = child.stdin();
                let mut stdout = BufReader::new(child.stdout());
                let stderr = child.stderr();
                let commands = async {
                    let mut cached = BTreeMap::<String, (AccountAction, AccountOutcome)>::new();
                    while let Some(command) = receiver.recv().await {
                        if let Some((action, result)) = cached.get(&command.key) {
                            let result = if *action == command.action {
                                Ok(result.clone())
                            } else {
                                Err(Error::Conflict(
                                    "account request key was reused for a different action".into(),
                                ))
                            };
                            let _ = command.reply.send(result);
                            continue;
                        }
                        if cached.len() >= MAX_ACTIONS {
                            let _ = command.reply.send(Err(Error::Conflict(
                                "account session action limit reached".into(),
                            )));
                            continue;
                        }
                        let exchange = async {
                            let mut body = serde_json::to_vec(&WireRequest {
                                protocol_version: 1,
                                account_id: &config.id,
                                runtime_id: &config.runtime_id,
                                runtime_version: config.runtime_version,
                                actor: &owner,
                                request_key: &command.key,
                                action: command.action,
                            })
                            .map_err(|_| ())?;
                            if body.len() > 8192 {
                                return Err(());
                            }
                            body.push(b'\n');
                            stdin.write_all(&body).await.map_err(|_| ())?;
                            stdin.flush().await.map_err(|_| ())?;
                            let mut raw = Vec::new();
                            (&mut stdout)
                                .take(RESPONSE_BYTES as u64 + 1)
                                .read_until(b'\n', &mut raw)
                                .await
                                .map_err(|_| ())?;
                            if raw.len() > RESPONSE_BYTES || raw.last() != Some(&b'\n') {
                                return Err(());
                            }
                            let response: WireResponse =
                                serde_json::from_slice(&raw).map_err(|_| ())?;
                            if response.protocol_version != 1
                                || response.account_id != config.id
                                || response.runtime_id != config.runtime_id
                                || response.runtime_version != config.runtime_version
                                || response.request_key != command.key
                                || !response.response.validate(command.action)
                            {
                                return Err(());
                            }
                            Ok(response.response)
                        };
                        let result = tokio::time::timeout(Duration::from_secs(30), exchange).await;
                        match result {
                            Ok(Ok(outcome)) => {
                                cached.insert(command.key, (command.action, outcome.clone()));
                                let _ = command.reply.send(Ok(outcome));
                            }
                            _ => {
                                let _ = command.reply.send(Err(Error::Conflict(
                                    "account action outcome unavailable; no automatic retry".into(),
                                )));
                                return Err(());
                            }
                        }
                    }
                    Ok(())
                };
                let diagnostics = async {
                    bounded_read(stderr, 64 * 1024).await.map_err(|_| ())?;
                    std::future::pending::<std::result::Result<(), ()>>().await
                };
                tokio::select! { result=commands => result, result=diagnostics => result, _=child.wait() => Err(()) }
            };
            let _ = tokio::time::timeout_at(expires, work).await;
        });
        Ok(Self {
            actor,
            actions,
            sender,
            expires,
            _task: Arc::new(Task(task)),
        })
    }
    pub fn expired(&self) -> bool {
        self.sender.is_closed() || tokio::time::Instant::now() >= self.expires
    }
    pub fn close(&self, actor: &Actor) -> Result<()> {
        if actor != &self.actor {
            return Err(Error::NotFound);
        }
        self._task.0.abort();
        Ok(())
    }
    pub async fn action(
        &self,
        actor: &Actor,
        key: &str,
        action: AccountAction,
    ) -> Result<AccountOutcome> {
        if actor != &self.actor {
            return Err(Error::NotFound);
        }
        if !self.actions.contains(&action) {
            return Err(Error::Unsupported(
                "account action is not configured".into(),
            ));
        }
        if key.trim().is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
            return Err(Error::Invalid(
                "account action requires a bounded explicit request key".into(),
            ));
        }
        if self.expired() {
            return Err(Error::Conflict(
                "account session expired; create a new explicit session".into(),
            ));
        }
        let (reply, receive) = oneshot::channel();
        self.sender
            .try_send(Command {
                key: key.into(),
                action,
                reply,
            })
            .map_err(|_| Error::Conflict("account session busy or closed".into()))?;
        receive.await.map_err(|_| {
            Error::Conflict("account session closed; action outcome may be unknown".into())
        })?
    }
}
