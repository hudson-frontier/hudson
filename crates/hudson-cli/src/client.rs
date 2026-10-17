use crate::commands::{Args, Command, Start};
use serde_json::{json, Value};
use std::io::Read;

fn segment(id: &str) -> Result<&str, Box<dyn std::error::Error>> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err("invalid ID".into());
    }
    Ok(id)
}
pub fn execute(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(name) = &args.api_token_env {
        let url = reqwest::Url::parse(&args.url)?;
        let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        if (url.scheme() != "https" && !(url.scheme() == "http" && local))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "authenticated API URL requires HTTPS or loopback HTTP without URL credentials"
                    .into(),
            );
        }
        let token = std::env::var(name).map_err(|_| "API token environment variable is missing")?;
        if token.is_empty() {
            return Err("API token environment variable is empty".into());
        }
        let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))?;
        value.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    let client = reqwest::blocking::Client::builder()
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let base = args.url.trim_end_matches('/');
    let response = match args.command.unwrap_or(Command::Session(Default::default())) {
        Command::Session(session) => return crate::session::run(&client, base, session),
        Command::Follow {
            run_id,
            after,
            page_size,
            poll_ms,
            max_retries,
        } => {
            return crate::follow::run(
                &client,
                base,
                &segment(&run_id)?.to_ascii_lowercase(),
                crate::follow::Options {
                    after,
                    page_size,
                    poll_ms,
                    max_retries,
                },
            )
            .map_err(Into::into);
        }
        Command::Health => client.get(format!("{base}/health")).send()?,
        Command::Capabilities => client.get(format!("{base}/capabilities")).send()?,
        Command::PublishTool { file } => client
            .post(format!("{base}/tools"))
            .json(&read_json(&file)?)
            .send()?,
        Command::PublishAgent { file } => client
            .post(format!("{base}/agents"))
            .json(&read_json(&file)?)
            .send()?,
        Command::Tool { name, version } => client
            .get(format!(
                "{base}/tools/{}/versions/{version}",
                definition_name(&name)?
            ))
            .send()?,
        Command::Agent { name, version } => client
            .get(format!(
                "{base}/agents/{}/versions/{version}",
                definition_name(&name)?
            ))
            .send()?,
        Command::Start(start) => {
            let body = submission(start)?;
            client.post(format!("{base}/runs")).json(&body).send()?
        }
        Command::Reply {
            run_id,
            question_id,
            text,
            request_key,
        } => {
            let body = json!({"input":text,"request_key":request_key,"question_id":question_id});
            if serde_json::to_vec(&body)?.len() > 64 * 1024 {
                return Err("reply exceeds 64 KiB".into());
            }
            client
                .post(format!("{base}/runs/{}/input", segment(&run_id)?))
                .json(&body)
                .send()?
        }
        Command::Resume { run_id } => client
            .post(format!("{base}/runs/{}/resume", segment(&run_id)?))
            .send()?,
        Command::Get { run_id } => client
            .get(format!("{base}/runs/{}", segment(&run_id)?))
            .send()?,
        Command::Children { run_id } => client
            .get(format!("{base}/runs/{}/children", segment(&run_id)?))
            .send()?,
        Command::Operation { operation_id } => client
            .get(format!("{base}/operations/{}", segment(&operation_id)?))
            .send()?,
        Command::Events { run_id, after } => client
            .get(format!("{base}/runs/{}/events", segment(&run_id)?))
            .query(&[("after", after)])
            .send()?,
        Command::Approve { operation_id, deny } => client
            .post(format!(
                "{base}/operations/{}/approval",
                segment(&operation_id)?
            ))
            .json(&json!({"approved":!deny}))
            .send()?,
        Command::Cancel { run_id } => client
            .post(format!("{base}/runs/{}/cancel", segment(&run_id)?))
            .send()?,
    };
    let status = response.status();
    let body: Value = response.json()?;
    println!("{}", serde_json::to_string_pretty(&body)?);
    if !status.is_success() {
        return Err(format!("Hudson returned {status}").into());
    }
    Ok(())
}

fn submission(start: Start) -> Result<Value, Box<dyn std::error::Error>> {
    const MAX_BYTES: u64 = 64 * 1024;
    let input = if let Some(task) = start.task {
        json!(task)
    } else if let Some(path) = start.input_file {
        read_json(&path)?
    } else if let Some(order) = start.order {
        json!({"order_id":order,"action":if start.refund {"refund"} else {"lookup"}})
    } else {
        return Err("provide --task, --input-file, or fixture --order".into());
    };
    let mut body = json!({"input":input,"request_key":start.request_key});
    if let Some(agent) = start.agent {
        body["agent_ref"] = json!({"id":definition_name(&agent)?,"version":start.agent_version.ok_or("agent version required")?});
    }
    if serde_json::to_vec(&body)?.len() as u64 > MAX_BYTES {
        return Err("submission exceeds the API's 64 KiB body limit".into());
    }
    Ok(body)
}

pub(crate) fn definition_name(name: &str) -> Result<&str, Box<dyn std::error::Error>> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err("invalid definition name".into());
    }
    Ok(name)
}
fn read_json(path: &std::path::Path) -> Result<Value, Box<dyn std::error::Error>> {
    let reader: Box<dyn Read> = if path == std::path::Path::new("-") {
        Box::new(std::io::stdin())
    } else {
        Box::new(std::fs::File::open(path)?)
    };
    let mut bytes = Vec::new();
    reader.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        return Err("JSON input exceeds 64 KiB".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}
