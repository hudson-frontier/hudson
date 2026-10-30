//! Operator-owned SDK bridges. Only proposals cross this boundary; Hudson owns
//! tool execution, history, approval and verification. Bridges are trusted code;
//! explicit Linux isolation can restrict their filesystem and network access.
use super::{
    models::{ModelExecutor, ModelObserver},
    tools::ExecutionError,
};
use crate::{models::TokenUsage, Error, Result};
use hudson_harness::{ModelRequest, ModelResponse};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
mod isolation;
mod process;
mod stream;
pub use isolation::{IsolationNetwork, LinuxIsolation};
pub use stream::Frame as BridgeStreamFrame;

pub const PROTOCOL_VERSION: u32 = 1;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const STDERR_BYTES: usize = 64 * 1024;
fn timeout() -> u64 {
    120
}
fn bytes() -> usize {
    1024 * 1024
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    pub id: String,
    pub version: u32,
    pub transport: BridgeTransport,
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "bytes")]
    pub max_request_bytes: usize,
    #[serde(default = "bytes")]
    pub max_response_bytes: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub images: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub native_sessions: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<super::reasoning::Effort>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum BridgeTransport {
    Http {
        endpoint: String,
        #[serde(default)]
        bearer_env: Option<String>,
    },
    Stdio {
        executable: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        cwd: PathBuf,
        /// Names only. Values are captured at construction and never serialized.
        #[serde(default)]
        env: Vec<String>,
    },
    IsolatedStdio {
        executable: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        cwd: PathBuf,
        #[serde(default)]
        env: Vec<String>,
        isolation: LinuxIsolation,
    },
}

/// Public discovery metadata deliberately excludes endpoints, paths and secrets.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionDescriptor {
    pub id: String,
    pub version: u32,
    pub transport: String,
    pub capabilities: ExecutionCapabilities,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCapabilities {
    pub tools: bool,
    pub approvals: bool,
    pub resume: bool,
    pub follow_up: bool,
    pub steering: bool,
    pub subagents: bool,
    pub interrupt: bool,
    pub streaming: bool,
    pub images: bool,
    pub native_sessions: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reasoning_effort: bool,
}
impl ExecutionDescriptor {
    pub(crate) fn new(id: String, version: u32, transport: &str, interrupt: bool) -> Self {
        Self {
            id,
            version,
            transport: transport.into(),
            capabilities: ExecutionCapabilities {
                tools: true,
                approvals: true,
                resume: true,
                follow_up: true,
                steering: true,
                subagents: true,
                interrupt,
                streaming: false,
                images: transport == "provider_http",
                native_sessions: false,
                reasoning_effort: false,
            },
        }
    }
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}
fn failed(message: &str) -> ExecutionError {
    ExecutionError::Failed(message.into())
}
fn unknown(message: &str) -> ExecutionError {
    ExecutionError::Unknown(message.into())
}

fn endpoint(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value).map_err(|_| invalid("invalid runtime bridge endpoint"))?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !(url.scheme() == "https" || url.scheme() == "http" && local)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid("runtime bridge requires HTTPS or loopback HTTP without URL credentials, query or fragment"));
    }
    Ok(url)
}
fn env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .enumerate()
            .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || i > 0 && c.is_ascii_digit())
}
impl BridgeConfig {
    /// Pure validation. Does not resolve secrets, spawn a process or contact a service.
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || self.id.len() > 64
            || !self
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
            || self.version == 0
            || !(1..=300).contains(&self.timeout_seconds)
            || !(1..=MAX_BYTES).contains(&self.max_request_bytes)
            || !(1..=MAX_BYTES).contains(&self.max_response_bytes)
        {
            return Err(invalid(
                "invalid runtime bridge identity or resource limits",
            ));
        }
        match &self.transport {
            BridgeTransport::Http {
                endpoint: value,
                bearer_env,
            } => {
                endpoint(value)?;
                if bearer_env.as_ref().is_some_and(|name| !env_name(name)) {
                    return Err(invalid("invalid runtime bridge credential reference"));
                }
            }
            BridgeTransport::Stdio {
                executable,
                args,
                cwd,
                env,
            }
            | BridgeTransport::IsolatedStdio {
                executable,
                args,
                cwd,
                env,
                ..
            } => {
                if !cfg!(any(unix, windows)) {
                    return Err(Error::Unsupported(
                        "stdio runtime bridges require process containment".into(),
                    ));
                }
                if let BridgeTransport::IsolatedStdio { isolation, .. } = &self.transport {
                    isolation.validate(executable, cwd)?;
                }
                if !executable.is_absolute()
                    || !cwd.is_absolute()
                    || args.len() > 64
                    || args.iter().map(String::len).sum::<usize>() > 32768
                    || args.iter().any(|arg| arg.contains('\0'))
                    || env.len() > 32
                    || env.iter().any(|name| !env_name(name))
                    || env.iter().collect::<BTreeSet<_>>().len() != env.len()
                {
                    return Err(invalid("stdio bridges require absolute paths, bounded arguments and unique environment references"));
                }
            }
        }
        Ok(())
    }
    pub fn descriptor(&self) -> ExecutionDescriptor {
        let mut descriptor = ExecutionDescriptor::new(
            self.id.clone(),
            self.version,
            match self.transport {
                BridgeTransport::Http { .. } => "http_bridge",
                BridgeTransport::Stdio { .. } => "stdio_bridge",
                BridgeTransport::IsolatedStdio { .. } => "isolated_stdio_bridge",
            },
            matches!(
                self.transport,
                BridgeTransport::Stdio { .. } | BridgeTransport::IsolatedStdio { .. }
            ),
        );
        descriptor.capabilities.streaming = self.stream;
        descriptor.capabilities.images = self.images;
        descriptor.capabilities.native_sessions = self.native_sessions;
        descriptor.capabilities.reasoning_effort = self.reasoning_effort.is_some();
        descriptor
    }
}

#[derive(Serialize)]
pub struct BridgeRequest<'a> {
    pub protocol_version: u32,
    pub runtime_id: &'a str,
    pub runtime_version: u32,
    pub max_output_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<super::reasoning::Effort>,
    pub request: &'a ModelRequest,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<super::models::RuntimeCallContext>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeResponse {
    pub protocol_version: u32,
    pub runtime_id: String,
    pub runtime_version: u32,
    pub response: ModelResponse,
    #[serde(default)]
    pub usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<super::models::RuntimeCheckpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<super::reasoning::Effort>,
}

enum Transport {
    Http {
        client: reqwest::Client,
        endpoint: reqwest::Url,
        bearer: Option<String>,
    },
    Stdio {
        executable: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
        env: Vec<(String, String)>,
    },
}
pub struct BridgeModel {
    config: BridgeConfig,
    transport: std::sync::Arc<Transport>,
    io: Option<tokio::runtime::Runtime>,
    max_output_tokens: u32,
    usage: Option<TokenUsage>,
}
impl BridgeModel {
    /// Construct on a blocking host thread, like the other synchronous executors.
    pub fn new(config: BridgeConfig, max_output_tokens: u32) -> Result<Self> {
        config.validate()?;
        if max_output_tokens == 0 {
            return Err(invalid(
                "runtime bridge requires a positive output token limit",
            ));
        }
        let transport = match &config.transport {
            BridgeTransport::Http {
                endpoint: value,
                bearer_env,
            } => {
                let endpoint = endpoint(value)?;
                let local = endpoint.scheme() == "http";
                let builder = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .connect_timeout(Duration::from_secs(10))
                    .timeout(Duration::from_secs(config.timeout_seconds));
                let client = if local { builder.no_proxy() } else { builder }
                    .build()
                    .map_err(|_| invalid("cannot construct runtime bridge client"))?;
                let bearer = bearer_env
                    .as_ref()
                    .map(|name| {
                        std::env::var(name)
                            .ok()
                            .filter(|v| !v.trim().is_empty() && v.len() <= 8192)
                            .ok_or_else(|| {
                                invalid("runtime bridge credential is missing or invalid")
                            })
                    })
                    .transpose()?;
                Transport::Http {
                    client,
                    endpoint,
                    bearer,
                }
            }
            BridgeTransport::Stdio {
                executable,
                args,
                cwd,
                env,
            }
            | BridgeTransport::IsolatedStdio {
                executable,
                args,
                cwd,
                env,
                ..
            } => {
                if !executable.is_file() || !cwd.is_dir() {
                    return Err(invalid(
                        "runtime bridge executable or working directory is unavailable",
                    ));
                }
                let env = env
                    .iter()
                    .map(|name| {
                        std::env::var(name)
                            .ok()
                            .filter(|v| v.len() <= 32768 && !v.contains('\0'))
                            .map(|v| (name.clone(), v))
                            .ok_or_else(|| {
                                invalid(
                                    "runtime bridge environment reference is missing or invalid",
                                )
                            })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let (executable, args) = match &config.transport {
                    BridgeTransport::IsolatedStdio { isolation, .. } => (
                        isolation.bubblewrap.clone(),
                        isolation.prepare(executable, args, cwd)?,
                    ),
                    _ => (executable.clone(), args.clone()),
                };
                Transport::Stdio {
                    executable,
                    args,
                    cwd: cwd.clone(),
                    env,
                }
            }
        };
        let io = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(2)
            .enable_all()
            .build()
            .map_err(|_| invalid("cannot construct runtime bridge IO worker"))?;
        Ok(Self {
            config,
            transport: std::sync::Arc::new(transport),
            io: Some(io),
            max_output_tokens,
            usage: None,
        })
    }
}

impl Drop for BridgeModel {
    fn drop(&mut self) {
        // A configured Axum host can release its last driver from an async
        // task. Runtime's ordinary Drop would block (and panic) in that context.
        // Calls join their IO task before returning; only idle IO remains here.
        if let Some(io) = self.io.take() {
            io.shutdown_background();
        }
    }
}

/// Serialization stops before allocating an oversized request.
struct BoundedBytes {
    bytes: Vec<u8>,
    limit: usize,
}
impl std::io::Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit - self.bytes.len() {
            return Err(std::io::Error::other(
                "runtime bridge request exceeds byte limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl ModelExecutor for BridgeModel {
    fn take_usage(&mut self) -> Option<TokenUsage> {
        self.usage.take()
    }
    fn call(
        &mut self,
        request: &ModelRequest,
    ) -> std::result::Result<ModelResponse, ExecutionError> {
        self.call_controlled(request, &|| false)
    }
    fn call_controlled(
        &mut self,
        request: &ModelRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> std::result::Result<ModelResponse, ExecutionError> {
        struct Control<'a>(&'a dyn Fn() -> bool);
        impl ModelObserver for Control<'_> {
            fn cancelled(&self) -> bool {
                (self.0)()
            }
            fn text_delta(&mut self, _: &str) -> std::result::Result<(), ExecutionError> {
                Ok(())
            }
        }
        self.call_observed(request, &mut Control(cancelled))
    }
    fn call_observed(
        &mut self,
        request: &ModelRequest,
        observer: &mut dyn ModelObserver,
    ) -> std::result::Result<ModelResponse, ExecutionError> {
        self.usage = None;
        let has_images = request
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|c| matches!(c, hudson_harness::Content::Image { .. }));
        if has_images && !self.config.images {
            return Err(failed("runtime bridge does not declare image support"));
        }
        hudson_harness::images::validate_history(&request.messages)
            .map_err(|_| failed("invalid runtime image input"))?;
        if observer.cancelled() {
            return Err(failed("runtime bridge cancelled before dispatch"));
        }
        let mut body = BoundedBytes {
            bytes: Vec::new(),
            limit: self.config.max_request_bytes,
        };
        let context = if self.config.native_sessions {
            Some(observer.runtime_context(request)?.ok_or_else(|| {
                failed("native sessions require an admitted Hudson model context")
            })?)
        } else {
            None
        };
        let expected_checkpoint = context
            .as_ref()
            .map(|ctx| super::models::RuntimeCheckpoint {
                run_id: ctx.call.run_id,
                operation_id: ctx.call.operation_id,
                attempt_id: ctx.call.attempt_id,
                request_fingerprint: ctx.request_fingerprint.clone(),
                input_messages: request.messages.len(),
            });
        serde_json::to_writer(
            &mut body,
            &BridgeRequest {
                protocol_version: PROTOCOL_VERSION,
                runtime_id: &self.config.id,
                runtime_version: self.config.version,
                max_output_tokens: self.max_output_tokens,
                reasoning_effort: self.config.reasoning_effort,
                request,
                stream: self.config.stream,
                context,
            },
        )
        .map_err(|_| failed("runtime bridge request exceeds limit or cannot be encoded"))?;
        let transport = self.transport.clone();
        let limit = self.config.max_response_bytes;
        let deadline = Duration::from_secs(self.config.timeout_seconds);
        let (sender, receiver) =
            std::sync::mpsc::sync_channel(if self.config.stream { 16 } else { 1 });
        let identity = stream::Identity {
            id: self.config.id.clone(),
            version: self.config.version,
        };
        let streaming = self.config.stream;
        struct IoTask(tokio::task::JoinHandle<()>);
        impl Drop for IoTask {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let io = self.io.as_ref().expect("live bridge runtime");
        let mut task = IoTask(io.spawn(async move {
            let result = tokio::time::timeout(deadline, async {
                if streaming {
                    stream::exchange(&transport, body.bytes, limit, &identity, &sender).await
                } else {
                    exchange(&transport, body.bytes, limit).await
                }
            })
            .await
            .unwrap_or_else(|_| {
                Err(unknown(
                    "runtime bridge deadline exceeded; no automatic retry",
                ))
            });
            let _ = stream::send(&sender, stream::Notice::Complete(result)).await;
        }));
        let mut last_control = std::time::Instant::now();
        let result = loop {
            if last_control.elapsed() >= Duration::from_millis(250) {
                if observer.cancelled() {
                    task.0.abort();
                    break Err(unknown("runtime bridge interrupted; no automatic retry"));
                }
                if let Err(error) = observer.heartbeat() {
                    task.0.abort();
                    break Err(error);
                }
                last_control = std::time::Instant::now();
            }
            match receiver.recv_timeout(Duration::from_millis(250)) {
                Ok(stream::Notice::Complete(result)) => break result,
                Ok(stream::Notice::Text(text)) => {
                    if let Err(error) = observer.text_delta(&text) {
                        task.0.abort();
                        break Err(error);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(unknown("runtime bridge IO worker unavailable"))
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    continue;
                }
            }
        };
        // Join abortion too: child/group guards run before returning control.
        let _ = io.block_on(&mut task.0);
        let response: BridgeResponse = serde_json::from_slice(&result?)
            .map_err(|_| failed("invalid runtime bridge response"))?;
        if response.protocol_version != PROTOCOL_VERSION
            || response.runtime_id != self.config.id
            || response.runtime_version != self.config.version
        {
            return Err(failed(
                "runtime bridge response identity or protocol mismatch",
            ));
        }
        self.usage = response.usage;
        if response.reasoning_effort != self.config.reasoning_effort {
            return Err(failed(
                "runtime reasoning receipt does not match the pinned setting",
            ));
        }
        if response.checkpoint != expected_checkpoint {
            return Err(failed(
                "runtime checkpoint receipt does not match the admitted model attempt",
            ));
        }
        if let ModelResponse::ToolCalls { calls } = &response.response {
            let mut ids = BTreeSet::new();
            if calls.is_empty()
                || calls.len() > 128
                || calls.iter().any(|call| {
                    call.call_id.trim().is_empty()
                        || call.call_id.len() > 256
                        || !ids.insert(&call.call_id)
                        || !request.tools.iter().any(|tool| tool.name == call.name)
                })
            {
                return Err(failed(
                    "runtime bridge proposed an invalid or unregistered tool batch",
                ));
            }
        }
        Ok(response.response)
    }
}

async fn bounded_read(
    reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::result::Result<Vec<u8>, ExecutionError> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| unknown("runtime bridge output unavailable; no automatic retry"))?;
    if bytes.len() > limit {
        return Err(failed("runtime bridge output exceeds byte limit"));
    }
    Ok(bytes)
}
async fn exchange(
    transport: &Transport,
    body: Vec<u8>,
    limit: usize,
) -> std::result::Result<Vec<u8>, ExecutionError> {
    match transport {
        Transport::Http {
            client,
            endpoint,
            bearer,
        } => {
            let mut request = client
                .post(endpoint.clone())
                .header("Content-Type", "application/json")
                .body(body);
            if let Some(bearer) = bearer {
                request = request.bearer_auth(bearer);
            }
            let mut response = request
                .send()
                .await
                .map_err(|_| unknown("runtime bridge transport failed; no automatic retry"))?;
            if !response.status().is_success() {
                return Err(unknown(
                    "runtime bridge returned an unsuccessful status; no automatic retry",
                ));
            }
            if response
                .content_length()
                .is_some_and(|len| len > limit as u64)
            {
                return Err(failed("runtime bridge output exceeds byte limit"));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| unknown("runtime bridge response interrupted; no automatic retry"))?
            {
                if chunk.len() > limit - bytes.len() {
                    return Err(failed("runtime bridge output exceeds byte limit"));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        }
        Transport::Stdio {
            executable,
            args,
            cwd,
            env,
        } => process(executable, args, cwd, env, body, limit).await,
    }
}

async fn process(
    executable: &std::path::Path,
    args: &[String],
    cwd: &std::path::Path,
    env: &[(String, String)],
    body: Vec<u8>,
    limit: usize,
) -> std::result::Result<Vec<u8>, ExecutionError> {
    let mut child = process::ContainedChild::spawn(executable, args, cwd, env)?;
    let mut stdin = child.stdin();
    let stdout = child.stdout();
    let stderr = child.stderr();
    let write = async move {
        stdin
            .write_all(&body)
            .await
            .map_err(|_| unknown("runtime bridge input interrupted; no automatic retry"))?;
        stdin
            .shutdown()
            .await
            .map_err(|_| unknown("runtime bridge input interrupted; no automatic retry"))
    };
    // Drain all three pipes concurrently; a noisy peer cannot deadlock input.
    let (_, output, _) = tokio::try_join!(
        write,
        bounded_read(stdout, limit),
        bounded_read(stderr, STDERR_BYTES)
    )?;
    let status = child.wait().await?;
    if !status.success() {
        return Err(unknown(
            "runtime bridge exited unsuccessfully; no automatic retry",
        ));
    }
    Ok(output)
}
