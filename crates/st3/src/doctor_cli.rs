//! Explicit local developer diagnostics and human-only CLI recovery advice.
use std::collections::BTreeMap;
use std::path::Path;

use st3::client::{OutagePhase, daemon_unreachable};
use st3::model::DoctorCheck;

pub(super) fn developer_tools(environment: &BTreeMap<String, String>) -> DoctorCheck {
    let mut tools = vec!["cargo", "rustc", "sccache", "git", "nix"];
    if cfg!(target_os = "linux") {
        tools.push("mold");
    }
    let missing = tools
        .into_iter()
        .filter(|tool| st_runtime::resolve_executable(tool, environment).is_err())
        .collect::<Vec<_>>();
    DoctorCheck {
        name: "build-tools".into(),
        status: if missing.is_empty() { "pass" } else { "warn" }.into(),
        message: if missing.is_empty() {
            "source-build tools are on this account's login PATH; compilation was not tested".into()
        } else {
            format!(
                "missing from this account's login PATH: {}; these tools are needed to build Smalltalk from source",
                missing.join(", ")
            )
        },
    }
}

pub(super) fn human_outage_message(error: &anyhow::Error) -> Option<String> {
    // Keep native drivers and seats on their existing retry/recovery messages.
    if ["ST_AGENT", "ST_MISSION_RUN", "ST3_INCARNATION"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return None;
    }
    let outage = daemon_unreachable(error)?;
    // Starting recovery and remote endpoints need their own diagnosis, not a local setup.
    if outage.phase() != OutagePhase::Connect
        || !Path::new(outage.endpoint()).is_absolute()
        || st3::startup::read(Path::new(outage.endpoint()))
            .is_some_and(|readiness| readiness.status == "starting")
    {
        return None;
    }
    Some(format!(
        "the st daemon at {} is unavailable. Run `st` to open Smalltalk, or `st setup` to set up or start its background service, then retry this command",
        outage.endpoint()
    ))
}
