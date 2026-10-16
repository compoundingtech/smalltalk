//! Source- and incarnation-bound launch observations, served by the requested host.
use crate::model::ClaimRecord;
use crate::store::{Store, claim_log_order};
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

/// A conditional wait marker over this bounded response, not a control-authority token.
pub fn wait_marker(status: &LaunchStatus) -> anyhow::Result<String> {
    use sha2::Digest as _;
    Ok(hex::encode(sha2::Sha256::digest(serde_json::to_vec(
        status,
    )?)))
}

pub fn read(store: &Store, subject: &str, token: &str) -> anyhow::Result<LaunchStatus> {
    store.read_snapshot(|_| read_at_snapshot(store, subject, token))
}

fn covered_window(
    store: &Store,
    subject: &str,
    kind: &str,
    source: Option<&ClaimRecord>,
) -> anyhow::Result<Vec<ClaimRecord>> {
    let (records, truncated) = store.launch_observations(subject, kind)?;
    anyhow::ensure!(
        !truncated
            || source.zip(records.first()).is_some_and(|(source, oldest)| {
                claim_log_order(oldest) <= claim_log_order(source)
            }),
        "requested {kind} evidence exceeds the retained window; retry explicitly"
    );
    // Replicated person-authored actions and observations from another host do not
    // prove that this requested runtime host launched or observed the incarnation.
    Ok(records
        .into_iter()
        .filter(|claim| claim.origin == store.origin())
        .collect())
}

fn read_at_snapshot(store: &Store, subject: &str, token: &str) -> anyhow::Result<LaunchStatus> {
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
    let requested = store.launch_request_source(subject, token)?;
    let launch_token = requested
        .as_ref()
        .map(|(_, token)| token.as_str())
        .unwrap_or(token);
    let selected = store.selected_desired_token(subject)?;
    let selected_launch = match selected.as_deref() {
        Some(selected) if selected != token => store
            .launch_request_source(subject, selected)?
            .map(|(_, token)| token),
        Some(_) => requested.as_ref().map(|(_, token)| token.clone()),
        None => None,
    };
    let destination = store
        .desired_subjects_named(&[subject.to_owned()])?
        .into_iter()
        .next()
        .and_then(|desired| desired.member)
        .map(|member| member.host);
    if requested.is_some()
        && selected_launch.as_deref() == Some(launch_token)
        && destination.as_deref() == Some(store.origin())
    {
        status.stage = "replicated".into();
    }
    if requested.is_none() {
        return Ok(status);
    }
    let source = requested.as_ref().map(|(source, _)| source);
    let launches = covered_window(store, subject, "runtime.action.succeeded", source)?;
    let launch = launches.iter().find(|claim| {
        claim.body["fields"]["action"] == "start"
            && claim.body["fields"]["desired_token"] == launch_token
    });
    let failures = covered_window(store, subject, "runtime.action.failed", source)?;
    let failed = failures.iter().find(|claim| {
        claim.body["fields"]["action"] == "start"
            && claim.body["fields"]["desired_token"] == launch_token
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
            let exits = covered_window(store, subject, "runtime.observed", Some(launch))?;
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
                let diagnostics =
                    covered_window(store, subject, "harness.diagnostic", Some(launch))?;
                if diagnostics.iter().any(|claim| {
                    claim.body["fields"]["incarnation_id"] == incarnation
                        && claim.body["fields"]["code"] == "launch-exited-before-ready"
                }) {
                    status.reason =
                        Some("the requested incarnation exited before harness readiness".into());
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
            // This is an observed launch milestone, never current input/attach authority.
            // Reconcile writes the receipt only after this exact incarnation becomes ready.
            // Reading it cannot fold harness history or initialize a derived cache.
            if store
                .local_observation_for_key(
                    subject,
                    "runtime.action.succeeded",
                    &format!("launch-ready:{subject}:{incarnation}"),
                )?
                .is_some_and(|receipt| {
                    receipt.body["fields"]["action"] == "harness-ready"
                        && receipt.body["fields"]["incarnation_id"] == incarnation
                })
            {
                status.stage = "harness-ready".into();
            }
        }
    }
    if selected_launch
        .as_deref()
        .is_some_and(|selected| selected != launch_token)
    {
        status.stage = "superseded".into();
        status.reason = Some("the requested declaration was replaced; retry explicitly".into());
    }
    Ok(status)
}
