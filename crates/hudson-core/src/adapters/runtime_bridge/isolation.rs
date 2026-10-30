//! Explicit Linux namespace isolation for trusted, operator-installed peers.
use super::{invalid, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

fn tmp_bytes() -> u64 {
    32 * 1024 * 1024
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxIsolation {
    pub bubblewrap: PathBuf,
    /// Explicit host files/directories exposed at the same absolute path.
    pub read_only: Vec<PathBuf>,
    /// Host networking is an explicit grant, never a fallback.
    pub network: IsolationNetwork,
    #[serde(default = "tmp_bytes")]
    pub tmp_bytes: u64,
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IsolationNetwork {
    Deny,
    Host,
}

fn clean(path: &Path) -> bool {
    path.is_absolute()
        && path.as_os_str().len() <= 4096
        && path.to_str().is_some_and(|value| !value.contains('\0'))
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        && path != Path::new("/")
}

fn reserved(path: &Path) -> bool {
    ["/proc", "/dev", "/sys", "/tmp", "/run"]
        .iter()
        .any(|root| path.starts_with(root))
}

impl LinuxIsolation {
    /// Pure validation; filesystem checks occur when constructing the executor.
    pub(super) fn validate(&self, executable: &Path, cwd: &Path) -> Result<()> {
        if !cfg!(target_os = "linux") {
            return Err(crate::Error::Unsupported(
                "isolated stdio requires Linux".into(),
            ));
        }
        if !clean(&self.bubblewrap)
            || !clean(executable)
            || !clean(cwd)
            || reserved(cwd)
            || self.read_only.is_empty()
            || self.read_only.len() > 32
            || !(1024 * 1024..=256 * 1024 * 1024).contains(&self.tmp_bytes)
        {
            return Err(invalid("invalid Linux isolation paths or limits"));
        }
        for (i, path) in self.read_only.iter().enumerate() {
            if !clean(path)
                || reserved(path)
                || path.starts_with(cwd)
                || cwd.starts_with(path)
                || self.read_only[..i]
                    .iter()
                    .any(|other| path.starts_with(other) || other.starts_with(path))
            {
                return Err(invalid(
                    "isolation mounts must be explicit, disjoint and outside reserved paths",
                ));
            }
        }
        if !self
            .read_only
            .iter()
            .any(|path| executable.starts_with(path))
        {
            return Err(invalid("isolated executable must be exposed read-only"));
        }
        Ok(())
    }

    pub(super) fn prepare(
        &self,
        executable: &Path,
        args: &[String],
        cwd: &Path,
    ) -> Result<Vec<String>> {
        self.validate(executable, cwd)?;
        if !self.bubblewrap.is_file() || !cwd.is_dir() || !executable.is_file() {
            return Err(invalid("isolated runtime installation is unavailable"));
        }
        // A mutable/symlink workspace root could redirect the host write grant.
        if std::fs::canonicalize(cwd).ok().as_deref() != Some(cwd) {
            return Err(invalid("isolated working directory must be canonical"));
        }
        let mut out: Vec<String> = [
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--disable-userns",
            "--cap-drop",
            "ALL",
            "--die-with-parent",
            "--new-session",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        if self.network == IsolationNetwork::Deny {
            out.push("--unshare-net".into());
        }
        for path in &self.read_only {
            let source = std::fs::canonicalize(path)
                .map_err(|_| invalid("isolation read-only mount is unavailable"))?;
            if reserved(&source) || source.starts_with(cwd) || cwd.starts_with(&source) {
                return Err(invalid(
                    "isolation source cannot expose reserved paths or alias the writable workspace",
                ));
            }
            let metadata = std::fs::metadata(&source)
                .map_err(|_| invalid("isolation read-only mount is unavailable"))?;
            if !metadata.is_file() && !metadata.is_dir() {
                return Err(invalid(
                    "isolation mounts require regular files or directories",
                ));
            }
            out.extend(["--ro-bind".into(), utf8(&source)?, utf8(path)?]);
        }
        out.extend([
            "--bind".into(),
            utf8(cwd)?,
            utf8(cwd)?,
            "--proc".into(),
            "/proc".into(),
            "--dev".into(),
            "/dev".into(),
            "--size".into(),
            self.tmp_bytes.to_string(),
            "--tmpfs".into(),
            "/tmp".into(),
            "--chdir".into(),
            utf8(cwd)?,
            "--remount-ro".into(),
            "/".into(),
            "--".into(),
            utf8(executable)?,
        ]);
        out.extend_from_slice(args);
        Ok(out)
    }
}

fn utf8(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("isolation paths must be UTF-8"))
}
