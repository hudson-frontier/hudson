use super::{Event, Sender};
use crate::commands::Permission;
use hudson_core::{
    adapters::tools::{ExecutionError, Invocation, ToolExecutor},
    models::*,
};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const FILE_LIMIT: usize = 64 * 1024;
const OUTPUT_LIMIT: usize = 24 * 1024;
fn failed(message: impl Into<String>) -> ExecutionError {
    ExecutionError::Failed(message.into())
}
fn io_failed(error: std::io::Error) -> ExecutionError {
    failed(format!("local tool IO: {}", error.kind()))
}
pub struct LocalTools {
    pub root: PathBuf,
    pub cancelled: Arc<AtomicBool>,
    pub events: Sender,
}
pub fn definitions(actor: &Actor, permission: Permission) -> Vec<Tool> {
    let string = || json!({"type":"string","minLength":1,"maxLength":65536});
    let specs = [
        ("list_directory", "List a directory in this workspace. Output is bounded and indicates truncation.", json!({"path":string()}), vec!["path"], Effect::Read),
        ("read_file", "Read a UTF-8 workspace file up to 64 KiB; returns its digest for safe replacement.", json!({"path":string()}), vec!["path"], Effect::Read),
        ("search", "Search workspace file contents with ripgrep. Ignores ignored files; no symlink following. Use list_directory to explore.", json!({"pattern":string()}), vec!["pattern"], Effect::Read),
        ("write_file", "Create a UTF-8 file or replace it. Replacing requires expected_digest from read_file; parent directory must exist.", json!({"path":string(),"content":{"type":"string","maxLength":65536},"expected_digest":string()}), vec!["path","content"], Effect::Write),
        ("edit_file", "Replace one unique exact old_text occurrence in a workspace UTF-8 file. Read the file first; unrelated content is preserved.", json!({"path":string(),"old_text":string(),"new_text":{"type":"string","maxLength":65536}}), vec!["path","old_text","new_text"], Effect::Write),
        ("shell", "Execute a POSIX shell command with workspace cwd and the user's OS permissions. No filesystem sandbox. Timeout 1..120 seconds; output is bounded. Background jobs are not retained. Nonzero exit is evidence of failure, not success.", json!({"command":string(),"timeout_seconds":{"type":"integer","minimum":1,"maximum":120}}), vec!["command"], Effect::Write),
    ];
    specs.into_iter().filter(|spec| permission != Permission::ReadOnly || spec.4 == Effect::Read).map(|(name, description, properties, required, effect)| Tool {
        id:name.into(), workspace_id:actor.workspace_id.clone(), version:1, schema_version:SCHEMA_VERSION, name:name.into(), description:description.into(),
        input_schema:json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}), output_schema:None,
        execution:Execution::Registered { key:name.into() }, credential_ref:None,
        policy_ref:if effect == Effect::Read {"local-read"} else {"local-write"}.into(), effect, created_at:0,
    }).collect()
}
impl LocalTools {
    fn path(&self, name: &str, create: bool) -> Result<PathBuf, ExecutionError> {
        let joined = self.root.join(name);
        let path = if create && !joined.exists() {
            joined
                .parent()
                .ok_or_else(|| failed("file parent required"))?
                .canonicalize()
                .map_err(io_failed)?
                .join(
                    joined
                        .file_name()
                        .ok_or_else(|| failed("file name required"))?,
                )
        } else {
            joined.canonicalize().map_err(io_failed)?
        };
        if !path.starts_with(&self.root) {
            return Err(failed("path is outside the session workspace"));
        }
        Ok(path)
    }
    fn read(&self, path: &Path) -> Result<String, ExecutionError> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(
                (rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::NOFOLLOW).bits() as i32,
            );
        }
        let file = options.open(path).map_err(io_failed)?;
        if !file.metadata().map_err(io_failed)?.is_file() {
            return Err(failed("path must be a regular file"));
        }
        let mut bytes = Vec::new();
        file.take(FILE_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(io_failed)?;
        if bytes.len() > FILE_LIMIT {
            return Err(failed(
                "file exceeds 64 KiB; use a bounded shell command with approval",
            ));
        }
        String::from_utf8(bytes).map_err(|_| failed("file is not UTF-8"))
    }
    fn write(
        &self,
        path: &Path,
        old: Option<&str>,
        content: &str,
    ) -> Result<Value, ExecutionError> {
        if content.len() > FILE_LIMIT {
            return Err(failed("resulting file exceeds 64 KiB"));
        }
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(failed("cancelled before file write"));
        }
        let temporary = path.with_file_name(format!(".hudson-{}.tmp", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(io_failed)?;
        let result = (|| {
            file.write_all(content.as_bytes()).map_err(io_failed)?;
            if let Ok(metadata) = fs::metadata(path) {
                file.set_permissions(metadata.permissions())
                    .map_err(io_failed)?;
            }
            file.sync_all().map_err(io_failed)?;
            if let Some(expected) = old {
                if self.read(path)? != expected {
                    return Err(failed("file changed during edit; read it again"));
                }
                fs::rename(&temporary, path).map_err(|_| {
                    ExecutionError::Unknown("file replacement outcome requires inspection".into())
                })?;
            } else {
                // Hard linking is an atomic create-if-absent; never clobber a file
                // concurrently created after the initial existence check.
                fs::hard_link(&temporary, path).map_err(io_failed)?;
            }
            Ok(())
        })();
        let _ = fs::remove_file(&temporary);
        result?;
        #[cfg(unix)]
        std::fs::File::open(
            path.parent()
                .ok_or_else(|| failed("file parent required"))?,
        )
        .and_then(|directory| directory.sync_all())
        .map_err(|_| {
            ExecutionError::Unknown(
                "file changed but directory durability is uncertain; inspect effects".into(),
            )
        })?;
        let before = old.unwrap_or("");
        let diff = format!(
            "{}\n{}\n{}",
            path.strip_prefix(&self.root).unwrap_or(path).display(),
            before
                .lines()
                .take(60)
                .map(|s| format!("- {s}"))
                .collect::<Vec<_>>()
                .join("\n"),
            content
                .lines()
                .take(60)
                .map(|s| format!("+ {s}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let _ = self.events.try_send(Event::Activity(diff));
        Ok(
            json!({"written":true,"bytes":content.len(),"digest":hudson_core::definitions::digest(&content).map_err(|_|failed("digest failed"))?}),
        )
    }
    fn execute_tool(&self, name: &str, args: &Value) -> Result<Value, ExecutionError> {
        let field = |key: &str| {
            args[key]
                .as_str()
                .ok_or_else(|| failed(format!("{key} must be text")))
        };
        match name {
            "list_directory" => {
                let path = self.path(field("path")?, false)?;
                let mut entries = Vec::new();
                let mut truncated = false;
                for (index, entry) in fs::read_dir(path).map_err(io_failed)?.enumerate() {
                    if index == 200 {
                        truncated = true;
                        break;
                    }
                    let entry = entry.map_err(io_failed)?;
                    entries.push(json!({"name":entry.file_name().to_string_lossy(),"directory":entry.file_type().map_err(io_failed)?.is_dir()}));
                }
                entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                Ok(json!({"entries":entries,"truncated":truncated}))
            }
            "read_file" => {
                let content = self.read(&self.path(field("path")?, false)?)?;
                Ok(
                    json!({"digest":hudson_core::definitions::digest(&content).map_err(|_|failed("digest failed"))?,"content":content}),
                )
            }
            "write_file" => {
                let path = self.path(field("path")?, true)?;
                let old = if path.exists() {
                    Some(self.read(&path)?)
                } else {
                    None
                };
                if let Some(old) = &old {
                    if args["expected_digest"].as_str()
                        != Some(
                            hudson_core::definitions::digest(old)
                                .map_err(|_| failed("digest failed"))?
                                .as_str(),
                        )
                    {
                        return Err(failed(
                            "expected_digest does not match; read the file again",
                        ));
                    }
                }
                self.write(&path, old.as_deref(), field("content")?)
            }
            "edit_file" => {
                let path = self.path(field("path")?, false)?;
                let old = self.read(&path)?;
                let needle = field("old_text")?;
                if needle.is_empty() || old.matches(needle).count() != 1 {
                    return Err(failed("old_text must match exactly once"));
                }
                let content = old.replacen(needle, field("new_text")?, 1);
                self.write(&path, Some(&old), &content)
            }
            "search" => {
                let mut command = Command::new("rg");
                command.args([
                    "--line-number",
                    "--no-heading",
                    "--color",
                    "never",
                    "--max-count",
                    "50",
                    "--",
                    field("pattern")?,
                    ".",
                ]);
                self.command(command, Duration::from_secs(15))
            }
            "shell" => {
                #[cfg(not(unix))]
                {
                    return Err(failed("local shell execution currently requires Unix"));
                }
                #[cfg(unix)]
                {
                    let mut command = Command::new("/bin/sh");
                    command.args(["-c", field("command")?]);
                    self.command(
                        command,
                        Duration::from_secs(
                            args["timeout_seconds"]
                                .as_u64()
                                .unwrap_or(120)
                                .clamp(1, 120),
                        ),
                    )
                }
            }
            _ => Err(failed("unknown local tool")),
        }
    }
    #[cfg(not(unix))]
    fn command(&self, _command: Command, _timeout: Duration) -> Result<Value, ExecutionError> {
        Err(failed("local process tools currently require Unix"))
    }
    #[cfg(unix)]
    fn command(&self, mut command: Command, timeout: Duration) -> Result<Value, ExecutionError> {
        command
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();
        for key in ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut process = RunningChild {
            child: command.spawn().map_err(io_failed)?,
            stopped: false,
        };
        let mut stdout = process
            .child
            .stdout
            .take()
            .ok_or_else(|| failed("stdout pipe unavailable"))?;
        let mut stderr = process
            .child
            .stderr
            .take()
            .ok_or_else(|| failed("stderr pipe unavailable"))?;
        for fd in [
            &stdout as &dyn std::os::fd::AsFd,
            &stderr as &dyn std::os::fd::AsFd,
        ] {
            let flags = rustix::fs::fcntl_getfl(fd).map_err(|_| {
                ExecutionError::Unknown("command pipe setup failed; inspect effects".into())
            })?;
            rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK).map_err(|_| {
                ExecutionError::Unknown("command pipe setup failed; inspect effects".into())
            })?;
        }
        let start = Instant::now();
        let mut output = Vec::new();
        let mut truncated = false;
        let mut interrupted = false;
        let mut drain = |pipe: &mut dyn Read, chunks: usize| -> Result<(), ExecutionError> {
            let mut bytes = [0; 2048];
            for _ in 0..chunks {
                match pipe.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => {
                        let remaining = OUTPUT_LIMIT.saturating_sub(output.len());
                        let retained = count.min(remaining);
                        output.extend_from_slice(&bytes[..retained]);
                        truncated |= count > remaining;
                        if retained > 0 {
                            let _ = self.events.try_send(Event::Activity(
                                String::from_utf8_lossy(&bytes[..retained]).into_owned(),
                            ));
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        return Err(ExecutionError::Unknown(
                            "command output observation failed; inspect effects".into(),
                        ))
                    }
                }
            }
            Ok(())
        };
        let status = loop {
            // Bounded reads prevent a noisy child from starving cancellation.
            drain(&mut stdout, 8)?;
            drain(&mut stderr, 8)?;
            if self.cancelled.load(Ordering::Relaxed) || start.elapsed() >= timeout {
                interrupted = true;
                process.stop();
                break process.child.wait().map_err(|_| {
                    ExecutionError::Unknown("command wait failed; inspect effects".into())
                })?;
            }
            match process.child.try_wait().map_err(|_| {
                ExecutionError::Unknown("command observation failed; inspect effects".into())
            })? {
                Some(status) => {
                    process.stop();
                    break status;
                }
                None => std::thread::sleep(Duration::from_millis(25)),
            }
        };
        drain(&mut stdout, 64)?;
        drain(&mut stderr, 64)?;
        if interrupted {
            return Err(ExecutionError::Unknown(
                "command cancelled or timed out; partial effects may exist and require inspection"
                    .into(),
            ));
        }
        Ok(
            json!({"exit_code":status.code(),"success":status.success(),"output":String::from_utf8_lossy(&output),"truncated":truncated}),
        )
    }
}
struct RunningChild {
    child: std::process::Child,
    stopped: bool,
}
impl RunningChild {
    fn stop(&mut self) {
        if !self.stopped {
            kill_group(&mut self.child);
            self.stopped = true;
        }
    }
}
impl Drop for RunningChild {
    fn drop(&mut self) {
        self.stop();
        let _ = self.child.wait();
    }
}
#[cfg(unix)]
fn kill_group(child: &mut std::process::Child) {
    if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill();
}
#[cfg(not(unix))]
fn kill_group(child: &mut std::process::Child) {
    let _ = child.kill();
}
impl ToolExecutor for LocalTools {
    fn execute(&mut self, invocation: Invocation<'_>) -> Result<Value, ExecutionError> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(failed("cancelled before tool dispatch"));
        }
        let Execution::Registered { key } = &invocation.tool.execution else {
            return Err(failed("local executor requires registered tools"));
        };
        let _ = self
            .events
            .try_send(Event::Activity(format!("{} {}", key, invocation.arguments)));
        self.execute_tool(key, invocation.arguments)
    }
}
