//! Resolved native driver paths. Catalog entry points resolve declarations separately.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

pub const ROOT_ENV: &str = "ST_DRIVER_ROOT";
pub const AGENT_DIR_ENV: &str = "ST_DRIVER_AGENT_DIR";
pub const SESSION_DIR_ENV: &str = "ST_DRIVER_SESSION_DIR";
pub const IDENTITY_ENV: &str = "ST_DRIVER_IDENTITY";

/// The host resolves these once, then carries them across provider/channel re-execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paths {
    pub root: PathBuf,
    pub agent_dir: PathBuf,
    pub session_dir: PathBuf,
}

impl Paths {
    pub fn environment(&self, identity: &str) -> Vec<(String, String)> {
        [
            (ROOT_ENV, self.root.to_string_lossy().into_owned()),
            (AGENT_DIR_ENV, self.agent_dir.to_string_lossy().into_owned()),
            (
                SESSION_DIR_ENV,
                self.session_dir.to_string_lossy().into_owned(),
            ),
            (IDENTITY_ENV, identity.to_owned()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
    }

    /// An explicit contract must be complete. Never fall back to an inherited catalog when
    /// native paths are present but malformed or belong to another seat.
    pub fn from_environment(
        identity: &str,
        var: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>> {
        let names = [ROOT_ENV, AGENT_DIR_ENV, SESSION_DIR_ENV, IDENTITY_ENV];
        let values = names.map(var);
        if values.iter().all(Option::is_none) {
            return Ok(None);
        }
        let mut values = names.into_iter().zip(values).map(|(name, value)| {
            value
                .filter(|value| !value.is_empty())
                .with_context(|| format!("native driver paths have no {name}"))
        });
        let paths = Self {
            root: values.next().unwrap()?.into(),
            agent_dir: values.next().unwrap()?.into(),
            session_dir: values.next().unwrap()?.into(),
        };
        anyhow::ensure!(
            values.next().unwrap()? == identity,
            "native driver paths belong to another identity"
        );
        anyhow::ensure!(
            paths.root.is_absolute()
                && paths.agent_dir.is_absolute()
                && paths.session_dir.is_absolute(),
            "native driver paths must be absolute"
        );
        Ok(Some(paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn partial_or_foreign_native_paths_cannot_fall_back_to_a_catalog() {
        let paths = Paths {
            root: "/tmp/example-driver".into(),
            agent_dir: "/tmp/example-driver/observations".into(),
            session_dir: "/tmp/example-driver/sessions/claude".into(),
        };
        let mut env: BTreeMap<_, _> = paths.environment("example/seat").into_iter().collect();
        env.insert("CATALOG".into(), "/tmp/predecessor-catalog".into());
        assert_eq!(
            Paths::from_environment("example/seat", &|key| env.get(key).cloned()).unwrap(),
            Some(paths)
        );
        assert!(Paths::from_environment("another/seat", &|key| env.get(key).cloned()).is_err());
        env.remove(SESSION_DIR_ENV);
        assert!(Paths::from_environment("example/seat", &|key| env.get(key).cloned()).is_err());
        env.insert(SESSION_DIR_ENV.into(), "relative/sessions".into());
        assert!(Paths::from_environment("example/seat", &|key| env.get(key).cloned()).is_err());
    }
}
