//! Source- and incarnation-bound launch observations, served by the requested host.
use crate::store::Store;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LaunchStatus {
    pub host: String,
    pub subject: String,
    pub desired_token: String,
    pub stage: String,
    pub incarnation_id: Option<String>,
    pub exit_code: Option<i64>,
    pub reason: Option<String>,
    pub output_tail: Option<String>,
}

pub fn read(store: &Store, subject: &str, token: &str) -> anyhow::Result<LaunchStatus> {
    let mut status = LaunchStatus {
        host: store.origin().into(),
        subject: subject.into(),
        desired_token: token.into(),
        stage: "replicating".into(),
        incarnation_id: None,
        exit_code: None,
        reason: None,
        output_tail: None,
    };
    let selected = store.selected_desired_token(subject)?;
    let destination = store
        .desired_subjects_named(&[subject.to_owned()])?
        .into_iter()
        .next()
        .and_then(|desired| desired.member)
        .map(|member| member.host);
    if selected.as_deref() == Some(token) && destination.as_deref() == Some(store.origin()) {
        status.stage = "replicated".into();
    }
    let (launches, truncated) = store.launch_observations(subject, "runtime.action.succeeded")?;
    if truncated {
        let declaration = store.claim_by_id(token)?;
        anyhow::ensure!(
            declaration
                .as_ref()
                .zip(launches.first())
                .is_some_and(
                    |(source, oldest)| oldest.accepted_at_unix_ms < source.accepted_at_unix_ms
                ),
            "requested launch evidence exceeds the retained window; retry explicitly"
        );
    }
    let launch = launches.iter().find(|claim| {
        claim.body["fields"]["action"] == "start" && claim.body["fields"]["desired_token"] == token
    });
    let (failures, _) = store.launch_observations(subject, "runtime.action.failed")?;
    let failed = failures.iter().find(|claim| {
        claim.body["fields"]["action"] == "start" && claim.body["fields"]["desired_token"] == token
    });
    if let Some(failed) = failed.filter(|failed| {
        launch.is_none_or(|launch| {
            crate::store::claim_log_order(failed) < crate::store::claim_log_order(launch)
        })
    }) {
        status.stage = "launch-failed".into();
        status.reason = failed.body["fields"]["reason"].as_str().map(str::to_owned);
        return Ok(status);
    }
    if let Some(launch) = launch {
        status.stage = "launched".into();
        status.incarnation_id = launch.body["fields"]["incarnation_id"]
            .as_str()
            .map(str::to_owned);
        if let Some(incarnation) = status.incarnation_id.as_deref() {
            let (exits, _) = store.launch_observations(subject, "runtime.observed")?;
            if let Some(exit) = exits.iter().find(|claim| {
                claim.body["fields"]["incarnation_id"] == incarnation
                    && matches!(
                        claim.body["fields"]["status"].as_str(),
                        Some("exited" | "vanished" | "stopped")
                    )
            }) {
                status.stage = "exited".into();
                status.exit_code = exit.body["fields"]["exit_code"].as_i64();
                status.reason = Some("the requested incarnation exited before attachment".into());
                let (diagnostics, _) = store.launch_observations(subject, "harness.diagnostic")?;
                if let Some(diagnostic) = diagnostics.iter().find(|claim| {
                    claim.body["fields"]["incarnation_id"] == incarnation
                        && claim.body["fields"]["code"] == "launch-exited-before-ready"
                }) {
                    status.reason = diagnostic.body["fields"]["reason"]
                        .as_str()
                        .map(str::to_owned);
                }
                status.output_tail = store
                    .local_observation_for_key(
                        subject,
                        "runtime.action.failed",
                        &format!("launch-output:{subject}:{incarnation}"),
                    )?
                    .and_then(|receipt| {
                        receipt.body["fields"]["reason"].as_str().map(str::to_owned)
                    });
                return Ok(status);
            }
            if store
                .current_harness(subject)?
                .is_some_and(|harness| harness.incarnation_id == incarnation && harness.is_ready())
            {
                status.stage = "harness-ready".into();
            }
        }
    }
    if selected
        .as_deref()
        .is_some_and(|selected| selected != token)
    {
        status.stage = "superseded".into();
        status.reason = Some("the requested declaration was replaced; retry explicitly".into());
    }
    Ok(status)
}
