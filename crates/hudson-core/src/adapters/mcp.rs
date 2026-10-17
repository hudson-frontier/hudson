//! Pinned MCP tools executed only through the ordinary runtime tool boundary.
//! The host chooses schemas and effects; remote annotations never grant authority.
use super::tools::{ExecutionError, ToolRegistry};
use crate::{definitions::digest, models::*, Error, Result};
use futures::{Stream, StreamExt, TryStream};
use http::{HeaderName, HeaderValue};
use rmcp::{
    model::{CallToolRequestParams, ClientJsonRpcMessage, PaginatedRequestParams},
    transport::{
        common::client_side_sse::BoxedSseResponse,
        streamable_http_client::{
            SseError, StreamableHttpClient, StreamableHttpClientTransportConfig,
            StreamableHttpError, StreamableHttpPostResponse,
        },
        StreamableHttpClientTransport,
    },
    ServiceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sse_stream::{Sse, SseStream};
use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpToolBinding {
    pub name: String,
    pub remote_name: String,
    pub description: String,
    pub input_schema: Value,
    pub effect: Effect,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub endpoint: String,
    pub bearer_env: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_limit")]
    pub max_response_bytes: usize,
    pub tools: Vec<McpToolBinding>,
}
fn default_timeout() -> u64 {
    60
}
fn default_limit() -> usize {
    1_048_576
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiscoveredTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl McpServerConfig {
    pub fn validate(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.endpoint)
            .map_err(|_| Error::Invalid("invalid MCP endpoint".into()))?;
        let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if (url.scheme() != "https" && !(url.scheme() == "http" && local))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Invalid(
                "MCP requires HTTPS or loopback HTTP without URL credentials".into(),
            ));
        }
        if !(1..=300).contains(&self.timeout_seconds)
            || !(1024..=4_194_304).contains(&self.max_response_bytes)
            || self.tools.len() > 128
        {
            return Err(Error::Invalid(
                "invalid MCP timeout, output limit, or tool count".into(),
            ));
        }
        let mut names = BTreeSet::new();
        for tool in &self.tools {
            if tool.name.is_empty()
                || tool.name.len() > 64
                || !tool
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || tool.remote_name.is_empty()
                || tool.remote_name.len() > 128
                || tool.description.len() > 8192
                || !names.insert(&tool.name)
                || !tool.input_schema.is_object()
            {
                return Err(Error::Invalid(
                    "invalid or duplicate MCP tool binding".into(),
                ));
            }
            crate::definitions::validate_schema(&tool.input_schema)?;
        }
        Ok(())
    }
}

/// Offline registration. Initializing/listing a server never dispatches a tool.
/// A new schema requires a new immutable execution key and agent definition.
pub fn register_tools(
    config: &McpServerConfig,
    registry: &mut ToolRegistry,
    workspace: &str,
    policy: &str,
) -> Result<Vec<Tool>> {
    config.validate()?;
    let fingerprint = digest(config)?;
    let mut tools = Vec::new();
    for binding in &config.tools {
        let key = format!("hudson.mcp.{fingerprint}.{}", binding.name);
        let tool = Tool {
            id: key.clone(),
            workspace_id: workspace.into(),
            version: 1,
            schema_version: SCHEMA_VERSION,
            name: binding.name.clone(),
            description: binding.description.clone(),
            input_schema: binding.input_schema.clone(),
            output_schema: None,
            execution: Execution::Registered { key: key.clone() },
            credential_ref: None,
            policy_ref: policy.into(),
            effect: binding.effect.clone(),
            created_at: now(),
        };
        let config = config.clone();
        let binding = binding.clone();
        registry.register(key, move |call| {
            let config = config.clone();
            let binding = binding.clone();
            let arguments = call.arguments.clone();
            let operation_id = call.operation_id.to_string();
            // A dedicated runtime thread works both from synchronous and Tokio hosts.
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| ExecutionError::Failed("cannot initialize MCP runtime".into()))?;
                runtime.block_on(invoke(config, binding, arguments, operation_id))
            })
            .join()
            .unwrap_or_else(|_| Err(ExecutionError::Unknown("MCP executor interrupted".into())))
        })?;
        tools.push(tool);
    }
    Ok(tools)
}

fn failed() -> ExecutionError {
    ExecutionError::Failed("MCP connection or discovery failed before tool dispatch".into())
}
fn unknown() -> ExecutionError {
    ExecutionError::Unknown("MCP outcome uncertain; reconcile destination before retry".into())
}

async fn connect(
    config: &McpServerConfig,
) -> std::result::Result<rmcp::service::RunningService<rmcp::RoleClient, ()>, ExecutionError> {
    let token = config
        .bearer_env
        .as_ref()
        .map(|key| std::env::var(key).map_err(|_| failed()))
        .transpose()?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.timeout_seconds))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| failed())?;
    let mut transport = StreamableHttpClientTransportConfig::with_uri(config.endpoint.clone());
    transport.auth_header = token;
    transport.reinit_on_expired_session = false; // Never replay a tools/call after an expired session.
    transport.max_concurrent_requests = 1;
    transport.max_sse_event_size = config.max_response_bytes;
    ().serve(StreamableHttpClientTransport::with_client(
        BoundedHttp {
            client,
            limit: config.max_response_bytes,
        },
        transport,
    ))
    .await
    .map_err(|_| failed())
}

async fn discover(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
) -> std::result::Result<Vec<DiscoveredTool>, ExecutionError> {
    let mut cursor = None;
    let mut seen = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut tools = Vec::new();
    for _ in 0..16 {
        let page = client
            .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
            .await
            .map_err(|_| failed())?;
        for tool in page.tools {
            if tools.len() >= 128 || !names.insert(tool.name.to_string()) {
                return Err(failed());
            }
            tools.push(DiscoveredTool {
                name: tool.name.to_string(),
                description: tool.description.map(|s| s.to_string()).unwrap_or_default(),
                input_schema: Value::Object((*tool.input_schema).clone()),
            });
        }
        cursor = page.next_cursor;
        match &cursor {
            None => return Ok(tools),
            Some(value) if !seen.insert(value.clone()) => return Err(failed()),
            _ => {}
        }
    }
    Err(failed())
}

/// Authoring-time discovery. Persist selected schemas and assign effects explicitly.
pub fn discover_tools(config: &McpServerConfig) -> Result<Vec<DiscoveredTool>> {
    config.validate()?;
    let config = config.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| failed())?;
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(config.timeout_seconds), async {
                let client = connect(&config).await?;
                let result = discover(&client).await;
                let _ = tokio::time::timeout(Duration::from_secs(1), client.cancel()).await;
                result
            })
            .await
            .map_err(|_| failed())?
        })
    })
    .join()
    .map_err(|_| Error::Invalid("MCP discovery interrupted".into()))?
    .map_err(|_| Error::Invalid("MCP discovery failed".into()))
}

async fn invoke(
    config: McpServerConfig,
    binding: McpToolBinding,
    arguments: Value,
    operation_id: String,
) -> std::result::Result<Value, ExecutionError> {
    let timeout = Duration::from_secs(config.timeout_seconds);
    let client = tokio::time::timeout(timeout, async {
        let client = connect(&config).await?;
        let tools = discover(&client).await?;
        let matching = tools
            .iter()
            .find(|tool| tool.name == binding.remote_name)
            .ok_or_else(failed)?;
        if matching.input_schema != binding.input_schema {
            return Err(ExecutionError::Failed(
                "MCP schema changed; publish a new agent version".into(),
            ));
        }
        Ok(client)
    })
    .await
    .map_err(|_| failed())??;
    let arguments = arguments.as_object().cloned().ok_or_else(failed)?;
    let mut request = CallToolRequestParams::new(binding.remote_name).with_arguments(arguments);
    // Correlation metadata is advisory; MCP does not guarantee idempotency.
    request.meta = Some(
        serde_json::from_value(serde_json::json!({"hudson/operation_id": operation_id}))
            .map_err(|_| failed())?,
    );
    let result = tokio::time::timeout(timeout, client.call_tool(request)).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), client.cancel()).await;
    let result = result.map_err(|_| unknown())?.map_err(|_| unknown())?;
    if result.is_error == Some(true) {
        return Err(unknown());
    }
    let value = serde_json::to_value(result).map_err(|_| unknown())?;
    if serde_json::to_vec(&value).map_err(|_| unknown())?.len() > config.max_response_bytes {
        return Err(unknown());
    }
    Ok(value)
}

/// SDK lifecycle/protocol with a byte-bounded transport (including JSON, not just SSE).
#[derive(Clone)]
struct BoundedHttp {
    client: reqwest::Client,
    limit: usize,
}
type HttpError = StreamableHttpError<std::io::Error>;
fn wire_error() -> HttpError {
    StreamableHttpError::Io(std::io::Error::other("MCP transport failed"))
}

// RMCP still exposes SSE 0.2 types. Parse with 0.3, then move the event fields
// into the SDK's event type without cloning or serializing the payload again.
type SdkSse = <BoxedSseResponse as TryStream>::Ok;
#[expect(
    clippy::field_reassign_with_default,
    reason = "the SDK event is an associated type and cannot use a struct literal"
)]
fn sdk_sse_stream(
    stream: impl Stream<Item = std::result::Result<Sse, sse_stream::Error>> + Send + 'static,
) -> BoxedSseResponse {
    stream
        .map(|result| {
            result
                .map(|event| {
                    let mut converted = SdkSse::default();
                    converted.event = event.event;
                    converted.data = event.data;
                    converted.id = event.id;
                    converted.retry = event.retry;
                    converted
                })
                .map_err(|error| match error {
                    sse_stream::Error::Body(source) => SseError::Body(source),
                    sse_stream::Error::Utf8Parse(source) => SseError::Utf8Parse(source),
                    other => SseError::Body(Box::new(other)),
                })
        })
        .boxed()
}

impl StreamableHttpClient for BoundedHttp {
    type Error = std::io::Error;
    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> std::result::Result<StreamableHttpPostResponse, HttpError> {
        let mut request = self
            .client
            .post(uri.as_ref())
            .header("Accept", "application/json, text/event-stream")
            .json(&message);
        if let Some(session) = session_id {
            request = request.header("Mcp-Session-Id", session.as_ref());
        }
        if let Some(token) = auth_header {
            request = request.bearer_auth(token);
        }
        for (key, value) in custom_headers {
            request = request.header(key, value);
        }
        let mut response = request.send().await.map_err(|_| wire_error())?;
        if !response.status().is_success() {
            return Err(wire_error());
        }
        if response.status().as_u16() == 202 || response.status().as_u16() == 204 {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        let session = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let sse = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|s| s.starts_with("text/event-stream"));
        if response
            .content_length()
            .is_some_and(|n| n > self.limit as u64)
        {
            return Err(wire_error());
        }
        if sse {
            let stream = futures::stream::try_unfold(
                (response, self.limit),
                |(mut response, remaining)| async move {
                    match response
                        .chunk()
                        .await
                        .map_err(|_| std::io::Error::other("MCP stream failed"))?
                    {
                        Some(bytes) if bytes.len() <= remaining => {
                            let remaining = remaining - bytes.len();
                            Ok(Some((bytes, (response, remaining))))
                        }
                        Some(_) => Err(std::io::Error::other("MCP response limit exceeded")),
                        None => Ok(None),
                    }
                },
            );
            return Ok(StreamableHttpPostResponse::Sse(
                sdk_sse_stream(SseStream::from_bytes_stream(stream)),
                session,
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| wire_error())? {
            if chunk.len() > self.limit.saturating_sub(bytes.len()) {
                return Err(wire_error());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(StreamableHttpPostResponse::Json(
            serde_json::from_slice(&bytes).map_err(|_| wire_error())?,
            session,
        ))
    }
    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> std::result::Result<(), HttpError> {
        let mut request = self
            .client
            .delete(uri.as_ref())
            .header("Mcp-Session-Id", session_id.as_ref());
        if let Some(token) = auth_header {
            request = request.bearer_auth(token);
        }
        for (key, value) in custom_headers {
            request = request.header(key, value);
        }
        let response = request.send().await.map_err(|_| wire_error())?;
        if response.status().is_success() || matches!(response.status().as_u16(), 404 | 405) {
            Ok(())
        } else {
            Err(wire_error())
        }
    }
    async fn get_stream(
        &self,
        _uri: Arc<str>,
        _session_id: Option<Arc<str>>,
        _last_event_id: Option<String>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> std::result::Result<BoxedSseResponse, HttpError> {
        Err(StreamableHttpError::ServerDoesNotSupportSse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sse_bridge_preserves_fragmented_events_and_metadata() {
        let chunks = [
            &b"event: message\r\ndata: first\r\ndata: caf\xc3"[..],
            &b"\xa9\r\nid: event-1\r\nretry: 250\r\n\r"[..],
            &b"\nid:\ndata:\n\n"[..],
        ];
        let bytes = futures::stream::iter(chunks.map(Ok::<_, std::io::Error>));
        let mut events = sdk_sse_stream(SseStream::from_bytes_stream(bytes));
        let event = events.next().await.unwrap().unwrap();
        assert_eq!(event.event.as_deref(), Some("message"));
        assert_eq!(event.data.as_deref(), Some("first\ncaf\u{e9}"));
        assert_eq!(event.id.as_deref(), Some("event-1"));
        assert_eq!(event.retry, Some(250));
        let empty = events.next().await.unwrap().unwrap();
        assert_eq!(empty.event, None);
        assert_eq!(empty.data.as_deref(), Some(""));
        assert_eq!(empty.id.as_deref(), Some(""));
        assert_eq!(empty.retry, None);
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn sse_bridge_preserves_terminal_parse_and_body_errors() {
        let bytes = futures::stream::iter([Ok::<_, std::io::Error>(&b"data: \xff\n\n"[..])]);
        let mut events = sdk_sse_stream(SseStream::from_bytes_stream(bytes));
        assert!(matches!(
            events.next().await,
            Some(Err(SseError::Utf8Parse(_)))
        ));
        assert!(events.next().await.is_none());

        let bytes = futures::stream::iter([Err::<&[u8], _>(std::io::Error::other(
            "MCP response limit exceeded",
        ))]);
        let mut events = sdk_sse_stream(SseStream::from_bytes_stream(bytes));
        let Some(Err(SseError::Body(source))) = events.next().await else {
            panic!("expected the response limit error");
        };
        assert_eq!(source.to_string(), "MCP response limit exceeded");
        assert!(source.downcast_ref::<std::io::Error>().is_some());
        assert!(events.next().await.is_none());
    }
}
