use super::{memory::Data, MemoryStore};
use crate::{Error, Result};
use postgres::{Client, NoTls};
use std::sync::{Arc, Mutex};

pub(super) struct Postgres {
    client: Client,
    namespace: String,
}
fn database_error(_: postgres::Error) -> Error {
    // Connection strings and server error detail must not enter public events.
    Error::Conflict(
        "PostgreSQL operation failed; transaction outcome may require inspection".into(),
    )
}
impl MemoryStore {
    /// Connect using a caller-configured PostgreSQL client (including its TLS policy).
    pub fn from_postgres(mut client: Client, namespace: &str) -> Result<Self> {
        if namespace.trim().is_empty() || namespace.len() > 128 {
            return Err(Error::Invalid(
                "store namespace must contain 1 to 128 bytes".into(),
            ));
        }
        client.batch_execute("CREATE TABLE IF NOT EXISTS hudson_state (namespace TEXT PRIMARY KEY, schema_version INTEGER NOT NULL, state JSONB NOT NULL)").map_err(database_error)?;
        let initial = serde_json::to_value(Data::default())?;
        client.execute("INSERT INTO hudson_state(namespace,schema_version,state) VALUES($1,1,$2) ON CONFLICT(namespace) DO NOTHING", &[&namespace,&initial]).map_err(database_error)?;
        let mut database = Postgres {
            client,
            namespace: namespace.into(),
        };
        database.read(|_| Ok(()))?;
        Ok(Self {
            postgres: Some(Arc::new(Mutex::new(database))),
            ..Self::default()
        })
    }
    /// Convenience for local Unix-socket PostgreSQL. Remote/TLS connections use
    /// `from_postgres` with an explicitly configured client.
    pub fn postgres_local(socket_directory: &str, database: &str, namespace: &str) -> Result<Self> {
        if !std::path::Path::new(socket_directory).is_absolute() {
            return Err(Error::Invalid(
                "PostgreSQL socket directory must be absolute".into(),
            ));
        }
        let client = postgres::Config::new()
            .host(socket_directory)
            .dbname(database)
            .connect_timeout(std::time::Duration::from_secs(10))
            .connect(NoTls)
            .map_err(database_error)?;
        Self::from_postgres(client, namespace)
    }
}
fn decode(row: postgres::Row) -> Result<Data> {
    let version: i32 = row.try_get(0).map_err(database_error)?;
    if !matches!(version, 1..=3) {
        return Err(Error::Unsupported("unknown PostgreSQL state schema".into()));
    }
    let value: serde_json::Value = row.try_get(1).map_err(database_error)?;
    let mut data: Data = serde_json::from_value(value)?;
    for run in data.runs.values_mut() {
        run.restore_verified_null_result();
    }
    Ok(data)
}
impl Postgres {
    pub fn read<T>(&mut self, f: impl FnOnce(&Data) -> Result<T>) -> Result<T> {
        let row = self
            .client
            .query_one(
                "SELECT schema_version,state FROM hudson_state WHERE namespace=$1",
                &[&self.namespace],
            )
            .map_err(database_error)?;
        f(&decode(row)?)
    }
    pub fn transact<T>(&mut self, f: impl FnOnce(&mut Data) -> Result<T>) -> Result<T> {
        let mut transaction = self.client.transaction().map_err(database_error)?;
        transaction
            .batch_execute("SET LOCAL lock_timeout = '10s'; SET LOCAL statement_timeout = '30s'")
            .map_err(database_error)?;
        let row = transaction
            .query_one(
                "SELECT schema_version,state FROM hudson_state WHERE namespace=$1 FOR UPDATE",
                &[&self.namespace],
            )
            .map_err(database_error)?;
        let previous_version: i32 = row.try_get(0).map_err(database_error)?;
        let mut data = decode(row)?;
        let output = f(&mut data)?;
        // Legacy writers accept only version 1 and would discard unknown inbox
        // fields. Promotion is atomic with the first accepted message and sticky.
        let required_version = if !data.experiments.is_empty() {
            3
        } else if !data.inboxes.is_empty() {
            2
        } else {
            1
        };
        let version = previous_version.max(required_version);
        let value = serde_json::to_value(data)?;
        transaction
            .execute(
                "UPDATE hudson_state SET state=$2,schema_version=$3 WHERE namespace=$1",
                &[&self.namespace, &value, &version],
            )
            .map_err(database_error)?;
        transaction.commit().map_err(database_error)?;
        Ok(output)
    }
}
