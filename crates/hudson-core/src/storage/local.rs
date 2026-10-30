//! Single-process local snapshots. The lifetime lock excludes another host;
//! intent transitions are synced before any runtime effect can be dispatched.
use super::{memory::Data, MemoryStore};
use crate::{Error, Result};
#[cfg(unix)]
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{Read, Write},
    sync::{Arc, Mutex},
};
use std::{fs::File, path::Path};

#[cfg(unix)]
const MAX_BYTES: u64 = 64 * 1024 * 1024;
#[derive(serde::Deserialize)]
#[cfg(unix)]
struct Saved {
    schema_version: u32,
    state: Data,
}
#[derive(serde::Serialize)]
#[cfg(unix)]
struct Snapshot<'a> {
    schema_version: u32,
    state: &'a Data,
}
#[derive(Default)]
#[cfg(unix)]
struct BoundedSnapshot {
    bytes: Vec<u8>,
    exceeded: bool,
}
#[cfg(unix)]
impl Write for BoundedSnapshot {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > MAX_BYTES - self.bytes.len() as u64 {
            self.exceeded = true;
            return Err(std::io::Error::other("local snapshot exceeds byte limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(super) struct LocalFile {
    _lock: File,
    #[cfg(unix)]
    directory: File,
    #[cfg(unix)]
    name: OsString,
    data: Data,
    unavailable: bool,
}

fn io_error(_: std::io::Error) -> Error {
    Error::Conflict("local state IO failed; inspect saved state before retrying".into())
}
#[cfg(unix)]
fn private_metadata(metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if !metadata.is_file() {
        return Err(Error::Invalid("local state must use regular files".into()));
    }
    if metadata.permissions().mode() & 0o077 != 0
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.nlink() != 1
    {
        return Err(Error::Invalid(
            "local state files require private ownership and one link".into(),
        ));
    }
    Ok(())
}
#[cfg(unix)]
struct Temporary<'a> {
    directory: &'a File,
    name: &'a OsStr,
    persisted: bool,
}
#[cfg(unix)]
impl Drop for Temporary<'_> {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = rustix::fs::unlinkat(self.directory, self.name, rustix::fs::AtFlags::empty());
        }
    }
}
impl MemoryStore {
    /// Unix snapshot storage; other platforms return `Error::Unsupported`.
    /// A private host-owned directory must already exist. One host may open the
    /// path at a time. Failed commits do not update the in-memory state.
    #[cfg(unix)]
    pub fn local_file(path: &Path) -> Result<Self> {
        use rustix::fs::{openat, Mode, OFlags, CWD};
        let name = path
            .file_name()
            .ok_or_else(|| Error::Invalid("state filename required".into()))?
            .to_owned();
        let directory = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| Error::Invalid("private state directory required".into()))?;
        let directory = File::from(
            openat(
                CWD,
                directory,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| io_error(error.into()))?,
        );
        Self::local_file_at(&directory, &name)
    }
    /// Share an already pinned private directory with the host's metadata IO.
    #[cfg(unix)]
    pub fn local_file_in(directory: &File, name: &str) -> Result<Self> {
        Self::local_file_at(directory, OsStr::new(name))
    }
    #[cfg(unix)]
    fn local_file_at(directory: &File, name: &OsStr) -> Result<Self> {
        use rustix::fs::{openat, Mode, OFlags};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(part)) if part == name)
            || components.next().is_some()
            || name.as_encoded_bytes().contains(&0)
        {
            return Err(Error::Invalid(
                "state filename requires one normal component".into(),
            ));
        }
        let name = name.to_owned();
        let lock_name = Path::new(&name).with_extension("lock");
        if lock_name.as_os_str() == name.as_os_str() {
            return Err(Error::Invalid(
                "state filename collides with its lock".into(),
            ));
        }
        let directory = directory.try_clone().map_err(io_error)?;
        let metadata = directory.metadata().map_err(io_error)?;
        if !metadata.is_dir()
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(Error::Invalid(
                "local state directory must be private".into(),
            ));
        }
        let flags = OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let lock = File::from(
            openat(
                &directory,
                lock_name.as_os_str(),
                OFlags::WRONLY | OFlags::CREATE | flags,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|error| io_error(error.into()))?,
        );
        private_metadata(&lock.metadata().map_err(io_error)?)?;
        lock.try_lock()
            .map_err(|_| Error::Conflict("local session is already open".into()))?;
        let data = match openat(
            &directory,
            name.as_os_str(),
            OFlags::RDONLY | flags,
            Mode::empty(),
        )
        .map(File::from)
        {
            Ok(file) => {
                private_metadata(&file.metadata().map_err(io_error)?)?;
                let mut bytes = Vec::new();
                file.take(MAX_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(io_error)?;
                if bytes.len() as u64 > MAX_BYTES {
                    return Err(Error::Invalid("local state exceeds 64 MiB".into()));
                }
                let envelope: Saved = serde_json::from_slice(&bytes)?;
                if !matches!(envelope.schema_version, 1 | 2) {
                    return Err(Error::Unsupported("unknown local state schema".into()));
                }
                let mut data = envelope.state;
                for run in data.runs.values_mut() {
                    run.restore_verified_null_result();
                }
                data
            }
            Err(rustix::io::Errno::NOENT) => Data::default(),
            Err(error) => return Err(io_error(error.into())),
        };
        Ok(Self {
            local: Some(Arc::new(Mutex::new(LocalFile {
                _lock: lock,
                directory,
                name,
                data,
                unavailable: false,
            }))),
            ..Self::default()
        })
    }
    /// Local snapshots require Unix private-file and ownership guarantees.
    #[cfg(not(unix))]
    pub fn local_file(_path: &Path) -> Result<Self> {
        Err(Error::Unsupported(
            "local snapshots require Unix; use WSL or PostgreSQL".into(),
        ))
    }
    /// Local snapshots require Unix private-file and ownership guarantees.
    #[cfg(not(unix))]
    pub fn local_file_in(_directory: &File, _name: &str) -> Result<Self> {
        Err(Error::Unsupported(
            "local snapshots require Unix; use WSL or PostgreSQL".into(),
        ))
    }
    /// Host-owned checkpoint access for reconstructing a local conversation.
    pub fn conversation_history(
        &self,
        actor: &crate::models::Actor,
        id: uuid::Uuid,
    ) -> Result<hudson_harness::context::History> {
        self.read(|data| {
            Ok(hudson_harness::agent_loop::history(
                &data.run(actor, id)?.state,
            )?)
        })
    }
}
impl LocalFile {
    pub fn read<T>(&self, f: impl FnOnce(&Data) -> Result<T>) -> Result<T> {
        if self.unavailable {
            return Err(io_error(std::io::ErrorKind::Other.into()));
        }
        f(&self.data)
    }
    #[cfg(unix)]
    pub fn transact<T>(&mut self, f: impl FnOnce(&mut Data) -> Result<T>) -> Result<T> {
        use rustix::fs::{openat, renameat, Mode, OFlags};
        if self.unavailable {
            return Err(io_error(std::io::ErrorKind::Other.into()));
        }
        let mut next = self.data.clone();
        let result = f(&mut next)?;
        let mut snapshot = BoundedSnapshot::default();
        serde_json::to_writer(
            &mut snapshot,
            &Snapshot {
                schema_version: if !next.spend_policies.is_empty()
                    || !next.spend_entries.is_empty()
                    || !next.spend_accounts.is_empty()
                {
                    2
                } else {
                    1
                },
                state: &next,
            },
        )
        .map_err(|error| {
            if snapshot.exceeded {
                Error::Invalid("local state exceeds 64 MiB; start a new session".into())
            } else {
                Error::from(error)
            }
        })?;
        let name = OsString::from(format!(".hudson-state-{}.tmp", uuid::Uuid::new_v4()));
        let mut file = File::from(
            openat(
                &self.directory,
                name.as_os_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|error| io_error(error.into()))?,
        );
        let mut temporary = Temporary {
            directory: &self.directory,
            name: name.as_os_str(),
            persisted: false,
        };
        file.write_all(&snapshot.bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        if let Err(error) = renameat(
            &self.directory,
            name.as_os_str(),
            &self.directory,
            self.name.as_os_str(),
        ) {
            // POSIX permits an uncertain replacement when rename returns EIO.
            if error == rustix::io::Errno::IO {
                self.unavailable = true;
            }
            return Err(io_error(error.into()));
        }
        temporary.persisted = true;
        // A directory-sync failure leaves the durability of the rename uncertain.
        // Block further access until the host reopens and inspects the saved state.
        self.unavailable = true;
        // Sync the rename as well as its contents on Unix.
        self.directory.sync_all().map_err(io_error)?;
        self.data = next;
        self.unavailable = false;
        Ok(result)
    }
    #[cfg(not(unix))]
    pub fn transact<T>(&mut self, _f: impl FnOnce(&mut Data) -> Result<T>) -> Result<T> {
        Err(Error::Unsupported(
            "local snapshots require Unix; use WSL or PostgreSQL".into(),
        ))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn directory() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_paths_are_rejected_without_opening_special_files_or_touching_targets() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let directory = directory();
        let path = directory.path().join("state.json");
        let clear_lock = || {
            let lock = path.with_extension("lock");
            if lock.exists() {
                fs::remove_file(lock).unwrap();
            }
        };
        let victim = directory.path().join("victim");
        fs::write(&victim, b"do not overwrite").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();
        for link in [path.clone(), path.with_extension("lock")] {
            symlink(&victim, &link).unwrap();
            assert!(MemoryStore::local_file(&path).is_err());
            assert_eq!(fs::read(&victim).unwrap(), b"do not overwrite");
            fs::remove_file(link).unwrap();
            clear_lock();
        }
        fs::hard_link(&victim, &path).unwrap();
        assert!(MemoryStore::local_file(&path).is_err());
        fs::remove_file(&path).unwrap();
        clear_lock();
        fs::write(&path, b"not private").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(MemoryStore::local_file(&path).is_err());
        fs::remove_file(&path).unwrap();
        clear_lock();
        for fifo in [path.clone(), path.with_extension("lock")] {
            rustix::fs::mkfifoat(
                rustix::fs::CWD,
                &fifo,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            )
            .unwrap();
            assert!(MemoryStore::local_file(&path).is_err());
            fs::remove_file(fifo).unwrap();
            clear_lock();
        }
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(MemoryStore::local_file(&path).is_err());
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let link = directory.path().join("directory-link");
        symlink(directory.path(), &link).unwrap();
        assert!(MemoryStore::local_file(&link.join("state.json")).is_err());
    }

    #[test]
    fn failed_atomic_replace_rolls_back_and_removes_its_temporary_file() {
        let directory = directory();
        let path = directory.path().join("state.json");
        let store = MemoryStore::local_file(&path).unwrap();
        store.transact(|_| Ok(())).unwrap();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(store
            .transact(|data| {
                data.model_bindings.insert(
                    (
                        "workspace".into(),
                        crate::models::VersionRef {
                            id: "agent".into(),
                            version: 1,
                        },
                    ),
                    "not committed".into(),
                );
                Ok(())
            })
            .is_err());
        assert!(store
            .read(|data| Ok(data.model_bindings.is_empty()))
            .unwrap());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn pinned_directory_keeps_state_and_lock_together_after_parent_replacement() {
        use std::os::unix::fs::symlink;
        let original = directory();
        let victim = directory();
        let session = original.path().join("session");
        fs::create_dir(&session).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&session, fs::Permissions::from_mode(0o700)).unwrap();
        let capability = File::open(&session).unwrap();
        let moved = original.path().join("moved-session");
        fs::rename(&session, &moved).unwrap();
        symlink(victim.path(), &session).unwrap();
        let store = MemoryStore::local_file_in(&capability, "state.json").unwrap();
        store.transact(|_| Ok(())).unwrap();
        assert!(moved.join("state.json").is_file());
        assert!(moved.join("state.lock").is_file());
        assert_eq!(fs::read_dir(victim.path()).unwrap().count(), 0);
        assert!(MemoryStore::local_file(&session.join("state.json")).is_err());
        drop(store);
        MemoryStore::local_file(&moved.join("state.json")).unwrap();
    }

    #[test]
    fn directory_capability_rejects_paths_and_lock_filename_collisions() {
        let directory = directory();
        let capability = File::open(directory.path()).unwrap();
        for name in [
            "../escape",
            "nested/state.json",
            "/tmp/state.json",
            "",
            "state.lock",
            "./state.json",
            "state.json/",
            "state\0.json",
        ] {
            assert!(
                MemoryStore::local_file_in(&capability, name).is_err(),
                "accepted {name:?}"
            );
        }
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn oversized_snapshot_rolls_back_before_replacing_saved_state() {
        let directory = directory();
        let path = directory.path().join("state.json");
        let store = MemoryStore::local_file(&path).unwrap();
        store.transact(|_| Ok(())).unwrap();
        let original = fs::read(&path).unwrap();
        assert!(matches!(
            store.transact(|data| {
                data.model_bindings.insert(
                    (
                        "workspace".into(),
                        crate::models::VersionRef {
                            id: "agent".into(),
                            version: 1,
                        },
                    ),
                    "x".repeat(MAX_BYTES as usize),
                );
                Ok(())
            }),
            Err(Error::Invalid(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(store
            .read(|data| Ok(data.model_bindings.is_empty()))
            .unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_directory_sync_failure_blocks_stale_state_until_reopen() {
        use rustix::fs::{openat, Mode, OFlags, CWD};
        let directory = directory();
        let path = directory.path().join("state.json");
        let key = (
            "workspace".into(),
            crate::models::VersionRef {
                id: "agent".into(),
                version: 1,
            },
        );
        let original = MemoryStore::local_file(&path).unwrap();
        original
            .transact(|data| {
                data.model_bindings.insert(key.clone(), "original".into());
                Ok(())
            })
            .unwrap();
        let before = fs::read(&path).unwrap();
        drop(original);
        // O_PATH supports relative opens and replacement but cannot be fsynced.
        let capability = File::from(
            openat(
                CWD,
                directory.path(),
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .unwrap(),
        );
        assert!(capability.sync_all().is_err());
        let uncertain = MemoryStore::local_file_in(&capability, "state.json").unwrap();
        assert!(uncertain
            .transact(|data| {
                data.model_bindings.insert(key.clone(), "replaced".into());
                Ok(())
            })
            .is_err());
        assert_ne!(
            before,
            fs::read(&path).unwrap(),
            "failure must occur after replacement"
        );
        assert!(uncertain.read(|_| Ok(())).is_err());
        let mut callback_ran = false;
        assert!(uncertain
            .transact(|_| {
                callback_ran = true;
                Ok(())
            })
            .is_err());
        assert!(!callback_ran);
        drop(uncertain);
        let reopened = MemoryStore::local_file(&path).unwrap();
        assert_eq!(
            reopened
                .read(|data| Ok(data.model_bindings[&key].clone()))
                .unwrap(),
            "replaced"
        );
    }
    #[test]
    fn local_state_has_exclusive_ownership_durable_commit_and_rollback() {
        let directory = std::env::temp_dir().join(format!("hudson-store-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = directory.join("state.json");
        let store = MemoryStore::local_file(&path).unwrap();
        assert!(MemoryStore::local_file(&path).is_err());
        store
            .transact(|data| {
                data.model_bindings.insert(
                    (
                        "workspace".into(),
                        crate::models::VersionRef {
                            id: "agent".into(),
                            version: 1,
                        },
                    ),
                    "saved".into(),
                );
                Ok(())
            })
            .unwrap();
        let before = fs::read(&path).unwrap();
        assert!(store
            .transact(|data| {
                data.model_bindings.clear();
                Err::<(), _>(Error::Denied)
            })
            .is_err());
        assert_eq!(before, fs::read(&path).unwrap());
        drop(store);
        let reopened = MemoryStore::local_file(&path).unwrap();
        assert_eq!(
            reopened.read(|data| Ok(data.model_bindings.len())).unwrap(),
            1
        );
        drop(reopened);
        fs::write(&path, b"{\"schema_version\":99,\"state\":{}}").unwrap();
        assert!(MemoryStore::local_file(&path).is_err());
        fs::remove_dir_all(directory).unwrap();
    }
}

#[cfg(all(test, not(unix)))]
#[test]
fn local_snapshots_fail_closed_without_unix_filesystem_guarantees() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.json");
    assert!(matches!(
        MemoryStore::local_file(&path),
        Err(Error::Unsupported(_))
    ));
    assert!(!path.exists());
}
