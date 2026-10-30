//! Strict LF-framed streaming with identity validation, backpressure and an
//! independent total/record/frame ceiling. No provider text enters diagnostics.
use super::*;
use std::sync::mpsc::{SyncSender, TrySendError};

const RECORD_LIMIT: usize = MAX_BYTES;
const FRAME_LIMIT: usize = 8192;
pub(super) struct Identity {
    pub id: String,
    pub version: u32,
}
pub(super) enum Notice {
    Text(String),
    Complete(std::result::Result<Vec<u8>, ExecutionError>),
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    Start {
        protocol_version: u32,
        runtime_id: String,
        runtime_version: u32,
    },
    TextDelta {
        text: String,
    },
    Response {
        result: BridgeResponse,
    },
}
pub(super) async fn send(
    sender: &SyncSender<Notice>,
    mut notice: Notice,
) -> std::result::Result<(), ExecutionError> {
    loop {
        match sender.try_send(notice) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Disconnected(_)) => {
                return Err(unknown("runtime bridge observer closed"))
            }
            Err(TrySendError::Full(value)) => {
                notice = value;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}
struct Decoder<'a> {
    identity: &'a Identity,
    sender: &'a SyncSender<Notice>,
    bytes: Vec<u8>,
    total: usize,
    frames: usize,
    limit: usize,
    started: bool,
    result: Option<BridgeResponse>,
}
impl<'a> Decoder<'a> {
    fn new(identity: &'a Identity, sender: &'a SyncSender<Notice>, limit: usize) -> Self {
        Self {
            identity,
            sender,
            bytes: Vec::new(),
            total: 0,
            frames: 0,
            limit,
            started: false,
            result: None,
        }
    }
    async fn frame(&mut self, line: &[u8]) -> std::result::Result<(), ExecutionError> {
        self.frames += 1;
        if self.frames > FRAME_LIMIT || self.result.is_some() || line.len() > RECORD_LIMIT {
            return Err(failed(
                "runtime bridge stream frame limit or order violated",
            ));
        }
        let frame: Frame = serde_json::from_slice(line)
            .map_err(|_| failed("invalid runtime bridge stream frame"))?;
        match frame {
            Frame::Start {
                protocol_version,
                runtime_id,
                runtime_version,
            } if !self.started
                && protocol_version == PROTOCOL_VERSION
                && runtime_id == self.identity.id
                && runtime_version == self.identity.version =>
            {
                self.started = true;
            }
            Frame::TextDelta { text } if self.started => {
                send(self.sender, Notice::Text(text)).await?
            }
            Frame::Response { result }
                if self.started
                    && result.protocol_version == PROTOCOL_VERSION
                    && result.runtime_id == self.identity.id
                    && result.runtime_version == self.identity.version =>
            {
                self.result = Some(result);
            }
            _ => return Err(failed("runtime bridge stream identity or order mismatch")),
        }
        Ok(())
    }
    async fn push(&mut self, chunk: &[u8]) -> std::result::Result<(), ExecutionError> {
        if chunk.len() > self.limit - self.total {
            return Err(failed("runtime bridge stream exceeds byte limit"));
        }
        self.total += chunk.len();
        // Split only ASCII LF. Unicode paragraph separators remain JSON data.
        let mut rest = chunk;
        while let Some(end) = rest.iter().position(|b| *b == b'\n') {
            if end > RECORD_LIMIT - self.bytes.len() {
                return Err(failed("runtime bridge stream record exceeds limit"));
            }
            self.bytes.extend_from_slice(&rest[..end]);
            let mut line = std::mem::take(&mut self.bytes);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.frame(&line).await?;
            line.clear();
            self.bytes = line;
            rest = &rest[end + 1..];
        }
        if rest.len() > RECORD_LIMIT - self.bytes.len() {
            return Err(failed("runtime bridge stream record exceeds limit"));
        }
        self.bytes.extend_from_slice(rest);
        Ok(())
    }
    async fn finish(mut self) -> std::result::Result<Vec<u8>, ExecutionError> {
        if !self.bytes.is_empty() {
            let line = std::mem::take(&mut self.bytes);
            self.frame(&line).await?;
        }
        let response = self.result.ok_or_else(|| {
            unknown("runtime bridge stream ended without a result; no automatic retry")
        })?;
        serde_json::to_vec(&response).map_err(|_| failed("cannot encode runtime bridge result"))
    }
}
pub(super) async fn exchange(
    transport: &Transport,
    body: Vec<u8>,
    limit: usize,
    identity: &Identity,
    sender: &SyncSender<Notice>,
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
                .header("Accept", "application/x-ndjson")
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
            if response.content_length().is_some_and(|n| n > limit as u64) {
                return Err(failed("runtime bridge stream exceeds byte limit"));
            }
            let mut decoder = Decoder::new(identity, sender, limit);
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| unknown("runtime bridge stream interrupted; no automatic retry"))?
            {
                decoder.push(&chunk).await?;
            }
            decoder.finish().await
        }
        Transport::Stdio { .. } => process(transport, body, limit, identity, sender).await,
    }
}
async fn process(
    transport: &Transport,
    body: Vec<u8>,
    limit: usize,
    identity: &Identity,
    sender: &SyncSender<Notice>,
) -> std::result::Result<Vec<u8>, ExecutionError> {
    let Transport::Stdio {
        executable,
        args,
        cwd,
        env,
    } = transport
    else {
        return Err(failed("invalid stdio transport"));
    };
    let mut child = super::process::ContainedChild::spawn(executable, args, cwd, env)?;
    let mut input = child.stdin();
    let mut output = child.stdout();
    let stderr = child.stderr();
    let write = async move {
        input
            .write_all(&body)
            .await
            .map_err(|_| unknown("runtime bridge input interrupted"))?;
        input
            .shutdown()
            .await
            .map_err(|_| unknown("runtime bridge input interrupted"))
    };
    let read = async {
        let mut decoder = Decoder::new(identity, sender, limit);
        let mut chunk = [0; 4096];
        loop {
            let count = output
                .read(&mut chunk)
                .await
                .map_err(|_| unknown("runtime bridge stream interrupted"))?;
            if count == 0 {
                break;
            }
            decoder.push(&chunk[..count]).await?;
        }
        decoder.finish().await
    };
    let (_, result, _) = tokio::try_join!(write, read, bounded_read(stderr, STDERR_BYTES))?;
    if !child.wait().await?.success() {
        return Err(unknown(
            "runtime bridge exited unsuccessfully; no automatic retry",
        ));
    }
    Ok(result)
}
