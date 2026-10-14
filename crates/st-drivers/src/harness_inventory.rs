//! Exact, metadata-only observations from the running native OMP extension.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_COMMANDS: usize = 2_000;
pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Source { Extension, Prompt, Skill }

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub name: String,
    pub source: Source,
}

/// Decimal strings preserve native 64-bit filesystem identities across JSON/JavaScript.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceIdentity {
    pub device: String,
    pub inode: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Availability { Supported, Unsupported, Unavailable }

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub harness: String,
    pub session_id: String,
    pub incarnation_id: String,
    pub observed_at: String,
    /// Local owner evidence only. Never publish the absolute root in a claim/client response.
    pub workspace: Option<String>,
    pub workspace_identity: Option<WorkspaceIdentity>,
    pub dynamic_commands: Availability,
    pub commands: Vec<Command>,
}

/// The channel has authenticated the native session separately; payload identity cannot replace it.
pub fn observation(frame: &Value, driver: &str, native_session: Option<&str>, runtime: &str) -> Result<Option<Snapshot>> {
    if driver != "omp" || frame["type"] != "inventory" { return Ok(None) }
    let bound = native_session.context("inventory has no authenticated native session")?;
    anyhow::ensure!(frame["session_id"].as_str() == Some(bound), "inventory native session was superseded");
    let workspace = frame["workspace"].as_str().filter(|path| {
        !path.is_empty() && path.len() <= 4096 && !path.contains('\0') && std::path::Path::new(path).is_absolute()
    }).map(str::to_owned);
    let workspace_identity: Option<WorkspaceIdentity> = serde_json::from_value(
        frame.get("workspace_identity").cloned().unwrap_or(Value::Null))?;
    anyhow::ensure!(workspace_identity.as_ref().is_none_or(|identity|
        [&identity.device, &identity.inode].iter().all(|value|
            !value.is_empty() && value.len() <= 20 && value.bytes().all(|byte| byte.is_ascii_digit())
                && value.parse::<u64>().is_ok())), "invalid native workspace identity");
    let dynamic_commands: Availability = serde_json::from_value(frame["dynamic_commands"].clone())?;
    anyhow::ensure!(frame["commands"].as_array().is_some_and(|commands| commands.len() <= MAX_COMMANDS),
        "inventory command bound exceeded");
    let mut commands: Vec<Command> = serde_json::from_value(frame["commands"].clone())?;
    anyhow::ensure!(commands.iter().all(|command| {
        !command.name.is_empty() && command.name.len() <= 160 && !command.name.chars().any(char::is_control)
    }), "invalid native command identifier");
    anyhow::ensure!(matches!(dynamic_commands, Availability::Supported) || commands.is_empty(),
        "unsupported inventory cannot advertise commands");
    commands.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    anyhow::ensure!(commands.windows(2).all(|pair| pair[0].name != pair[1].name), "ambiguous native command identifier");
    let observed_at = frame["observed_at"].as_str().context("inventory has no native observation timestamp")?;
    anyhow::ensure!(observed_at.len() <= 64, "inventory timestamp bound exceeded");
    let snapshot = Snapshot { harness: "omp".into(), session_id: bound.into(), incarnation_id: runtime.into(),
        observed_at: observed_at.into(), workspace, workspace_identity, dynamic_commands, commands };
    anyhow::ensure!(serde_json::to_vec(&snapshot)?.len() <= MAX_SNAPSHOT_BYTES, "inventory serialized bound exceeded");
    Ok(Some(snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame() -> Value {
        serde_json::json!({"type":"inventory", "session_id":"native-one",
            "workspace":"/workspace", "observed_at":"2026-10-04T12:00:00Z",
            "dynamic_commands":"supported", "commands":[{"name":"skill:review","source":"skill"}]})
    }
    #[test]
    fn native_binding_and_descriptor_boundary_refuse_stale_or_contentful_sources() {
        assert!(observation(&frame(), "omp", Some("native-two"), "runtime-one").is_err());
        let mut unsafe_frame = frame();
        unsafe_frame["commands"][0]["content"] = "private skill contents".into();
        assert!(observation(&unsafe_frame, "omp", Some("native-one"), "runtime-one").is_err());
        let supported = observation(&frame(), "omp", Some("native-one"), "runtime-one").unwrap().unwrap();
        assert_eq!(supported.session_id, "native-one");
        assert_eq!(supported.incarnation_id, "runtime-one");
        assert_eq!(supported.commands, vec![Command { name: "skill:review".into(), source: Source::Skill }]);
        assert!(observation(&frame(), "claude", Some("native-one"), "runtime-one").unwrap().is_none());
    }
    #[test]
    fn unsupported_and_oversize_inventory_cannot_masquerade_as_supported_empty() {
        let mut unavailable = frame();
        unavailable["dynamic_commands"] = "unsupported".into();
        assert!(observation(&unavailable, "omp", Some("native-one"), "runtime-one").is_err());
        unavailable["commands"] = serde_json::json!([]);
        assert_eq!(observation(&unavailable, "omp", Some("native-one"), "runtime-one").unwrap().unwrap().dynamic_commands, Availability::Unsupported);
        let mut too_many = frame();
        too_many["commands"] = serde_json::json!(vec![serde_json::json!({"name":"review","source":"extension"}); MAX_COMMANDS + 1]);
        assert!(observation(&too_many, "omp", Some("native-one"), "runtime-one").is_err());
    }
}
