//! A state directory owns a stable writer/placement identity, independent of computer names.
//! Bootstrap old installs from local launch receipts, never peer presence or host claims.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _};

use crate::config::Config;

const FILE: &str = "node-identity.json";

#[derive(Debug)]
pub struct StateLock {
    _file: File,
    state_dir: PathBuf,
}

impl StateLock {
    /// Called only after the join handshake (or its checkpoint resume) has succeeded.
    /// An explicit admission establishes a new writer, unlike a computer-name change.
    pub fn record_fleet_join(&self, joined: &crate::fleet::join::Joined) -> Result<()> {
        self.record_admission(&joined.fleet_id, &joined.name)
    }

    pub fn record_fleet_found(&self, founded: &crate::fleet::join::Founded) -> Result<()> {
        self.record_admission(&founded.fleet_id, &founded.node)
    }

    fn record_admission(&self, fleet_id: &str, node: &str) -> Result<()> {
        let membership = crate::config::FleetFile::load(&self.state_dir)?
            .context("successful fleet admission has no saved membership")?;
        anyhow::ensure!(
            membership.node.as_deref() == Some(node) && membership.fleet_id == fleet_id,
            "saved membership does not match the successful fleet admission"
        );
        self.write_pin(node, true)
    }

    fn write_pin(&self, node: &str, replace: bool) -> Result<()> {
        anyhow::ensure!(
            !node.trim().is_empty() && node.trim() == node,
            "invalid node identity"
        );
        let path = self.state_dir.join(FILE);
        let mut pending = tempfile::NamedTempFile::new_in(&self.state_dir)?;
        serde_json::to_writer(&mut pending, node)?;
        pending.write_all(b"\n")?;
        pending.as_file().sync_all()?;
        if replace {
            pending.persist(&path)
        } else {
            pending.persist_noclobber(&path)
        }
        .with_context(|| format!("pin node identity at {}", path.display()))?;
        File::open(&self.state_dir)?.sync_all()?;
        Ok(())
    }
}

/// Hold for the daemon lifetime, including before store migration and runtime initialization.
/// The state lock also fences a second daemon using different socket paths on the same state.
pub fn lock(state_dir: &Path) -> Result<StateLock> {
    fs::create_dir_all(state_dir)?;
    let path = state_dir.join("node-identity.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)?;
    // SAFETY: the open file owns this descriptor until StateLock drops.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "another daemon owns the state directory {}",
                state_dir.display()
            )
        });
    }
    Ok(StateLock {
        _file: lock,
        state_dir: state_dir.into(),
    })
}

/// Service stop can return before the old daemon has closed its file descriptors.
pub async fn lock_after_stop(state_dir: &Path) -> Result<StateLock> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match lock(state_dir) {
            Err(error)
                if tokio::time::Instant::now() < deadline
                    && error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            result => return result,
        }
    }
}

pub fn acquire(config: &mut Config) -> Result<StateLock> {
    let lock = lock(&config.state_dir)?;
    resolve(config)?;
    config.validate()?;
    let path = config.state_dir.join(FILE);
    if !path.exists() {
        lock.write_pin(&config.node, false)?;
    }
    Ok(lock)
}

/// Read only: used by the worker and service generator as well as daemon startup.
pub fn resolve(config: &mut Config) -> Result<()> {
    let path = config.state_dir.join(FILE);
    let pinned = match fs::read(&path) {
        Ok(bytes) => Some(
            serde_json::from_slice::<String>(&bytes)
                .with_context(|| format!("read stable node identity {}", path.display()))?,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("read stable node identity"),
    };
    let node = match pinned {
        Some(node) => {
            if let Some(writer) = restored_writer(&config.state_dir.join("claims.sqlite3"))? {
                anyhow::ensure!(
                    writer == node,
                    "restored database requires writer `{writer}`, but this directory is pinned to `{node}`; restore into a fresh state directory without an existing node pin, then configure the restored writer"
                );
            }
            Some(node)
        }
        None => previous_local_node(&config.state_dir.join("claims.sqlite3"), &config.node)?,
    };
    if let Some(node) = node {
        anyhow::ensure!(
            !node.trim().is_empty() && node.trim() == node,
            "invalid stable node identity in {}",
            config.state_dir.display()
        );
        if let Some(member) = config.fleet.as_ref().and_then(|file| file.node.as_deref()) {
            anyhow::ensure!(
                member == node,
                "state belongs to node `{node}`, but fleet membership pins `{member}`; restore the matching configuration and keys before starting, without editing membership or deleting state"
            );
        }
        if config.node != node {
            eprintln!(
                "st: retaining stable node `{node}` for {}; configured name `{}` does not rename its writer or live seats",
                config.state_dir.display(),
                config.node
            );
            config.node = node;
        }
    }
    Ok(())
}

fn has_columns(connection: &Connection, table: &str, required: &[&str]) -> Result<bool> {
    // Table names are internal constants, never configuration values.
    let mut query = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = query
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(required
        .iter()
        .all(|name| columns.iter().any(|column| column == name)))
}

fn restored_writer_from(connection: &Connection) -> Result<Option<String>> {
    if !has_columns(connection, "meta", &["key", "value"])? {
        return Ok(None);
    }
    Ok(connection
        .query_row(
            "SELECT value FROM meta WHERE key='backup_restore_writer'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

fn restored_writer(database: &Path) -> Result<Option<String>> {
    if !database.exists() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    restored_writer_from(&connection)
}

fn previous_local_node(database: &Path, requested: &str) -> Result<Option<String>> {
    if !database.exists() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("inspect previous local daemon identity without migrating the database")?;
    if let Some(writer) = restored_writer_from(&connection)? {
        anyhow::ensure!(
            requested == writer,
            "restored database requires fresh writer `{writer}`; configure node to that identity before starting"
        );
        return Ok(Some(writer));
    }
    if !has_columns(
        &connection,
        "local_observations",
        &["id", "subject", "kind", "body"],
    )? || !has_columns(
        &connection,
        "claims",
        &["id", "subject", "kind", "body", "origin", "store_index"],
    )? {
        return Ok(None);
    }
    // Only runtime.action receipts are local. daemon.started and runtime.observed replicate.
    // A receipt binds this directory to the immutable declaration it actually launched;
    // a matching observation must still report that source's runtime running. A peer's
    // replicated declaration/observation without a local launch receipt cannot select a node.
    let mut query = connection.prepare("SELECT DISTINCT json_extract(d.body,'$.member.host')
        FROM local_observations AS o
        JOIN claims AS d ON d.id=json_extract(o.body,'$.fields.desired_token')
        JOIN claims AS r ON r.subject=o.subject AND r.kind='runtime.observed'
            AND r.origin=json_extract(d.body,'$.member.host')
        WHERE o.kind='runtime.action.succeeded'
            AND json_extract(o.body,'$.fields.action')='start'
            AND json_extract(d.body,'$.member.host') IS NOT NULL
            AND json_extract(r.body,'$.fields.runtime_id')=json_extract(o.body,'$.fields.runtime_id')
            AND json_extract(r.body,'$.fields.status')='running'
            AND (json_extract(o.body,'$.fields.incarnation_id') IS NULL
                OR json_extract(o.body,'$.fields.incarnation_id')=json_extract(r.body,'$.fields.incarnation_id'))
            AND o.id=(SELECT MAX(id) FROM local_observations WHERE subject=o.subject
                AND kind='runtime.action.succeeded' AND json_extract(body,'$.fields.action')='start')
            AND r.store_index=(SELECT MAX(store_index) FROM claims WHERE subject=r.subject
                AND kind='runtime.observed' AND origin=r.origin)")?;
    let hosts = query
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    anyhow::ensure!(
        hosts.len() <= 1,
        "ambiguous local launch placements {}; inspect node configuration and live runtimes before recovery",
        hosts.join(", ")
    );
    Ok(hosts.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_identity_survives_name_changes_and_reversal_and_fences_duplicates() {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config {
            node: "orchid".into(),
            state_dir: root.path().into(),
            ..Config::default()
        };
        let lock = acquire(&mut config).unwrap();
        let mut duplicate = config.clone();
        assert!(
            acquire(&mut duplicate)
                .unwrap_err()
                .to_string()
                .contains("another daemon")
        );
        drop(lock);
        for requested in ["orchid-laptop", "orchid"] {
            config.node = requested.into();
            let lock = acquire(&mut config).unwrap();
            assert_eq!(config.node, "orchid");
            drop(lock);
        }
    }

    #[test]
    fn upgrade_uses_local_launch_receipts_and_refuses_conflicting_membership() {
        let root = tempfile::tempdir().unwrap();
        let connection = Connection::open(root.path().join("claims.sqlite3")).unwrap();
        connection.execute_batch(r#"CREATE TABLE local_observations(id INTEGER PRIMARY KEY, subject TEXT, kind TEXT, body TEXT);
            CREATE TABLE claims(store_index INTEGER PRIMARY KEY,id TEXT,subject TEXT,kind TEXT,origin TEXT,body TEXT);
            INSERT INTO claims VALUES
            (1,'desired-one','agent/example/one','intent.desired','orchid','{"member":{"host":"orchid"}}'),
            (2,'observed-one','agent/example/one','runtime.observed','orchid','{"fields":{"runtime_id":"one","incarnation_id":"native-one","status":"running"}}'),
            (3,'remote','agent/example/remote','runtime.observed','fern','{"fields":{"runtime_id":"remote","status":"running","host":"fern"}}'),
            (4,'remote-startup','daemon/fern','daemon.started','fern','{"fields":{"status":"running"}}');
            INSERT INTO local_observations VALUES(1,'agent/example/one','runtime.action.succeeded','{"fields":{"action":"start","desired_token":"desired-one","runtime_id":"one","incarnation_id":"native-one"}}');"#).unwrap();
        let mut config = Config {
            node: "orchid-laptop".into(),
            state_dir: root.path().into(),
            ..Config::default()
        };
        resolve(&mut config).unwrap();
        assert_eq!(config.node, "orchid");
        assert!(
            !root.path().join(FILE).exists(),
            "worker inspection cannot pin state"
        );
        let fleet = crate::config::FleetFile {
            node: Some("orchid-laptop".into()),
            ..crate::config::FleetFile::default()
        };
        config.fleet = Some(fleet);
        assert!(
            resolve(&mut config)
                .unwrap_err()
                .to_string()
                .contains("fleet membership pins")
        );
        connection
            .execute("DELETE FROM local_observations", [])
            .unwrap();
        config.fleet = None;
        config.node = "orchid-laptop".into();
        resolve(&mut config).unwrap();
        assert_eq!(
            config.node, "orchid-laptop",
            "replicated history alone cannot rename this node"
        );
    }

    #[test]
    fn restore_refusal_does_not_pin_an_incorrect_writer() {
        let root = tempfile::tempdir().unwrap();
        let connection = Connection::open(root.path().join("claims.sqlite3")).unwrap();
        connection.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT); INSERT INTO meta VALUES('backup_restore_writer','restored-orchid');").unwrap();
        let mut config = Config {
            node: "orchid".into(),
            state_dir: root.path().into(),
            ..Config::default()
        };
        assert!(
            acquire(&mut config)
                .unwrap_err()
                .to_string()
                .contains("fresh writer")
        );
        assert!(!root.path().join(FILE).exists());
        config.node = "restored-orchid".into();
        let _lock = acquire(&mut config).unwrap();
        assert_eq!(config.node, "restored-orchid");
    }

    #[test]
    fn successful_explicit_join_records_its_admitted_identity_under_the_state_lock() {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config {
            node: "orchid".into(),
            state_dir: root.path().into(),
            ..Config::default()
        };
        let guard = acquire(&mut config).unwrap();
        let joined = crate::fleet::join::Joined {
            fleet_id: "11111111-1111-4111-8111-111111111111".into(),
            name: "orchid-returned".into(),
            sponsor: "fern".into(),
            writer_floor: None,
            migrate: false,
            resumed: false,
        };
        assert!(guard.record_fleet_join(&joined).is_err());
        let membership = crate::config::FleetFile {
            fleet_id: joined.fleet_id.clone(),
            node: Some(joined.name.clone()),
            mode: crate::config::FleetMode::DialOut,
            ..crate::config::FleetFile::default()
        };
        membership.save(root.path()).unwrap();
        let mut conflicting = config.clone();
        conflicting.fleet = Some(membership.clone());
        assert!(
            resolve(&mut conflicting).is_err(),
            "editing fleet.toml alone cannot rebind the pin"
        );
        guard.record_fleet_join(&joined).unwrap();
        assert!(
            lock(root.path()).is_err(),
            "the join must fence concurrent daemon startup"
        );
        drop(guard);
        let mut updated = config.clone();
        updated.fleet = Some(membership);
        let guard = acquire(&mut updated).unwrap();
        assert_eq!(updated.node, "orchid-returned");
        let mismatched = crate::fleet::join::Joined {
            name: "other-member".into(),
            ..joined
        };
        assert!(guard.record_fleet_join(&mismatched).is_err());
        resolve(&mut updated).unwrap();
        assert_eq!(updated.node, "orchid-returned");
    }

    #[test]
    fn older_schema_without_receipt_columns_keeps_configured_node_for_normal_migration() {
        let root = tempfile::tempdir().unwrap();
        let connection = Connection::open(root.path().join("claims.sqlite3")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE claims(id TEXT, kind TEXT, subject TEXT, body TEXT);
            CREATE TABLE local_observations(subject TEXT, kind TEXT, body TEXT);
            INSERT INTO claims VALUES('legacy','runtime.observed','agent/example/one','{}');",
            )
            .unwrap();
        let mut config = Config {
            node: "orchid".into(),
            state_dir: root.path().into(),
            ..Config::default()
        };
        resolve(&mut config).unwrap();
        assert_eq!(config.node, "orchid");
        assert!(!root.path().join(FILE).exists());
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM claims", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            1
        );
        let origin = connection
            .prepare("SELECT origin FROM claims")
            .err()
            .unwrap();
        assert!(
            origin.to_string().contains("no such column"),
            "inspection must not migrate the schema"
        );
    }

    #[test]
    fn ambiguous_local_receipts_refuse_without_pinning_or_rewriting_history() {
        let root = tempfile::tempdir().unwrap();
        let connection = Connection::open(root.path().join("claims.sqlite3")).unwrap();
        connection.execute_batch(r#"CREATE TABLE local_observations(id INTEGER PRIMARY KEY, subject TEXT, kind TEXT, body TEXT);
            CREATE TABLE claims(store_index INTEGER PRIMARY KEY,id TEXT,subject TEXT,kind TEXT,origin TEXT,body TEXT);
            INSERT INTO claims VALUES
            (1,'desired-one','agent/example/one','intent.desired','orchid','{"member":{"host":"orchid"}}'),
            (2,'observed-one','agent/example/one','runtime.observed','orchid','{"fields":{"runtime_id":"one","incarnation_id":"native-one","status":"running"}}'),
            (3,'desired-two','agent/example/two','intent.desired','orchid-old','{"member":{"host":"orchid-old"}}'),
            (4,'observed-two','agent/example/two','runtime.observed','orchid-old','{"fields":{"runtime_id":"two","incarnation_id":"native-two","status":"running"}}');
            INSERT INTO local_observations VALUES
            (1,'agent/example/one','runtime.action.succeeded','{"fields":{"action":"start","desired_token":"desired-one","runtime_id":"one","incarnation_id":"native-one"}}'),
            (2,'agent/example/two','runtime.action.succeeded','{"fields":{"action":"start","desired_token":"desired-two","runtime_id":"two","incarnation_id":"native-two"}}');"#).unwrap();
        let mut config = Config {
            node: "orchid".into(),
            state_dir: root.path().into(),
            ..Config::default()
        };
        assert!(
            acquire(&mut config)
                .unwrap_err()
                .to_string()
                .contains("ambiguous local launch")
        );
        assert!(!root.path().join(FILE).exists());
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM claims", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            4
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM local_observations", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn restored_database_under_an_existing_pin_requires_a_fresh_directory() {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config {
            node: "orchid".into(),
            state_dir: root.path().into(),
            ..Config::default()
        };
        drop(acquire(&mut config).unwrap());
        let connection = Connection::open(root.path().join("claims.sqlite3")).unwrap();
        connection.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT); INSERT INTO meta VALUES('backup_restore_writer','restored-orchid');").unwrap();
        config.node = "restored-orchid".into();
        assert!(
            acquire(&mut config)
                .unwrap_err()
                .to_string()
                .contains("fresh state directory")
        );
        assert_eq!(
            serde_json::from_slice::<String>(&fs::read(root.path().join(FILE)).unwrap()).unwrap(),
            "orchid"
        );
    }

    #[tokio::test]
    async fn admission_waits_for_a_stopping_daemon_to_release_its_lock() {
        let root = tempfile::tempdir().unwrap();
        let guard = lock(root.path()).unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            drop(guard);
        });
        let admitted = lock_after_stop(root.path()).await.unwrap();
        release.await.unwrap();
        assert!(lock(root.path()).is_err());
        drop(admitted);
    }
}
