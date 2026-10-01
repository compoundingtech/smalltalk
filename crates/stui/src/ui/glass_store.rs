//! A person's glasses on this device, until the graph keeps them. The file holds what the graph
//! will: each glass's name and tabs as layouts of pane keys, and nothing about focus or scroll
//! beyond which glass was used last here.

use super::layout::Layout;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const VERSION: u32 = 1;
const MAX_BYTES: u64 = 1 << 20;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Stored {
    pub version: u32,
    /// The glass `stui --glasses` opens here.
    pub last: Option<String>,
    pub glasses: Vec<StoredGlass>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredGlass {
    pub name: String,
    /// The tabs after Home.
    pub tabs: Vec<StoredTab>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredTab {
    pub layout: Layout,
}

/// Where this person's glasses live on this device.
pub fn path(person: &str) -> Option<PathBuf> {
    if person.is_empty() {
        return None;
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
        })?;
    if !base.is_absolute() {
        return None;
    }
    let mut hasher = DefaultHasher::new();
    person.hash(&mut hasher);
    Some(
        base.join("st3")
            .join("stui")
            .join(format!("glasses-{:016x}.json", hasher.finish())),
    )
}

/// What the file holds; nothing when it is missing, unreadable, too big or open to others.
pub fn load(path: &Path) -> Stored {
    let readable = fs::metadata(path).ok().filter(|metadata| {
        metadata.len() <= MAX_BYTES && metadata.permissions().mode() & 0o077 == 0
    });
    readable
        .and_then(|_| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Stored>(&bytes).ok())
        .filter(|stored| stored.version == VERSION)
        .unwrap_or_default()
}

/// Replace the file at once, readable by this user only.
pub fn save(path: &Path, stored: &Stored) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(&Stored {
        version: VERSION,
        ..stored.clone()
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
    fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::layout::Side;

    #[test]
    fn glasses_round_trip_through_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("st3").join("stui").join("glasses.json");
        assert_eq!(load(&path), Stored::default(), "nothing saved yet");
        let mut layout = Layout::pane("agent:agent/example/harbor");
        layout.split(0, Side::Right, "mission:mission/example/audit");
        let stored = Stored {
            version: VERSION,
            last: Some("review".into()),
            glasses: vec![
                StoredGlass {
                    name: "main".into(),
                    tabs: vec![],
                },
                StoredGlass {
                    name: "review".into(),
                    tabs: vec![StoredTab { layout }],
                },
            ],
        };
        save(&path, &stored).unwrap();
        assert_eq!(load(&path), stored);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // A file others can read is not trusted.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(load(&path), Stored::default());
    }
}
