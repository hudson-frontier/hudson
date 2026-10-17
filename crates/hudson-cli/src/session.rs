//! Interactive operator session. Project display grants no tool or filesystem authority.
use crate::{commands::Session, follow};
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::io::{self, BufRead, IsTerminal, Read, Write};

type Error = Box<dyn std::error::Error>;

/// Escape terminal controls from server/model text, including ANSI and bidi controls.
fn safe(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if (c.is_control() && c != '\n' && c != '\t')
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                c.escape_unicode().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

fn id(text: &str) -> Result<String, Error> {
    Ok(uuid::Uuid::parse_str(text)
        .map_err(|_| "expected a run or operation UUID")?
        .to_string())
}

fn request(client: &Client, base: &str, path: &str, body: Option<Value>) -> Result<Value, Error> {
    let builder = if let Some(body) = body {
        if serde_json::to_vec(&body)?.len() > 64 * 1024 {
            return Err("input exceeds 64 KiB".into());
        }
        client.post(format!("{base}{path}")).json(&body)
    } else {
        client.get(format!("{base}{path}"))
    };
    // Mutations are sent exactly once. Ambiguous errors must be reconciled explicitly.
    let response = builder.send().map_err(|_| "API unavailable; mutation outcome may be unknown. Reuse the printed request key with `start` or `reply`; inspect existing runs before other retries")?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("Hudson returned {status}").into());
    }
    let mut bytes = Vec::new();
    response.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("response exceeds 8 MiB".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn display(record: Value) -> Result<(), String> {
    if record["type"] == "event" {
        let event = &record["event"];
        println!(
            "[{}] {} {}",
            record["after"],
            safe(event["event_type"].as_str().unwrap_or("event")),
            safe(&event["payload"].to_string())
        );
    } else {
        let run = &record["run"];
        println!(
            "Status: {}",
            safe(run["status"].as_str().unwrap_or("unknown"))
        );
        if !run["wait"].is_null() {
            println!("Waiting: {}", safe(&run["wait"].to_string()));
        }
        if !run["result"].is_null() {
            println!("Result: {}", safe(&run["result"].to_string()));
        }
        if let Some(reason) = run["reason"].as_str() {
            println!("Reason: {}", safe(reason));
        }
    }
    io::stdout()
        .flush()
        .map_err(|_| "cannot write terminal output".into())
}

const HELP: &str = "Type a task to create a new run. Each task is independent.\n/open UUID | /follow | /status | /reply QUESTION_ID answer\n/approve OPERATION_UUID | /deny OPERATION_UUID | /resume | /cancel | /help | /quit\nCtrl-C exits the client; it does not cancel the run. Use /cancel explicitly.";

fn current(run: &Option<String>) -> Result<&str, Error> {
    run.as_deref()
        .ok_or_else(|| "no active run; submit a task or /open UUID".into())
}

fn action(
    client: &Client,
    base: &str,
    session: &Session,
    line: &str,
    active: &mut Option<String>,
) -> Result<bool, Error> {
    let (command, argument) = line.split_once(' ').unwrap_or((line, ""));
    match command {
        "/quit" => return Ok(false),
        "/help" => println!("{HELP}"),
        "/open" => {
            let run = id(argument.trim())?;
            request(client, base, &format!("/runs/{run}"), None)?;
            *active = Some(run);
        }
        "/status" => {
            let value = request(client, base, &format!("/runs/{}", current(active)?), None)?;
            display(json!({"type":"run", "run":value})).map_err(|e| -> Error { e.into() })?;
        }
        "/follow" => {}
        "/resume" | "/cancel" => {
            request(
                client,
                base,
                &format!("/runs/{}/{}", current(active)?, &command[1..]),
                Some(json!({})),
            )?;
        }
        "/approve" | "/deny" => {
            let operation = id(argument.trim())?;
            // Existing API checks ownership; a session cannot expand authority.
            request(
                client,
                base,
                &format!("/operations/{operation}/approval"),
                Some(json!({"approved":command == "/approve"})),
            )?;
        }
        "/reply" => {
            let (question, answer) = argument
                .split_once(' ')
                .ok_or("usage: /reply QUESTION_ID answer")?;
            if answer.trim().is_empty() {
                return Err("answer cannot be empty".into());
            }
            let question: u64 = question
                .parse()
                .map_err(|_| "question ID must be an unsigned integer")?;
            let run = current(active)?;
            let key = uuid::Uuid::new_v4().to_string();
            println!("Reply request key: {key} (retain for an ambiguous retry)");
            io::stdout().flush()?;
            request(
                client,
                base,
                &format!("/runs/{run}/input"),
                Some(json!({"input":answer,"question_id":question,"request_key":key})),
            )?;
        }
        _ if line.starts_with('/') => return Err("unknown command; use /help".into()),
        _ => {
            let key = uuid::Uuid::new_v4().to_string();
            let mut body = json!({"input":line,"request_key":key});
            if let Some(agent) = &session.agent {
                body["agent_ref"] = json!({"id":crate::client::definition_name(agent)?,"version":session.agent_version.ok_or("agent version required")?});
            }
            println!("Submission request key: {key} (retain for an ambiguous retry)");
            io::stdout().flush()?;
            let response = request(client, base, "/runs", Some(body))?;
            let run = id(response["run_id"]
                .as_str()
                .ok_or("submission response has no run ID")?)?;
            println!("Run: {run}");
            *active = Some(run);
        }
    }
    Ok(true)
}

pub fn run(client: &Client, base: &str, session: Session) -> Result<(), Error> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("interactive session requires a terminal; use start/follow for scripts".into());
    }
    if let Some(agent) = &session.agent {
        crate::client::definition_name(agent)?;
    }
    println!("Hudson • interactive local API session\nProject: {}\nCurrent directory is display context only; no files are uploaded and tools remain host-configured.", safe(&std::env::current_dir()?.display().to_string()));
    println!("Connect to an existing hudson-server; /help lists controls.\n{HELP}");
    let mut active = None;
    let mut cursor = 0;
    let input = io::stdin();
    let mut input = input.lock();
    loop {
        print!("hudson> ");
        io::stdout().flush()?;
        // Bound allocation even for paste/input containing no newline.
        let mut bytes = Vec::new();
        let count = input
            .by_ref()
            .take(64 * 1024 + 1)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            break;
        }
        if bytes.len() > 64 * 1024 {
            return Err("terminal input exceeds 64 KiB".into());
        }
        let line = String::from_utf8(bytes)?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let previous = active.clone();
        match action(client, base, &session, line, &mut active) {
            Ok(false) => break,
            Err(error) => {
                eprintln!("{}", safe(&error.to_string()));
                continue;
            }
            Ok(true) => {}
        }
        if active != previous {
            cursor = 0;
        }
        if line == "/status" || line == "/help" {
            continue;
        }
        if let Some(run) = &active {
            // Callback tracks emitted cursor even when a later read fails.
            let result = follow::observe(
                client,
                base,
                run,
                follow::Options {
                    after: cursor,
                    page_size: 100,
                    poll_ms: 500,
                    max_retries: 3,
                },
                true,
                |record| {
                    display(record.clone())?;
                    if let Some(after) = record["after"].as_u64() {
                        cursor = after;
                    }
                    Ok(())
                },
            );
            if let Err(reason) = result {
                eprintln!("{}; /follow retries observation", safe(&reason));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escapes_terminal_injection_and_bidi_but_retains_readable_text() {
        assert_eq!(safe("hello\n\tworld"), "hello\n\tworld");
        assert_eq!(
            safe("\x1b]52;c;secret\x07\r\u{202e}x"),
            "\\u{1b}]52;c;secret\\u{7}\\u{d}\\u{202e}x"
        );
    }
    #[test]
    fn command_entrypoints_preserve_subcommands_and_require_pinned_agent_version() {
        use crate::commands::{Args, Command};
        use clap::Parser;
        assert!(Args::try_parse_from(["hudson"]).unwrap().command.is_none());
        assert!(matches!(
            Args::try_parse_from(["hudson", "health"]).unwrap().command,
            Some(Command::Health)
        ));
        assert!(Args::try_parse_from(["hudson", "session", "--agent", "analyst"]).is_err());
        assert!(Args::try_parse_from([
            "hudson",
            "session",
            "--agent",
            "analyst",
            "--agent-version",
            "1"
        ])
        .is_ok());
    }
    #[test]
    fn validates_real_uuids_and_missing_session() {
        assert!(id("-").is_err());
        assert!(current(&None).is_err());
        assert_eq!(
            id("AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA").unwrap(),
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
        );
    }
}
