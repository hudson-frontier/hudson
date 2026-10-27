use super::{Command, Error, Event, Host};
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event as TerminalEvent, KeyCode,
        KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use hudson_core::models::{OperationStatus, RunStatus, WaitReason};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};
use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

/// No model/tool/file content can write terminal control sequences or bidi controls.
fn safe(text: &str) -> String {
    text.chars().flat_map(|c| {
        if (c.is_control() && c != '\n' && c != '\t') || matches!(c,'\u{061c}'|'\u{200e}'|'\u{200f}'|'\u{2028}'..='\u{202e}'|'\u{2066}'..='\u{2069}') { c.escape_unicode().collect::<Vec<_>>() } else { vec![c] }
    }).collect()
}
struct Screen;
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
    }
}
const HELP: &str = "Enter: submit · /steer TEXT: redirect active work · /approve or /deny: pending write · /cancel or Ctrl-C: stop · /resume: saved task · /quit: exit · PageUp/Down: scroll";
fn push(log: &mut String, text: &str) {
    log.push_str(&safe(text));
    if log.len() > 256 * 1024 {
        let mut at = log.len() - 192 * 1024;
        while !log.is_char_boundary(at) {
            at += 1;
        }
        log.drain(..at);
    }
}
fn send(sender: &mpsc::SyncSender<Command>, command: Command, log: &mut String) -> bool {
    match sender.try_send(command) {
        Ok(()) => true,
        Err(_) => {
            push(log, "\nInput queue unavailable or full; try again.\n");
            false
        }
    }
}
pub(super) fn run(
    host: Host,
    receiver: mpsc::Receiver<Event>,
    cancelled: Arc<AtomicBool>,
) -> Result<(), Error> {
    let id = host.session.id;
    let workspace = host.session.workspace.clone();
    let header = safe(&format!(
        "Hudson · code · {} · {:?}\n{}\nSession {}",
        host.session.model,
        host.session.permission,
        host.session.workspace.display(),
        id
    ));
    let store = host.runtime.store.clone();
    let actor = host.actor.clone();
    let mut log = format!("{HELP}\n");
    if host.session.permission == crate::commands::Permission::FullAccess {
        log.push_str("Full access: commands use your OS permissions; no filesystem sandbox.\n");
    }
    if let Some(last) = host.active() {
        if let Ok(history) = store.conversation_history(&actor, last) {
            for message in history.messages {
                for content in message.content {
                    match content {
                        hudson_harness::Content::Json { value } => push(
                            &mut log,
                            &format!(
                                "\n{:?}: {}\n",
                                message.role,
                                value
                                    .as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| value.to_string())
                            ),
                        ),
                        hudson_harness::Content::Text { text } => {
                            push(&mut log, &format!("\n{:?}: {text}\n", message.role))
                        }
                        _ => {}
                    }
                }
            }
        }
        let view = store.inspect(&actor, last)?;
        push(
            &mut log,
            &format!(
                "\nRestored task {last}: {:?}. /resume continues a safe pending boundary.\n",
                view.status
            ),
        );
    }
    let (sender, commands) = mpsc::sync_channel(32);
    let worker = std::thread::spawn(move || host.work(commands));
    let mut sender = Some(sender);
    enable_raw_mode()?;
    let _screen = Screen;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut input = String::new();
    let mut status = "Ready".to_owned();
    let mut scroll = 0usize;
    let mut busy = false;
    let mut quitting = false;
    let mut streamed = String::new();
    let result = (|| -> Result<(), Error> {
        loop {
            while let Ok(event) = receiver.try_recv() {
                match event {
                    Event::Text(text) => {
                        if streamed.is_empty() {
                            push(&mut log, "\nHudson: ");
                        }
                        if streamed.len() + text.len() <= 128 * 1024 {
                            streamed.push_str(&text);
                        }
                        push(&mut log, &text);
                    }
                    Event::Activity(text) => {
                        streamed.clear();
                        push(&mut log, &format!("\n› {text}\n"));
                    }
                    Event::Error(error) => {
                        push(&mut log, &format!("\nError: {error}\n"));
                        status = "Needs attention".into();
                        busy = false;
                    }
                    Event::State(view) => {
                        busy = !view.status.terminal() && view.status != RunStatus::Waiting;
                        status = format!(
                            "{:?} · calls {} · tools {}",
                            view.status, view.usage.model_calls, view.usage.tool_calls
                        );
                        if let Some(WaitReason::Approval { .. }) = view.wait {
                            for operation in store.operations(&actor, view.id)? {
                                if operation.status == OperationStatus::WaitingApproval {
                                    let details =
                                        store.inspect_operation(&actor, operation.meta.id)?;
                                    push(
                                        &mut log,
                                        &format!(
                                            "\nApproval required: {} {}\nUse /approve or /deny.\n",
                                            details.tool_name.unwrap_or_default(),
                                            details.arguments.unwrap_or_default()
                                        ),
                                    );
                                    break;
                                }
                            }
                        }
                        if view.status.terminal() {
                            if let Some(value) = view.result {
                                if value.as_str() != Some(streamed.as_str()) {
                                    push(
                                        &mut log,
                                        &format!(
                                            "\n\nHudson: {}\n",
                                            value
                                                .as_str()
                                                .map(str::to_owned)
                                                .unwrap_or_else(|| value.to_string())
                                        ),
                                    );
                                } else {
                                    push(&mut log, "\n");
                                }
                            }
                            streamed.clear();
                            if let Some(reason) = view.reason {
                                push(&mut log, &format!("\n{reason}\n"));
                            }
                        }
                    }
                }
            }
            if quitting && worker.is_finished() {
                break;
            }
            if worker.is_finished() {
                return Err("local host exited unexpectedly".into());
            }
            terminal.draw(|frame| {
                let layout = Layout::vertical([
                    Constraint::Length(4),
                    Constraint::Min(3),
                    Constraint::Length(3),
                    Constraint::Length(2),
                ])
                .split(frame.area());
                frame.render_widget(
                    Paragraph::new(header.as_str()).style(Style::default().fg(Color::Cyan)),
                    layout[0],
                );
                let width = layout[1].width.saturating_sub(2).max(1) as usize;
                let wrapped: Vec<String> = log
                    .lines()
                    .flat_map(|line| {
                        let chars: Vec<char> = line.chars().collect();
                        if chars.is_empty() {
                            vec![String::new()]
                        } else {
                            chars
                                .chunks(width)
                                .map(|chunk| chunk.iter().collect())
                                .collect()
                        }
                    })
                    .collect();
                let height = layout[1].height.saturating_sub(2) as usize;
                let end = wrapped.len().saturating_sub(scroll);
                let start = end.saturating_sub(height);
                frame.render_widget(
                    Paragraph::new(wrapped[start..end].join("\n"))
                        .block(Block::default().borders(Borders::ALL).title("Conversation")),
                    layout[1],
                );
                frame.render_widget(
                    Paragraph::new(safe(&input))
                        .wrap(Wrap { trim: false })
                        .block(Block::default().borders(Borders::ALL).title(if busy {
                            "Working · type /steer or /cancel"
                        } else {
                            "Task"
                        })),
                    layout[2],
                );
                frame.render_widget(
                    Paragraph::new(format!("{} · Ctrl-C cancel · /quit exit", safe(&status))),
                    layout[3],
                );
            })?;
            if !event::poll(Duration::from_millis(40))? {
                continue;
            }
            match event::read()? {
                TerminalEvent::Paste(text) if !quitting => {
                    if input.len() + text.len() <= 32 * 1024 {
                        input.push_str(&safe(&text));
                    }
                }
                TerminalEvent::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        cancelled.store(true, Ordering::Relaxed);
                        if let Some(sender) = &sender {
                            send(sender, Command::Stop, &mut log);
                        }
                        status = "Cancelling; model IO may wait for its bounded transport timeout"
                            .into();
                    }
                    KeyCode::PageUp => scroll = scroll.saturating_add(10).min(log.lines().count()),
                    KeyCode::PageDown => scroll = scroll.saturating_sub(10),
                    KeyCode::Backspace if !quitting => {
                        input.pop();
                    }
                    KeyCode::Char(c)
                        if !quitting
                            && !key.modifiers.contains(KeyModifiers::CONTROL)
                            && input.len() + c.len_utf8() <= 32 * 1024 =>
                    {
                        input.push(c)
                    }
                    KeyCode::Enter if !quitting => {
                        let text = std::mem::take(&mut input);
                        let text = text.trim();
                        if text.is_empty() {
                            continue;
                        }
                        scroll = 0;
                        push(&mut log, &format!("\nYou: {text}\n"));
                        if text == "/quit" {
                            cancelled.store(true, Ordering::Relaxed);
                            sender.take();
                            quitting = true;
                            status = "Stopping local host…".into();
                            continue;
                        }
                        if text == "/help" {
                            push(&mut log, &format!("\n{HELP}\n"));
                            continue;
                        }
                        let command = match text {
                            "/cancel" => {
                                cancelled.store(true, Ordering::Relaxed);
                                Command::Stop
                            }
                            "/approve" => Command::Approve(true),
                            "/deny" => Command::Approve(false),
                            "/resume" => Command::Resume,
                            _ if text.starts_with("/steer ") => {
                                Command::Steer(text[7..].to_owned())
                            }
                            _ if text.starts_with('/') => {
                                push(&mut log, "\nUnknown command; /help lists controls.\n");
                                continue;
                            }
                            _ if busy => {
                                push(&mut log, "\nUse /steer TEXT while a task is active.\n");
                                continue;
                            }
                            _ => Command::Prompt(text.into()),
                        };
                        if let Some(sender) = &sender {
                            if send(sender, command, &mut log) {
                                busy = true;
                                status = "Working…".into();
                            }
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        Ok(())
    })();
    cancelled.store(true, Ordering::Relaxed);
    drop(sender);
    // Drain bounded worker output while waiting, avoiding a shutdown deadlock.
    while !worker.is_finished() {
        let _ = receiver.recv_timeout(Duration::from_millis(50));
    }
    worker.join().map_err(|_| "local host worker panicked")?;
    result?;
    drop(_screen);
    println!(
        "Resume: hudson --workspace '{}' --resume {id}",
        workspace.display().to_string().replace('\'', "'\\''")
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    #[test]
    fn terminal_content_cannot_inject_controls() {
        let value = super::safe("hello\n\x1b]52;c;secret\x07\u{202e}abc");
        assert!(!value.contains('\x1b'));
        assert!(!value.contains('\u{202e}'));
        assert!(value.contains("hello\n"));
    }
}
