//! Owned process-tree lifetime. Windows assigns the suspended child to a job
//! before it can run; failure never falls back to an uncontained child.
use super::{failed, unknown, ExecutionError};
use std::{
    path::Path,
    process::{ExitStatus, Stdio},
};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};

pub(super) struct ContainedChild {
    #[cfg(unix)]
    child: tokio::process::Child,
    #[cfg(unix)]
    group: rustix::process::Pid,
    #[cfg(windows)]
    child: Box<dyn process_wrap::tokio::ChildWrapper>,
}
impl ContainedChild {
    pub fn spawn(
        executable: &Path,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
    ) -> Result<Self, ExecutionError> {
        let mut command = tokio::process::Command::new(executable);
        command
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            command.process_group(0);
            let child = command
                .spawn()
                .map_err(|_| failed("cannot start contained runtime process"))?;
            let group = rustix::process::Pid::from_raw(child.id().expect("spawned child") as i32)
                .expect("positive process ID");
            Ok(Self { child, group })
        }
        #[cfg(windows)]
        {
            use process_wrap::tokio::{CommandWrap, CreationFlags, JobObject, KillOnDrop};
            use windows::Win32::System::Threading::CREATE_NO_WINDOW;
            let mut wrapped = CommandWrap::from(command);
            wrapped
                .wrap(CreationFlags(CREATE_NO_WINDOW))
                .wrap(JobObject)
                .wrap(KillOnDrop);
            let child = wrapped
                .spawn()
                .map_err(|_| failed("cannot assign runtime process to a Windows job"))?;
            Ok(Self { child })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = command;
            Err(failed("process containment unavailable"))
        }
    }
    pub fn stdin(&mut self) -> ChildStdin {
        #[cfg(unix)]
        {
            self.child.stdin.take().expect("piped stdin")
        }
        #[cfg(windows)]
        {
            self.child.stdin().take().expect("piped stdin")
        }
        #[cfg(not(any(unix, windows)))]
        {
            unreachable!()
        }
    }
    pub fn stdout(&mut self) -> ChildStdout {
        #[cfg(unix)]
        {
            self.child.stdout.take().expect("piped stdout")
        }
        #[cfg(windows)]
        {
            self.child.stdout().take().expect("piped stdout")
        }
        #[cfg(not(any(unix, windows)))]
        {
            unreachable!()
        }
    }
    pub fn stderr(&mut self) -> ChildStderr {
        #[cfg(unix)]
        {
            self.child.stderr.take().expect("piped stderr")
        }
        #[cfg(windows)]
        {
            self.child.stderr().take().expect("piped stderr")
        }
        #[cfg(not(any(unix, windows)))]
        {
            unreachable!()
        }
    }
    pub async fn wait(&mut self) -> Result<ExitStatus, ExecutionError> {
        #[cfg(any(unix, windows))]
        {
            self.child
                .wait()
                .await
                .map_err(|_| unknown("runtime process exit unavailable"))
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(failed("process containment unavailable"))
        }
    }
}
impl Drop for ContainedChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = rustix::process::kill_process_group(self.group, rustix::process::Signal::KILL);
        }
        #[cfg(windows)]
        {
            let _ = self.child.start_kill();
        }
    }
}
