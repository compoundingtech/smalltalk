//! Names shared by current drivers and readers of already-running driver generations.

use std::path::Path;

/// The retired ST2 spelling, retained for already-running providers.
pub fn legacy_env_name(name: &str) -> Option<String> {
    (name.starts_with("ST_CLAUDE_")
        || name.starts_with("ST_PI_CHANNEL_")
        || name.starts_with("ST_OMP_CHANNEL_"))
    .then(|| format!("ST2_{}", name.strip_prefix("ST_").unwrap()))
}

// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-16-env-fractal-dual-read — DELETE at contraction — https://github.com/compoundingtech/smalltalk/issues/3032
/// Fractal spelling of an ST/ST3 environment variable.
pub fn fractal_env_name(name: &str) -> Option<String> {
    name.strip_prefix("ST_")
        .or_else(|| name.strip_prefix("ST3_"))
        .map(|suffix| format!("FRACTAL_{suffix}"))
}

fn identity_env(name: &str) -> bool {
    matches!(
        name,
        "ST_AGENT" | "ST_SUPERVISOR" | "ST3_SUBJECT" | "ST3_INCARNATION"
            | "ST_GATE_CAPABILITY" | "ST3_TERMINAL_CAPABILITY"
    ) || name.starts_with("ST_DRIVER_")
        || ((name.starts_with("ST_CLAUDE_")
            || name.starts_with("ST_PI_CHANNEL_")
            || name.starts_with("ST_OMP_CHANNEL_"))
            && (name.ends_with("_IDENTITY")
                || name.ends_with("_SESSION")
                || name.ends_with("_SESSION_SEQ")
                || name.ends_with("_SEQ")
                || name.ends_with("_RUNTIME_ID")
                || name.ends_with("_RESUME_GENERATION")))
}

/// Prefer Fractal, then ST/ST3, then the existing ST2 harness alias.
/// An explicitly empty value masks every older spelling.
pub fn env_with(name: &str, get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    env_with_warning(name, get, &mut |message| eprintln!("{message}"))
}

fn env_with_warning(
    name: &str,
    get: &dyn Fn(&str) -> Option<String>,
    warn: &mut dyn FnMut(String),
) -> Option<String> {
    let old = get(name).or_else(|| legacy_env_name(name).and_then(|legacy| get(&legacy)));
    let Some(new_name) = fractal_env_name(name) else {
        return old.filter(|value| !value.is_empty());
    };
    let Some(new) = get(&new_name) else {
        return old.filter(|value| !value.is_empty());
    };
    if old.as_ref().is_some_and(|old| old != &new) {
        let identity = identity_env(name);
        warn(format!(
            "st: conflicting environment variables {new_name} and {name}; {}",
            if identity { "ignoring both identity values" } else { "using the Fractal value" }
        ));
        if identity {
            return None;
        }
    }
    Some(new).filter(|value| !value.is_empty())
}

pub fn env(name: &str) -> Option<String> {
    env_with(name, &|key| std::env::var(key).ok())
}
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-16-env-fractal-dual-read

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
    fn fractal_names_prefer_new_values_without_disclosing_conflicting_values() {
        for name in ["ST_AGENT", "ST_SUPERVISOR", "ST_CLAUDE_SESSION", "ST3_SUBJECT", "ST3_BIN"] {
            let new = fractal_env_name(name).unwrap();
            for (old_value, new_value, expected, warning) in [
                (None, None, None, false),
                (Some("seat"), None, Some("seat"), false),
                (None, Some("seat"), Some("seat"), false),
                (Some("seat"), Some("seat"), Some("seat"), false),
                (Some("old-secret"), Some("new-secret"),
                    if identity_env(name) { None } else { Some("new-secret") }, true),
                (Some("seat"), Some(""), None, true),
                (Some(""), None, None, false),
            ] {
                let vars: BTreeMap<_, _> = [(name, old_value), (new.as_str(), new_value)]
                    .into_iter()
                    .filter_map(|(key, value)| value.map(|value| (key, value.to_owned())))
                    .collect();
                let mut warnings = Vec::new();
                let actual = env_with_warning(name, &|key| vars.get(key).cloned(),
                    &mut |message| warnings.push(message));
                assert_eq!(actual.as_deref(), expected, "{name}: {vars:?}");
                assert_eq!(!warnings.is_empty(), warning);
                for message in warnings {
                    assert!(message.contains(name) && message.contains(&new));
                    assert!(!message.contains("old-secret") && !message.contains("new-secret"));
                }
            }
        }
        assert_eq!(fractal_env_name("HOME"), None);
    }

    #[test]
    fn fractal_empty_masks_st2_and_conflicting_st2_identity_is_rejected() {
        let mut vars = BTreeMap::from([
            ("ST2_OMP_CHANNEL_SESSION", "old".to_owned()),
            ("FRACTAL_OMP_CHANNEL_SESSION", String::new()),
        ]);
        assert_eq!(env_with("ST_OMP_CHANNEL_SESSION", &|key| vars.get(key).cloned()), None);
        vars.insert("FRACTAL_OMP_CHANNEL_SESSION", "new".into());
        assert_eq!(env_with("ST_OMP_CHANNEL_SESSION", &|key| vars.get(key).cloned()), None);
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
