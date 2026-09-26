use std::{
    collections::hash_map::DefaultHasher,
    fs::{self, OpenOptions},
    hash::{Hash, Hasher},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::model::Model;

const VERSION: u32 = 1;
const MAX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Deserialize, Serialize)]
struct CachedModel {
    version: u32,
    actor: String,
    saved_at_ms: u128,
    model: Model,
}

pub fn path(endpoint: &Path, actor: &str) -> Option<PathBuf> {
    if actor.is_empty() {
        return None;
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    if !base.is_absolute() {
        return None;
    }
    let mut hasher = DefaultHasher::new();
    endpoint.hash(&mut hasher);
    actor.hash(&mut hasher);
    Some(
        base.join("st3")
            .join("stui")
            .join(format!("{:016x}.json", hasher.finish())),
    )
}

fn now_ms() -> Option<u128> {
    Some(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
}

pub fn load(path: &Path, actor: &str) -> Option<Model> {
    let metadata = fs::metadata(path).ok()?;
    if metadata.len() > MAX_BYTES || metadata.permissions().mode() & 0o077 != 0 {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let cached: CachedModel = serde_json::from_slice(&bytes).ok()?;
    let age = now_ms()?.checked_sub(cached.saved_at_ms)?;
    if cached.version != VERSION || cached.actor != actor || age > MAX_AGE.as_millis() {
        return None;
    }
    let mut model = cached.model;
    model.timeline.clear();
    model.timeline_truncated = false;
    model.status = "Cached · refreshing…".into();
    Some(model)
}

pub fn save(path: &Path, actor: &str, model: &Model) -> std::io::Result<()> {
    let mut safe = model.clone();
    safe.timeline.clear();
    safe.timeline_truncated = false;
    let bytes = serde_json::to_vec(&CachedModel {
        version: VERSION,
        actor: actor.to_owned(),
        saved_at_ms: now_ms().unwrap_or_default(),
        model: safe,
    })?;
    if bytes.len() as u64 > MAX_BYTES {
        return Ok(());
    }
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(temp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_identity_is_scoped_to_endpoint_and_actor() {
        let a = path(Path::new("/tmp/endpoint-a.sock"), "person/one").unwrap();
        assert_ne!(
            a,
            path(Path::new("/tmp/endpoint-b.sock"), "person/one").unwrap()
        );
        assert_ne!(
            a,
            path(Path::new("/tmp/endpoint-a.sock"), "person/two").unwrap()
        );
        assert!(a.file_name().unwrap().to_string_lossy().ends_with(".json"));
    }

    #[test]
    fn private_cache_survives_restart_but_not_actor_change() {
        let dir = std::env::temp_dir().join(format!("stui-cache-test-{}", uuid::Uuid::now_v7()));
        let path = dir.join("snapshot.json");
        let mut model = Model::default();
        model.actor = "person/one".into();
        model.status = "Connected".into();
        model.event_cursor = "event/one".into();
        save(&path, "person/one", &model).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        let restored = load(&path, "person/one").unwrap();
        assert_eq!(restored.actor, "person/one");
        assert_eq!(restored.status, "Cached · refreshing…");
        assert!(load(&path, "person/two").is_none());
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&dir).unwrap();
    }
}
