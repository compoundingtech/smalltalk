//! The gateway's own store, owned by the sekrets user: profiles, the values put into them,
//! grants, registered daemon keys, locks, and the call log. Nothing here is readable by a seat.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

use super::policy::Policy;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS profiles (
  id TEXT PRIMARY KEY,
  owner TEXT NOT NULL,
  description TEXT,
  policy TEXT NOT NULL,
  is_default INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS profile_env (
  profile TEXT NOT NULL,
  name TEXT NOT NULL,
  value TEXT NOT NULL,
  set_at INTEGER NOT NULL,
  PRIMARY KEY (profile, name)
);
CREATE TABLE IF NOT EXISTS grants (
  id TEXT PRIMARY KEY,
  profile TEXT NOT NULL,
  grantee TEXT NOT NULL,
  policy TEXT NOT NULL,
  until_ms INTEGER,
  granted_by TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  revoked_at INTEGER,
  revoked_by TEXT
);
CREATE TABLE IF NOT EXISTS daemons (
  uid INTEGER PRIMARY KEY,
  person TEXT NOT NULL,
  node TEXT NOT NULL,
  key TEXT NOT NULL,
  registered_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS locks (
  scope TEXT PRIMARY KEY,
  locked_by TEXT NOT NULL,
  reason TEXT,
  at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS log (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  at INTEGER NOT NULL,
  caller_person TEXT,
  owner_person TEXT,
  event TEXT NOT NULL,
  actor TEXT,
  profile TEXT,
  detail TEXT NOT NULL
);
";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub owner: String,
    #[serde(default)]
    pub description: Option<String>,
    pub policy: Policy,
    pub default: bool,
    /// The names of the values put into it; never the values.
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Grant {
    pub id: String,
    pub profile: String,
    pub grantee: String,
    pub policy: Policy,
    #[serde(default)]
    pub until_unix_ms: Option<i64>,
    pub granted_by: String,
    pub created_at_unix_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub seq: i64,
    pub at_unix_ms: i64,
    /// The person the caller is or works for.
    #[serde(default)]
    pub caller_person: Option<String>,
    /// The person who owns the profile the entry is about.
    #[serde(default)]
    pub owner_person: Option<String>,
    pub event: String,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    pub detail: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lock {
    pub scope: String,
    pub locked_by: String,
    #[serde(default)]
    pub reason: Option<String>,
    pub at_unix_ms: i64,
}

pub struct GatewayStore {
    root: PathBuf,
    db: Connection,
}

/// `person/ada` → `ada`.
pub fn person_name(person: &str) -> Option<&str> {
    person
        .strip_prefix("person/")
        .filter(|name| valid_word(name))
}

fn valid_word(word: &str) -> bool {
    !word.is_empty()
        && word.len() <= 64
        && !word.starts_with(['.', '-'])
        && word.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
}

/// A profile ID is `OWNER/NAME`, where OWNER is the owning person's name.
pub fn check_profile_id(id: &str, owner: &str) -> Result<()> {
    let Some((prefix, name)) = id.split_once('/') else {
        bail!("profile `{id}` must be OWNER/NAME, such as ada/gh");
    };
    if !valid_word(prefix) || !valid_word(name) {
        bail!("profile `{id}`: use lowercase letters, digits, `-`, `_` and `.`");
    }
    if person_name(owner) != Some(prefix) {
        bail!("profile `{id}` must start with its owner's name ({owner})");
    }
    Ok(())
}

pub fn check_env_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    if !valid {
        bail!("`{name}` is not an environment variable name; use A-Z, 0-9 and _");
    }
    // The gateway sets these itself; a profile must not move a command's home or tool path.
    const RESERVED: &[&str] = &[
        "HOME", "PATH", "USER", "LOGNAME", "SHELL", "TERM", "PWD", "TMPDIR",
    ];
    if RESERVED.contains(&name)
        || name.starts_with("LD_")
        || name.starts_with("XDG_")
        || name.starts_with("GIT_CONFIG")
        || name.starts_with("ST_")
    {
        bail!("`{name}` is set by the gateway and cannot be put into a profile");
    }
    Ok(())
}

pub fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

impl GatewayStore {
    pub fn open(root: &Path) -> Result<Self> {
        let db = Connection::open(root.join("sekrets.db"))
            .with_context(|| format!("open the sekrets store in {}", root.display()))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        db.execute_batch(SCHEMA)?;
        Ok(Self {
            root: root.to_path_buf(),
            db,
        })
    }

    /// The home directory a profile's commands run with.
    pub fn profile_home(&self, id: &str) -> PathBuf {
        self.root.join("profiles").join(id).join("home")
    }

    pub fn create_profile(
        &mut self,
        id: &str,
        owner: &str,
        description: Option<&str>,
        policy: &Policy,
        default: bool,
    ) -> Result<Profile> {
        check_profile_id(id, owner)?;
        let now = now_unix_ms();
        let tx = self.db.transaction()?;
        let exists: Option<String> = tx
            .query_row("SELECT owner FROM profiles WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        if exists.is_some() {
            bail!("profile {id} already exists");
        }
        if default {
            tx.execute(
                "UPDATE profiles SET is_default = 0 WHERE owner = ?1",
                [owner],
            )?;
        }
        tx.execute(
            "INSERT INTO profiles (id, owner, description, policy, is_default, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![id, owner, description, serde_json::to_string(policy)?, default, now],
        )?;
        tx.commit()?;
        let home = self.profile_home(id);
        create_private_dir(&home)?;
        self.profile(id)?.context("the profile was just created")
    }

    pub fn profile(&self, id: &str) -> Result<Option<Profile>> {
        let row = self
            .db
            .query_row(
                "SELECT id, owner, description, policy, is_default FROM profiles WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, bool>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, owner, description, policy, default)) = row else {
            return Ok(None);
        };
        let mut statement = self
            .db
            .prepare("SELECT name FROM profile_env WHERE profile = ?1 ORDER BY name")?;
        let env = statement
            .query_map([&id], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(Some(Profile {
            id,
            owner,
            description,
            policy: serde_json::from_str(&policy)?,
            default,
            env,
        }))
    }

    pub fn profiles(&self) -> Result<Vec<Profile>> {
        let ids = {
            let mut statement = self.db.prepare("SELECT id FROM profiles ORDER BY id")?;
            statement
                .query_map([], |row| row.get(0))?
                .collect::<Result<Vec<String>, _>>()?
        };
        let mut profiles = Vec::new();
        for id in ids {
            profiles.extend(self.profile(&id)?);
        }
        Ok(profiles)
    }

    pub fn remove_profile(&mut self, id: &str) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM profiles WHERE id = ?1", [id])?;
        tx.execute("DELETE FROM profile_env WHERE profile = ?1", [id])?;
        tx.execute(
            "UPDATE grants SET revoked_at = ?2, revoked_by = 'profile removed'
             WHERE profile = ?1 AND revoked_at IS NULL",
            params![id, now_unix_ms()],
        )?;
        tx.commit()?;
        let directory = self.root.join("profiles").join(id);
        if directory.exists() {
            std::fs::remove_dir_all(&directory)
                .with_context(|| format!("remove {}", directory.display()))?;
        }
        Ok(())
    }

    pub fn set_policy(&mut self, id: &str, policy: &Policy) -> Result<()> {
        self.db.execute(
            "UPDATE profiles SET policy = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, serde_json::to_string(policy)?, now_unix_ms()],
        )?;
        Ok(())
    }

    pub fn put(&mut self, profile: &str, name: &str, value: &str) -> Result<()> {
        check_env_name(name)?;
        self.db.execute(
            "INSERT INTO profile_env (profile, name, value, set_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (profile, name) DO UPDATE SET value = excluded.value, set_at = excluded.set_at",
            params![profile, name, value, now_unix_ms()],
        )?;
        Ok(())
    }

    pub fn unset(&mut self, profile: &str, name: &str) -> Result<bool> {
        Ok(self.db.execute(
            "DELETE FROM profile_env WHERE profile = ?1 AND name = ?2",
            params![profile, name],
        )? > 0)
    }

    /// The values a profile's commands get as environment variables.
    pub fn env(&self, profile: &str) -> Result<Vec<(String, String)>> {
        let mut statement = self
            .db
            .prepare("SELECT name, value FROM profile_env WHERE profile = ?1 ORDER BY name")?;
        let env = statement
            .query_map([profile], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(env)
    }

    pub fn add_grant(
        &mut self,
        profile: &str,
        grantee: &str,
        policy: &Policy,
        until_unix_ms: Option<i64>,
        granted_by: &str,
    ) -> Result<Grant> {
        let id = format!("grant/{}", uuid::Uuid::now_v7().simple());
        let now = now_unix_ms();
        self.db.execute(
            "INSERT INTO grants (id, profile, grantee, policy, until_ms, granted_by, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                profile,
                grantee,
                serde_json::to_string(policy)?,
                until_unix_ms,
                granted_by,
                now
            ],
        )?;
        Ok(Grant {
            id,
            profile: profile.into(),
            grantee: grantee.into(),
            policy: policy.clone(),
            until_unix_ms,
            granted_by: granted_by.into(),
            created_at_unix_ms: now,
        })
    }

    pub fn grant(&self, id: &str) -> Result<Option<Grant>> {
        Ok(self
            .grants_where("id = ?1 AND revoked_at IS NULL", params![id])?
            .pop())
    }

    pub fn revoke_grant(&mut self, id: &str, by: &str) -> Result<bool> {
        Ok(self.db.execute(
            "UPDATE grants SET revoked_at = ?2, revoked_by = ?3 WHERE id = ?1 AND revoked_at IS NULL",
            params![id, now_unix_ms(), by],
        )? > 0)
    }

    /// Grants not revoked; expired ones included, for listing.
    pub fn grants(&self) -> Result<Vec<Grant>> {
        self.grants_where("revoked_at IS NULL", params![])
    }

    fn grants_where(&self, condition: &str, values: impl rusqlite::Params) -> Result<Vec<Grant>> {
        let mut statement = self.db.prepare(&format!(
            "SELECT id, profile, grantee, policy, until_ms, granted_by, created_at
             FROM grants WHERE {condition} ORDER BY created_at"
        ))?;
        let rows = statement
            .query_map(values, |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(
                |(id, profile, grantee, policy, until_unix_ms, granted_by, created_at_unix_ms)| {
                    Ok(Grant {
                        id,
                        profile,
                        grantee,
                        policy: serde_json::from_str(&policy)?,
                        until_unix_ms,
                        granted_by,
                        created_at_unix_ms,
                    })
                },
            )
            .collect()
    }

    pub fn register_daemon(&mut self, uid: u32, person: &str, node: &str, key: &str) -> Result<()> {
        self.db.execute(
            "INSERT INTO daemons (uid, person, node, key, registered_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (uid) DO UPDATE SET person = excluded.person, node = excluded.node,
               key = excluded.key, registered_at = excluded.registered_at",
            params![uid, person, node, key, now_unix_ms()],
        )?;
        Ok(())
    }

    /// The node and key registered for a Unix user's daemon.
    pub fn daemon(&self, uid: u32) -> Result<Option<(String, String, String)>> {
        Ok(self
            .db
            .query_row(
                "SELECT person, node, key FROM daemons WHERE uid = ?1",
                [uid],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?)
    }

    pub fn lock(&mut self, scope: &str, by: &str, reason: Option<&str>) -> Result<()> {
        self.db.execute(
            "INSERT INTO locks (scope, locked_by, reason, at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (scope) DO UPDATE SET locked_by = excluded.locked_by,
               reason = excluded.reason, at = excluded.at",
            params![scope, by, reason, now_unix_ms()],
        )?;
        Ok(())
    }

    pub fn unlock(&mut self, scope: &str) -> Result<bool> {
        Ok(self
            .db
            .execute("DELETE FROM locks WHERE scope = ?1", [scope])?
            > 0)
    }

    /// The lock that stops a call by `caller_person` with a profile `owner` holds, if any:
    /// a lock on everything, on the caller's person, or on the profile's owner.
    pub fn lock_for(&self, caller_person: &str, owner: &str) -> Result<Option<Lock>> {
        Ok(self
            .db
            .query_row(
                "SELECT scope, locked_by, reason, at FROM locks
                 WHERE scope IN ('*', ?1, ?2) ORDER BY scope = '*' DESC LIMIT 1",
                params![caller_person, owner],
                |row| {
                    Ok(Lock {
                        scope: row.get(0)?,
                        locked_by: row.get(1)?,
                        reason: row.get(2)?,
                        at_unix_ms: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn locks(&self) -> Result<Vec<Lock>> {
        let mut statement = self
            .db
            .prepare("SELECT scope, locked_by, reason, at FROM locks ORDER BY scope")?;
        let locks = statement
            .query_map([], |row| {
                Ok(Lock {
                    scope: row.get(0)?,
                    locked_by: row.get(1)?,
                    reason: row.get(2)?,
                    at_unix_ms: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(locks)
    }

    /// Append an entry both the caller's person and the profile's owner can read.
    pub fn log(
        &mut self,
        caller_person: Option<&str>,
        owner_person: Option<&str>,
        event: &str,
        actor: Option<&str>,
        profile: Option<&str>,
        detail: &serde_json::Value,
    ) -> Result<i64> {
        self.db.execute(
            "INSERT INTO log (at, caller_person, owner_person, event, actor, profile, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                now_unix_ms(),
                caller_person,
                owner_person,
                event,
                actor,
                profile,
                detail.to_string()
            ],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    /// Drop log entries older than `keep_ms`. The daemon records each person's entries as claims
    /// within moments, so the gateway's own copy only needs to bridge an offline daemon.
    pub fn trim_log(&mut self, keep_ms: i64) -> Result<usize> {
        Ok(self
            .db
            .execute("DELETE FROM log WHERE at < ?1", [now_unix_ms() - keep_ms])?)
    }

    pub fn log_for(&self, person: &str, after: i64, limit: i64) -> Result<Vec<LogEntry>> {
        let mut statement = self.db.prepare(
            "SELECT seq, at, event, actor, profile, detail, caller_person, owner_person FROM log
             WHERE seq > ?2 AND (caller_person = ?1 OR owner_person = ?1)
             ORDER BY seq LIMIT ?3",
        )?;
        let rows = statement
            .query_map(params![person, after, limit.clamp(1, 1000)], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(
                |(seq, at_unix_ms, event, actor, profile, detail, caller_person, owner_person)| {
                    Ok(LogEntry {
                        seq,
                        at_unix_ms,
                        caller_person,
                        owner_person,
                        event,
                        actor,
                        profile,
                        detail: serde_json::from_str(&detail)?,
                    })
                },
            )
            .collect()
    }
}

pub fn create_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("create {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_grants_locks_and_the_log_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = GatewayStore::open(directory.path()).unwrap();
        let policy = Policy::build(&["gh-pr".into()], &[], &[]).unwrap();
        assert!(
            store
                .create_profile("bob/gh", "person/ada", None, &policy, true)
                .is_err()
        );
        let profile = store
            .create_profile("ada/agent-gh", "person/ada", Some("agents"), &policy, true)
            .unwrap();
        assert!(profile.default);
        assert!(store.profile_home("ada/agent-gh").is_dir());
        store.put("ada/agent-gh", "GH_TOKEN", "example").unwrap();
        assert!(store.put("ada/agent-gh", "PATH", "/tmp").is_err());
        assert!(store.put("ada/agent-gh", "LD_PRELOAD", "x").is_err());
        assert_eq!(
            store.profile("ada/agent-gh").unwrap().unwrap().env,
            ["GH_TOKEN"]
        );
        let grant = store
            .add_grant(
                "ada/agent-gh",
                "agent/fleet/fixture-web/**",
                &policy,
                None,
                "person/ada",
            )
            .unwrap();
        assert_eq!(store.grants().unwrap().len(), 1);
        assert!(store.revoke_grant(&grant.id, "person/ada").unwrap());
        assert!(store.grants().unwrap().is_empty());
        assert!(
            store
                .lock_for("person/ada", "person/ada")
                .unwrap()
                .is_none()
        );
        store
            .lock("person/ada", "person/ada", Some("test"))
            .unwrap();
        assert!(
            store
                .lock_for("person/robin", "person/ada")
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .lock_for("person/robin", "person/robin")
                .unwrap()
                .is_none()
        );
        store.lock("*", "person/robin", None).unwrap();
        assert_eq!(
            store
                .lock_for("person/robin", "person/robin")
                .unwrap()
                .unwrap()
                .scope,
            "*"
        );
        let seq = store
            .log(
                Some("person/robin"),
                Some("person/ada"),
                "call",
                Some("agent/x"),
                Some("ada/agent-gh"),
                &serde_json::json!({"argv": ["gh"]}),
            )
            .unwrap();
        let entries = store.log_for("person/ada", 0, 10).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(store.trim_log(60_000).unwrap(), 0);
        assert_eq!(store.trim_log(-60_000).unwrap(), 1);
        assert!(store.log_for("person/ada", 0, 10).unwrap().is_empty());
        store
            .log(
                Some("person/robin"),
                Some("person/ada"),
                "call",
                Some("agent/x"),
                Some("ada/agent-gh"),
                &serde_json::json!({"argv": ["gh"]}),
            )
            .unwrap();
        let entries = store.log_for("person/ada", 0, 10).unwrap();
        assert_eq!(entries.len(), 1);
        // A trimmed sequence number is never handed out again, so a daemon's cursor stays good.
        assert_eq!(entries[0].seq, seq + 1);
        assert_eq!(entries[0].caller_person.as_deref(), Some("person/robin"));
        assert_eq!(store.log_for("person/robin", 0, 10).unwrap().len(), 1);
        assert!(store.log_for("person/avery", 0, 10).unwrap().is_empty());
        store.remove_profile("ada/agent-gh").unwrap();
        assert!(store.profile("ada/agent-gh").unwrap().is_none());
        assert!(store.env("ada/agent-gh").unwrap().is_empty());
    }
}
