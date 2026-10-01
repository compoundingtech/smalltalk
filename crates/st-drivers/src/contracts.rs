//! Names shared by current drivers and readers of already-running driver generations.

use std::path::Path;

/// The retired spelling of a harness variable. New launchers export only the current name.
pub fn legacy_env_name(name: &str) -> Option<String> {
    (name.starts_with("ST_CLAUDE_")
        || name.starts_with("ST_PI_CHANNEL_")
        || name.starts_with("ST_OMP_CHANNEL_"))
    .then(|| format!("ST2_{}", name.strip_prefix("ST_").unwrap()))
}

/// Read a current spelling first, then the spelling retained by an older running provider.
/// An explicitly empty current value masks an inherited legacy value rather than reviving it.
pub fn env_with(name: &str, get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    get(name)
        .or_else(|| legacy_env_name(name).and_then(|legacy| get(&legacy)))
        .filter(|value| !value.is_empty())
}

pub fn env(name: &str) -> Option<String> {
    env_with(name, &|key| std::env::var(key).ok())
}

/// Admit only the exact old spelling of this version; future or foreign schemas stay rejected.
pub(crate) fn schema_matches(actual: &str, current: &str) -> bool {
    actual == current
        || current
            .strip_prefix("st.")
            .is_some_and(|suffix| actual.strip_prefix("st2.") == Some(suffix))
}

/// An adopted incarnation keeps its record spelling while older sibling processes still read it.
/// A fresh ownership claim uses the current schema, so the next session advances the format.
pub(crate) fn schema_for_session(agent_dir: &Path, session: &str, current: &str) -> String {
    let legacy = std::fs::read(crate::harness_state::harness_state_path(agent_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|record| {
            record["schema"] == "st2.harness-state.v1"
                && record["incarnation"].as_str() == Some(session)
                && !session.is_empty()
        });
    if legacy {
        current.replacen("st.", "st2.", 1)
    } else {
        current.to_owned()
    }
}

/// Diagnostics have no session field; the live ownership record determines their spelling.
pub(crate) fn schema_for_agent(agent_dir: &Path, current: &str) -> String {
    let legacy = std::fs::read(crate::harness_state::harness_state_path(agent_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|record| record["schema"] == "st2.harness-state.v1");
    if legacy {
        current.replacen("st.", "st2.", 1)
    } else {
        current.to_owned()
    }
}

/// Preserve the schema family of a record belonging to an adopted provider.
pub(crate) fn schema_for_owner(owner: &str, current: &str) -> String {
    if owner.starts_with("st2.") {
        current.replacen("st.", "st2.", 1)
    } else {
        current.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn current_names_win_and_empty_values_mask_legacy_ownership() {
        for prefix in ["CLAUDE", "PI_CHANNEL", "OMP_CHANNEL"] {
            let current = format!("ST_{prefix}_SESSION");
            let legacy = format!("ST2_{prefix}_SESSION");
            let mut vars = BTreeMap::from([(legacy.clone(), "older".to_owned())]);
            assert_eq!(
                env_with(&current, &|key| vars.get(key).cloned()).as_deref(),
                Some("older")
            );
            vars.insert(current.clone(), "newer".into());
            assert_eq!(
                env_with(&current, &|key| vars.get(key).cloned()).as_deref(),
                Some("newer")
            );
            vars.insert(current.clone(), String::new());
            assert_eq!(env_with(&current, &|key| vars.get(key).cloned()), None);
        }
        assert_eq!(legacy_env_name("ST_AGENT"), None);
        assert_eq!(legacy_env_name("HOME"), None);
    }

    #[test]
    fn schema_aliases_preserve_version_admission() {
        assert!(schema_matches("st.harness-state.v1", "st.harness-state.v1"));
        assert!(schema_matches(
            "st2.harness-state.v1",
            "st.harness-state.v1"
        ));
        assert!(!schema_matches(
            "st2.harness-state.v2",
            "st.harness-state.v1"
        ));
        assert!(!schema_matches(
            "st2.harness-context.v1",
            "st.harness-state.v1"
        ));
    }
    #[test]
    fn adopted_records_keep_legacy_siblings_readable_until_a_new_incarnation_claims() {
        use crate::{
            harness_context as context, harness_state as state, harness_timeline as timeline,
        };
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("agents/host/worker");
        std::fs::create_dir_all(&dir).unwrap();
        let path = state::harness_state_path(&dir);
        let seq = state::claim(&dir, "host.worker", "codex", "old").unwrap();
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        record["schema"] = "st2.harness-state.v1".into();
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        let schema = |path: &Path| {
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap()).unwrap()["schema"].as_str().unwrap().to_owned()
        };
        let mut old =
            state::Writer::new(&dir, "host.worker", "codex", Some("host.worker".into())).with_ownership("old", seq);
        old.observe(state::Observation::new(
            state::Activity::Active,
            state::BlockedOn::None,
            state::InputBuffer::Unknown,
        ))
        .unwrap();
        old.heartbeat().unwrap();
        assert_eq!(schema(&path), "st2.harness-state.v1");
        assert!(state::read(&path, None).is_some());
        let mut numbers = context::Writer::new(&dir, "host.worker", context::Harness::Codex)
            .unwrap()
            .with_session("old");
        numbers
            .observe(context::Reading {
                used_percent: Some(15.0),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            schema(&context::harness_context_path(&dir)),
            "st2.harness-context.v1"
        );
        assert!(context::read(&context::harness_context_path(&dir)).is_some());
        let mut events = timeline::Writer::new(&dir, "codex", "old");
        events
            .append(
                "one",
                timeline::Role::User,
                timeline::EntryType::Message,
                serde_json::json!({"content":"hello"}),
                true,
            )
            .unwrap();
        assert_eq!(
            schema(&timeline::timeline_path(&dir)),
            "st2.harness-timeline.v1"
        );
        assert!(timeline::read(&timeline::timeline_path(&dir)).is_some());
        // Reclaiming an adopted runtime still uses its schema family.
        state::claim(&dir, "host.worker", "codex", "old").unwrap();
        assert_eq!(schema(&path), "st2.harness-state.v1");
        let next = state::claim(&dir, "host.worker", "codex", "new").unwrap();
        assert!(next > seq);
        assert_eq!(schema(&path), "st.harness-state.v1");
        let claimed = std::fs::read(&path).unwrap();
        old.ended("exit 0").unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            claimed,
            "legacy owner must not replace a fresh claim"
        );
        let mut numbers = context::Writer::new(&dir, "host.worker", context::Harness::Codex)
            .unwrap()
            .with_session("new");
        numbers.observe(context::Reading::default()).unwrap();
        assert_eq!(
            schema(&context::harness_context_path(&dir)),
            "st.harness-context.v1"
        );
        timeline::Writer::new(&dir, "codex", "new")
            .append(
                "two",
                timeline::Role::User,
                timeline::EntryType::Message,
                serde_json::json!({"content":"hello"}),
                true,
            )
            .unwrap();
        assert_eq!(
            schema(&timeline::timeline_path(&dir)),
            "st.harness-timeline.v1"
        );
    }
}
