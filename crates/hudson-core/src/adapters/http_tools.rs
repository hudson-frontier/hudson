//! Explicit HTTP service bindings. Endpoints and credentials are configured by the host.
use super::tools::{ExecutionError, Invocation, ToolExecutor, ToolRegistry};
use crate::{models::Execution, Error, Result};
use std::{collections::BTreeMap, io::Read, time::Duration};

pub struct HttpTools {
    pub registered: ToolRegistry,
    client: reqwest::blocking::Client,
    local_client: Option<reqwest::blocking::Client>,
    bindings: BTreeMap<String, HttpBinding>,
    fixture_only: bool,
}
struct HttpBinding {
    bearer: Option<String>,
    direct: bool,
}
impl HttpTools {
    pub fn new(registered: ToolRegistry) -> Result<Self> {
        Self::build(registered, false)
    }
    pub(crate) fn fixture(registered: ToolRegistry) -> Result<Self> {
        Self::build(registered, true)
    }
    fn build(registered: ToolRegistry, no_proxy: bool) -> Result<Self> {
        Ok(Self {
            registered,
            client: Self::http_client(no_proxy)?,
            local_client: None,
            bindings: BTreeMap::new(),
            fixture_only: no_proxy,
        })
    }
    fn http_client(no_proxy: bool) -> Result<reqwest::blocking::Client> {
        let builder = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none());
        let builder = if no_proxy {
            builder.no_proxy()
        } else {
            builder
        };
        builder
            .build()
            .map_err(|_| Error::Invalid("cannot initialize HTTP tool client".into()))
    }
    pub fn bind(&mut self, endpoint: &str, bearer_token: Option<String>) -> Result<()> {
        let url = reqwest::Url::parse(endpoint)
            .map_err(|_| Error::Invalid("invalid HTTP tool endpoint".into()))?;
        let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if (url.scheme() != "https" && !(url.scheme() == "http" && local))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || (self.fixture_only
                && (!matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
                    || bearer_token.is_some()))
        {
            return Err(Error::Invalid(
                "tool endpoint requires HTTPS or local HTTP without URL credentials".into(),
            ));
        }
        if self.bindings.contains_key(endpoint) {
            return Err(Error::Conflict("HTTP endpoint already bound".into()));
        }
        let direct = !self.fixture_only && url.scheme() == "http" && local;
        if direct && self.local_client.is_none() {
            // All local bindings share one direct client; HTTPS keeps the normal client.
            self.local_client = Some(Self::http_client(true)?);
        }
        self.bindings.insert(
            endpoint.into(),
            HttpBinding {
                bearer: bearer_token,
                direct,
            },
        );
        Ok(())
    }
}
impl ToolExecutor for HttpTools {
    fn execute(
        &mut self,
        invocation: Invocation<'_>,
    ) -> std::result::Result<serde_json::Value, ExecutionError> {
        let Execution::Http { endpoint } = &invocation.tool.execution else {
            return self.registered.execute(invocation);
        };
        let binding = self
            .bindings
            .get(endpoint)
            .ok_or_else(|| ExecutionError::Failed("HTTP tool endpoint is not bound".into()))?;
        let client = if binding.direct {
            self.local_client.as_ref().ok_or_else(|| {
                ExecutionError::Failed("HTTP tool transport is not initialized".into())
            })?
        } else {
            &self.client
        };
        let mut request = client
            .post(endpoint)
            .header("Idempotency-Key", invocation.operation_id.to_string())
            .json(invocation.arguments);
        if let Some(token) = &binding.bearer {
            request = request.bearer_auth(token);
        }
        let uncertain = || {
            ExecutionError::Unknown(
                "HTTP tool outcome is uncertain; inspect destination before retry".into(),
            )
        };
        let response = request.send().map_err(|_| uncertain())?;
        // Even a server error may follow a committed write. The generic protocol
        // cannot assert that any unsuccessful response proves absence of effects.
        if !response.status().is_success() {
            return Err(uncertain());
        }
        let mut bytes = Vec::new();
        response
            .take(1_048_577)
            .read_to_end(&mut bytes)
            .map_err(|_| uncertain())?;
        if bytes.len() > 1_048_576 {
            return Err(uncertain());
        }
        serde_json::from_slice(&bytes).map_err(|_| uncertain())
    }
}
