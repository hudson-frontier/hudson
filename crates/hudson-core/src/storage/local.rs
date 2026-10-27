//! Single-process local snapshots. The lifetime lock excludes another host;
//! intent transitions are synced before any runtime effect can be dispatched.
use super::{memory::Data, MemoryStore};
use crate::{Error, Result};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const MAX_BYTES: u64 = 64 * 1024 * 1024;
pub(super) struct LocalFile {
    _lock: File,
    path: PathBuf,
    data: Data,
}

fn io_error(_: std::io::Error) -> Error {
    Error::Conflict("local state IO failed; inspect saved state before retrying".into())
}
fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
impl MemoryStore {
    /// A private host-owned directory must already exist. One host may open the
    /// path at a time. Failed commits do not update the in-memory state.
    pub fn local_file(path: &Path) -> Result<Self> {
        let lock = private_options()
            .open(path.with_extension("lock"))
            .map_err(io_error)?;
        lock.try_lock()
            .map_err(|_| Error::Conflict("local session is already open".into()))?;
        let data = match File::open(path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(io_error)?;
                if bytes.len() as u64 > MAX_BYTES {
                    return Err(Error::Invalid("local state exceeds 64 MiB".into()));
                }
                let envelope: serde_json::Value = serde_json::from_slice(&bytes)?;
                if envelope["schema_version"] != 1 {
                    return Err(Error::Unsupported("unknown local state schema".into()));
                }
                let mut data: Data = serde_json::from_value(envelope["state"].clone())?;
                for run in data.runs.values_mut() {
                    run.restore_verified_null_result();
                }
                data
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Data::default(),
            Err(error) => return Err(io_error(error)),
        };
        Ok(Self {
            local: Some(Arc::new(Mutex::new(LocalFile {
                _lock: lock,
                path: path.into(),
                data,
            }))),
            ..Self::default()
        })
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
        f(&self.data)
    }
    pub fn transact<T>(&mut self, f: impl FnOnce(&mut Data) -> Result<T>) -> Result<T> {
        let mut next = self.data.clone();
        let result = f(&mut next)?;
        let bytes = serde_json::to_vec(&serde_json::json!({"schema_version":1,"state":next}))?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(Error::Invalid(
                "local state exceeds 64 MiB; start a new session".into(),
            ));
        }
        let temporary = self.path.with_extension("tmp");
        let mut file = private_options()
            .truncate(true)
            .open(&temporary)
            .map_err(io_error)?;
        file.write_all(&bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        fs::rename(&temporary, &self.path).map_err(io_error)?;
        // Sync the rename as well as its contents on Unix.
        #[cfg(unix)]
        File::open(
            self.path
                .parent()
                .ok_or_else(|| Error::Invalid("state directory required".into()))?,
        )
        .map_err(io_error)?
        .sync_all()
        .map_err(io_error)?;
        self.data = next;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_state_has_exclusive_ownership_durable_commit_and_rollback() {
        let directory = std::env::temp_dir().join(format!("hudson-store-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
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
