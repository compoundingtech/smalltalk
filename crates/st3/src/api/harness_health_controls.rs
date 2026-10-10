//! Test-only pure health reduction proposal. Inputs are captured at one namespace/cut; no Store,
//! provider-file, process, network or writer access belongs in this formatter.
//! All positive inputs below are authored model fixtures. Matching namespace/cut
//! strings refuse obvious mixtures; they do not authenticate an admitted source,
//! capability, native invocation or complete mailbox. No runtime caller is installed.

use super::harness_health_output_controls::{SILENCE_MS, Snapshot};
use serde::{Deserialize, Serialize};

use crate::model::CurrentHarnessView;

const STALE_MS: u64 = 15 * 60 * 1_000;
const FUTURE_SKEW_MS: u64 = 60 * 1_000;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Healthy,
    Suspect,
    /// Reserved for a positive supported stall observation. Silence is insufficient.
    Wedged,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    NativeOutputRecent,
    OutputSilenceWithPendingInbound,
    MissingNativeEvidence,
    MissingOwnershipEvidence,
    MissingNativeOutput,
    MissingProgressAnchor,
    IncompleteToolCoverage,
    UnresolvedTool,
    HumanBlocked,
    StaleEvidence,
    FutureEvidence,
    OwnershipMismatch,
    RuntimeUnavailable,
    NativeUnavailable,
    TransportUnavailable,
    NoRecentOutput,
    IncompletePendingEvidence,
    MissingPendingAnchor,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Evidence {
    pub captured_index: u64,
    pub driver: Option<String>,
    pub component: Option<String>,
    pub capability: Option<String>,
    pub incarnation_id: Option<String>,
    pub provider_incarnation: Option<String>,
    pub ownership_sequence: Option<u64>,
    pub current_provider_incarnation: Option<String>,
    pub current_ownership_sequence: Option<u64>,
    pub current_provider_source_id: Option<String>,
    pub observation_id: Option<String>,
    pub last_harness_output_at: Option<String>,
    pub output_entry_id: Option<String>,
    /// Exact only when the bounded capture is complete. Never a global count.
    pub pending_inbound: Option<u64>,
    pub pending_inbound_lower_bound: u64,
    pub pending_inbound_complete: bool,
    pub unresolved_tool_since: Option<String>,
    pub tool_coverage_complete: bool,
    pub reachability: String,
    pub harness_state: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Health {
    pub status: Status,
    pub since: Option<String>,
    pub reason: Reason,
    pub evidence: Evidence,
}

#[derive(Clone, Debug)]
pub struct Native {
    pub namespace: String,
    /// The reader cut at which this image was captured, not its claim's append index.
    pub captured_index: u64,
    pub incarnation_id: String,
    pub provider_incarnation: String,
    pub ownership_sequence: u64,
    pub observed_at_ms: u64,
    pub since_ms: u64,
    pub claim: String,
    pub output: Snapshot,
}

/// Independently selected current ownership from admitted provider authority at the
/// same namespace/cut. The capture must not fill these fields from `Native.output`.
/// This input contract alone does not validate the capture or establish authority.
#[derive(Clone, Debug)]
pub struct SelectedProvider {
    pub namespace: String,
    pub captured_index: u64,
    pub runtime_incarnation: String,
    pub provider_incarnation: String,
    pub ownership_sequence: u64,
    pub driver: String,
    pub component: String,
    pub source_id: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Pending {
    pub lower_bound: u64,
    pub complete: bool,
    pub oldest_at_ms: Option<u64>,
    /// Retained continuous pending predicate; never derived from the oldest row.
    pub continuous_since_ms: Option<u64>,
}

pub struct Inputs<'a> {
    pub namespace: &'a str,
    pub captured_index: u64,
    pub now_ms: u64,
    pub declared_driver: Option<&'a str>,
    pub runtime_incarnation: Option<&'a str>,
    pub runtime_running: bool,
    pub reachability: &'a str,
    pub harness: Option<&'a CurrentHarnessView>,
    pub current_provider: Option<&'a SelectedProvider>,
    pub native: Option<&'a Native>,
    pub pending: Pending,
    pub mailbox_fault: bool,
}

/// Earliest time at which these immutable inputs can reduce differently without a
/// new claim. The existing roster owner can combine this with its queue deadline;
/// this helper does not register a timer, read a source, or build a cached card.
/// Expired boundaries are excluded so an UNKNOWN image does not spin a refresher.
pub fn valid_until(inputs: &Inputs<'_>) -> Option<u64> {
    let mut next = None;
    let mut include = |boundary: u64| {
        if boundary > inputs.now_ms {
            next = Some(next.map_or(boundary, |prior: u64| prior.min(boundary)));
        }
    };
    {
        let mut observed = |at: u64| {
            // A future image is refused until it enters the allowed skew window.
            include(at.saturating_sub(FUTURE_SKEW_MS));
            include(at.saturating_add(STALE_MS));
        };
        if let Some(harness) = inputs.harness
            && let Ok(at) = u64::try_from(harness.observed_at_unix_ms)
        {
            observed(at);
        }
        if let Some(native) = inputs.native {
            observed(native.observed_at_ms);
        }
    }
    if let Some(harness) = inputs.harness
        && let Ok(since) = u64::try_from(harness.since_unix_ms)
    {
        include(since.saturating_sub(FUTURE_SKEW_MS));
    }
    if let Some(native) = inputs.native {
        include(native.since_ms.saturating_sub(FUTURE_SKEW_MS));
        if let Some(output) = &native.output.last_output {
            include(output.at_unix_ms.saturating_sub(FUTURE_SKEW_MS));
            include(output.at_unix_ms.saturating_add(SILENCE_MS));
        }
        if let Some(since) = native.output.progress_run_since_ms {
            include(since.saturating_sub(FUTURE_SKEW_MS));
        }
        for since in native.output.unresolved_tools.values() {
            include(since.saturating_sub(FUTURE_SKEW_MS));
        }
    }
    for since in [
        inputs.pending.oldest_at_ms,
        inputs.pending.continuous_since_ms,
    ]
    .into_iter()
    .flatten()
    {
        include(since.saturating_sub(FUTURE_SKEW_MS));
    }
    next
}

pub fn reduce(inputs: Inputs<'_>) -> Health {
    let native = inputs.native;
    let output = native.and_then(|native| native.output.last_output.as_ref());
    let tool_since =
        native.and_then(|native| native.output.unresolved_tools.values().min().copied());
    let stamp = |time: u64| super::client_timestamp(u128::from(time));
    let evidence = Evidence {
        captured_index: inputs.captured_index,
        driver: inputs.declared_driver.map(str::to_owned),
        component: native.map(|native| native.output.component.clone()),
        capability: native.map(|native| native.output.capability.clone()),
        incarnation_id: inputs.runtime_incarnation.map(str::to_owned),
        provider_incarnation: native.map(|native| native.provider_incarnation.clone()),
        ownership_sequence: native.map(|native| native.ownership_sequence),
        current_provider_incarnation: inputs
            .current_provider
            .map(|owner| owner.provider_incarnation.clone()),
        current_ownership_sequence: inputs
            .current_provider
            .map(|owner| owner.ownership_sequence),
        current_provider_source_id: inputs.current_provider.map(|owner| owner.source_id.clone()),
        observation_id: native.map(|native| native.claim.clone()),
        last_harness_output_at: output.map(|output| stamp(output.at_unix_ms)),
        output_entry_id: output.map(|output| output.entry_id.clone()),
        pending_inbound: inputs
            .pending
            .complete
            .then_some(inputs.pending.lower_bound),
        pending_inbound_lower_bound: inputs.pending.lower_bound,
        pending_inbound_complete: inputs.pending.complete,
        unresolved_tool_since: tool_since.map(stamp),
        tool_coverage_complete: native.is_some_and(|native| native.output.tool_coverage_complete),
        reachability: inputs.reachability.into(),
        harness_state: inputs.harness.map(|harness| harness.state.clone()),
    };
    let stable_since = native.map(|native| native.since_ms);
    let make = |status, reason, since| Health {
        status,
        reason,
        since: since.map(stamp),
        evidence: evidence.clone(),
    };
    let unknown = |reason, since| make(Status::Unknown, reason, since);
    if !inputs.runtime_running || inputs.runtime_incarnation.is_none_or(str::is_empty) {
        return unknown(Reason::RuntimeUnavailable, None);
    }
    let Some(harness) = inputs.harness else {
        return unknown(Reason::MissingNativeEvidence, None);
    };
    if inputs.runtime_incarnation != Some(harness.incarnation_id.as_str())
        || harness.driver.as_deref() != inputs.declared_driver
        || inputs.declared_driver.is_none()
    {
        return unknown(Reason::OwnershipMismatch, None);
    }
    // The anchor can be retained only when the native and current-owner images
    // agree at this cut. Human/stale precedence remains UNKNOWN, without carrying
    // a retired anchor through a missing or changed owner. Capture must still
    // independently establish the same namespace and admitted source authority.
    let ownership_bound = match (native, inputs.current_provider) {
        (Some(native), Some(owner)) => {
            !(inputs.namespace.is_empty()
                || owner.namespace != inputs.namespace
                || native.namespace != inputs.namespace
                || owner.captured_index != inputs.captured_index
                || native.captured_index != inputs.captured_index
                || native.claim.is_empty()
                || harness.claim.is_empty()
                || owner.source_id.is_empty()
                || owner.provider_incarnation.is_empty()
                || owner.ownership_sequence == 0
                || inputs.runtime_incarnation != Some(owner.runtime_incarnation.as_str())
                || inputs.declared_driver != Some(owner.driver.as_str())
                || owner.provider_incarnation != native.provider_incarnation
                || owner.ownership_sequence != native.ownership_sequence
                || owner.component != native.output.component
                || inputs.runtime_incarnation != Some(native.incarnation_id.as_str())
                || harness.transport.as_deref() != Some(native.output.component.as_str())
                || !inputs.declared_driver.is_some_and(|driver| {
                    native.output.owns(
                        driver,
                        &owner.provider_incarnation,
                        owner.ownership_sequence,
                    )
                }))
        }
        _ => false,
    };
    let harness_since =
        ownership_bound.then_some(harness.since_unix_ms.min(u128::from(u64::MAX)) as u64);
    let future_at = inputs.now_ms.saturating_add(FUTURE_SKEW_MS);
    if harness.observed_at_unix_ms > u128::from(future_at)
        || harness.since_unix_ms > u128::from(future_at)
    {
        return unknown(Reason::FutureEvidence, None);
    }
    if u128::from(inputs.now_ms).saturating_sub(harness.observed_at_unix_ms) >= u128::from(STALE_MS)
    {
        return unknown(
            Reason::StaleEvidence,
            ownership_bound.then_some(
                harness
                    .observed_at_unix_ms
                    .saturating_add(u128::from(STALE_MS))
                    .min(u128::from(u64::MAX)) as u64,
            ),
        );
    }
    if harness.blocked_on.as_deref() == Some("human")
        || matches!(
            harness.ask.as_deref(),
            Some("permission" | "question" | "review")
        )
    {
        return unknown(Reason::HumanBlocked, harness_since);
    }
    if !harness.is_ready()
        || harness.exit.is_some()
        || harness.blocked_on.is_some()
        || harness.ask.is_some()
    {
        return unknown(Reason::NativeUnavailable, harness_since);
    }
    let Some(native) = native else {
        return unknown(Reason::MissingNativeEvidence, None);
    };
    if native.claim.is_empty() || harness.claim.is_empty() {
        return unknown(Reason::MissingNativeEvidence, None);
    }
    if inputs.current_provider.is_none() {
        return unknown(Reason::MissingOwnershipEvidence, None);
    }
    if !ownership_bound {
        return unknown(Reason::OwnershipMismatch, None);
    }
    if native.observed_at_ms > inputs.now_ms.saturating_add(FUTURE_SKEW_MS)
        || native.since_ms > future_at
        || inputs
            .pending
            .oldest_at_ms
            .is_some_and(|since| since > future_at)
        || inputs
            .pending
            .continuous_since_ms
            .is_some_and(|since| since > future_at)
        || output
            .is_some_and(|output| output.at_unix_ms > inputs.now_ms.saturating_add(FUTURE_SKEW_MS))
        || native.output.progress_run_since_ms.is_some_and(|since| {
            since > inputs.now_ms.saturating_add(FUTURE_SKEW_MS)
                || output.is_none_or(|output| since > output.at_unix_ms)
        })
        || tool_since.is_some_and(|since| since > inputs.now_ms.saturating_add(FUTURE_SKEW_MS))
    {
        return unknown(Reason::FutureEvidence, None);
    }
    if inputs.now_ms.saturating_sub(native.observed_at_ms) >= STALE_MS {
        return unknown(
            Reason::StaleEvidence,
            Some(native.observed_at_ms.saturating_add(STALE_MS)),
        );
    }
    if inputs.mailbox_fault || !matches!(inputs.reachability, "local" | "reachable") {
        return unknown(Reason::TransportUnavailable, stable_since);
    }
    if let Some(since) = tool_since {
        return unknown(Reason::UnresolvedTool, Some(since));
    }
    if !native.output.tool_coverage_complete {
        return unknown(Reason::IncompleteToolCoverage, stable_since);
    }
    let Some(output) = output else {
        return unknown(Reason::MissingNativeOutput, stable_since);
    };
    if native.output.progress_run_since_ms.is_none() {
        return unknown(Reason::MissingProgressAnchor, stable_since);
    }
    let silence_at = output.at_unix_ms.saturating_add(SILENCE_MS);
    if inputs.now_ms < silence_at {
        return make(
            Status::Healthy,
            Reason::NativeOutputRecent,
            native.output.progress_run_since_ms,
        );
    }
    if !inputs.pending.complete {
        return unknown(Reason::IncompletePendingEvidence, None);
    }
    if inputs.pending.lower_bound > 0 {
        let Some(pending_since) = inputs.pending.continuous_since_ms else {
            return unknown(Reason::MissingPendingAnchor, None);
        };
        return make(
            Status::Suspect,
            Reason::OutputSilenceWithPendingInbound,
            Some(silence_at.max(pending_since)),
        );
    }
    unknown(Reason::NoRecentOutput, Some(silence_at))
}

#[cfg(test)]
mod tests {
    use super::super::harness_health_output_controls::{Output, Snapshot};
    use super::*;

    fn native(driver: &str) -> Native {
        let mut output = Snapshot::new(driver, "provider-a", 3).unwrap();
        output.tool_coverage_complete = true;
        output.last_output = Some(Output {
            at_unix_ms: 100,
            entry_id: "output-a".into(),
            sequence: 2,
            revision: 1,
        });
        output.progress_run_since_ms = Some(100);
        Native {
            namespace: "fixture".into(),
            captured_index: 44,
            incarnation_id: "runtime-a".into(),
            provider_incarnation: "provider-a".into(),
            ownership_sequence: 3,
            observed_at_ms: 600_000,
            since_ms: 10,
            claim: "native-a".into(),
            output,
        }
    }

    fn harness(native: &Native) -> CurrentHarnessView {
        CurrentHarnessView {
            state: "working".into(),
            driver: Some(native.output.driver.clone()),
            incarnation_id: native.incarnation_id.clone(),
            transport: Some(native.output.component.clone()),
            reason: None,
            blocked_on: None,
            ask: None,
            input_buffer: None,
            exit: None,
            claim: native.claim.clone(),
            observed_at_unix_ms: u128::from(native.observed_at_ms),
            since_unix_ms: u128::from(native.since_ms),
        }
    }

    fn selected_provider(driver: &str) -> SelectedProvider {
        SelectedProvider {
            namespace: "fixture".into(),
            captured_index: 44,
            runtime_incarnation: "runtime-a".into(),
            provider_incarnation: "provider-a".into(),
            ownership_sequence: 3,
            driver: driver.into(),
            component: super::super::harness_health_output_controls::component(driver)
                .unwrap()
                .into(),
            source_id: "current-owner-a".into(),
        }
    }

    fn inputs<'a>(
        native: &'a Native,
        harness: &'a CurrentHarnessView,
        owner: &'a SelectedProvider,
        now: u64,
    ) -> Inputs<'a> {
        Inputs {
            namespace: "fixture",
            captured_index: 44,
            now_ms: now,
            declared_driver: Some(&owner.driver),
            runtime_incarnation: Some("runtime-a"),
            runtime_running: true,
            reachability: "local",
            harness: Some(harness),
            current_provider: Some(owner),
            native: Some(native),
            pending: Pending {
                lower_bound: 80,
                complete: true,
                oldest_at_ms: Some(500),
                continuous_since_ms: Some(500),
            },
            mailbox_fault: false,
        }
    }

    #[test]
    fn each_harness_silence_with_backlog_is_stable_suspect_never_wedged() {
        for driver in ["codex", "claude", "pi", "omp", "opencode"] {
            let native = native(driver);
            let harness = harness(&native);
            let owner = selected_provider(driver);
            let first = reduce(inputs(&native, &harness, &owner, 600_100));
            assert_eq!(first.status, Status::Suspect, "{driver}");
            assert_ne!(first.status, Status::Wedged);
            assert_eq!(first, reduce(inputs(&native, &harness, &owner, 610_000)));
            let mut more_inbound = inputs(&native, &harness, &owner, 610_000);
            more_inbound.pending.lower_bound += 1;
            assert_eq!(reduce(more_inbound).since, first.since);
            assert_eq!(
                first.evidence.last_harness_output_at,
                Some(super::super::client_timestamp(100))
            );
        }
    }

    #[test]
    fn unknown_coverage_tools_and_owner_fences_outrank_recent_output() {
        let mut native = native("codex");
        native.observed_at_ms = 100;
        let harness = harness(&native);
        let owner = selected_provider(&harness.driver.clone().unwrap());
        assert_eq!(
            reduce(inputs(&native, &harness, &owner, 200)).status,
            Status::Healthy
        );
        native.output.unresolved_tools.insert("call-a".into(), 150);
        assert_eq!(
            reduce(inputs(&native, &harness, &owner, 200)).reason,
            Reason::UnresolvedTool
        );
        native.output.unresolved_tools.clear();
        native.output.gap();
        assert_eq!(
            reduce(inputs(&native, &harness, &owner, 200)).reason,
            Reason::IncompleteToolCoverage
        );
        native.output.tool_coverage_complete = true;
        native.ownership_sequence += 1;
        assert_eq!(
            reduce(inputs(&native, &harness, &owner, 200)).reason,
            Reason::OwnershipMismatch
        );
    }

    #[test]
    fn stale_future_missing_and_incomplete_pending_inputs_fail_closed() {
        let mut native = native("omp");
        let mut harness = harness(&native);
        let owner = selected_provider(&harness.driver.clone().unwrap());
        let stale = reduce(inputs(&native, &harness, &owner, 1_500_000));
        assert_eq!(stale.reason, Reason::StaleEvidence);
        assert_eq!(
            stale.since,
            reduce(inputs(&native, &harness, &owner, 1_600_000)).since
        );
        native.observed_at_ms = 100_000;
        harness.observed_at_unix_ms = 100_000;
        assert_eq!(
            reduce(inputs(&native, &harness, &owner, 200)).reason,
            Reason::FutureEvidence
        );
        native.observed_at_ms = 200;
        harness.observed_at_unix_ms = 200;
        let mut partial = inputs(&native, &harness, &owner, 600_100);
        partial.pending = Pending {
            lower_bound: 0,
            complete: false,
            oldest_at_ms: None,
            continuous_since_ms: None,
        };
        let health = reduce(partial);
        assert_eq!(health.status, Status::Unknown);
        assert_eq!(health.evidence.pending_inbound, None);
        assert!(!health.evidence.pending_inbound_complete);
    }

    #[test]
    fn missing_terminal_and_human_native_state_cannot_become_healthy() {
        let mut native = native("codex");
        native.observed_at_ms = 100;
        let mut harness = harness(&native);
        let owner = selected_provider(&harness.driver.clone().unwrap());
        let mut missing = inputs(&native, &harness, &owner, 200);
        missing.harness = None;
        assert_eq!(reduce(missing).reason, Reason::MissingNativeEvidence);
        harness.state = "ended".into();
        assert_eq!(
            reduce(inputs(&native, &harness, &owner, 200)).reason,
            Reason::NativeUnavailable
        );
        harness.state = "working".into();
        harness.exit = Some("completed".into());
        assert_eq!(
            reduce(inputs(&native, &harness, &owner, 200)).reason,
            Reason::NativeUnavailable
        );
        harness.exit = None;
        harness.blocked_on = Some("human".into());
        harness.ask = Some("permission".into());
        let blocked = reduce(inputs(&native, &harness, &owner, 200));
        assert_eq!(blocked.reason, Reason::HumanBlocked);
        assert_eq!(
            blocked.since,
            reduce(inputs(&native, &harness, &owner, 300)).since
        );
        harness.incarnation_id = "old-runtime".into();
        let old_owner = reduce(inputs(&native, &harness, &owner, 200));
        assert_eq!(old_owner.reason, Reason::OwnershipMismatch);
        assert_eq!(old_owner.since, None);
    }

    #[test]
    fn missing_progress_anchor_and_future_backlog_fail_closed() {
        let mut native = native("omp");
        native.observed_at_ms = 100;
        let harness = harness(&native);
        let owner = selected_provider(&harness.driver.clone().unwrap());
        native.output.progress_run_since_ms = None;
        assert_eq!(
            reduce(inputs(&native, &harness, &owner, 200)).reason,
            Reason::MissingProgressAnchor
        );
        native.output.progress_run_since_ms = Some(100);
        let mut future = inputs(&native, &harness, &owner, 200);
        future.pending.oldest_at_ms = Some(100_000);
        assert_eq!(reduce(future).reason, Reason::FutureEvidence);
    }

    #[test]
    fn a_consistent_retired_snapshot_cannot_self_select_current_ownership() {
        let mut native = native("codex");
        native.observed_at_ms = 100;
        let harness = harness(&native);
        let mut owner = selected_provider("codex");
        assert!(native.output.owns(
            "codex",
            &native.provider_incarnation,
            native.ownership_sequence
        ));
        assert_eq!(
            reduce(inputs(&native, &harness, &owner, 200)).status,
            Status::Healthy
        );

        // The runtime survives a provider re-exec. Both retired structures still
        // agree, so only independently captured current ownership can reject them.
        owner.provider_incarnation = "provider-b".into();
        owner.source_id = "current-owner-b".into();
        let replaced = reduce(inputs(&native, &harness, &owner, 200));
        assert_eq!(replaced.reason, Reason::OwnershipMismatch);
        assert_eq!(replaced.since, None);
        assert_eq!(
            replaced.evidence.provider_incarnation.as_deref(),
            Some("provider-a")
        );
        assert_eq!(
            replaced.evidence.current_provider_incarnation.as_deref(),
            Some("provider-b")
        );

        owner = selected_provider("codex");
        owner.ownership_sequence += 1;
        let reclaimed = reduce(inputs(&native, &harness, &owner, 200));
        assert_eq!(reclaimed.reason, Reason::OwnershipMismatch);
        assert_eq!(reclaimed.since, None);
        assert_eq!(reclaimed.evidence.current_ownership_sequence, Some(4));
    }

    #[test]
    fn absent_or_cross_cut_owner_evidence_cannot_support_positive_health() {
        let mut native = native("omp");
        native.observed_at_ms = 100;
        let harness = harness(&native);
        let mut owner = selected_provider("omp");
        let mut missing = inputs(&native, &harness, &owner, 200);
        missing.current_provider = None;
        assert_eq!(reduce(missing).reason, Reason::MissingOwnershipEvidence);
        owner.captured_index = 43;
        assert_eq!(
            reduce(inputs(&native, &harness, &owner, 200)).reason,
            Reason::OwnershipMismatch
        );
        owner.captured_index = 44;
        native.captured_index = 43;
        assert_eq!(
            reduce(inputs(&native, &harness, &owner, 200)).reason,
            Reason::OwnershipMismatch
        );
        native.captured_index = 44;
        owner.source_id.clear();
        assert_eq!(
            reduce(inputs(&native, &harness, &owner, 200)).reason,
            Reason::OwnershipMismatch
        );
    }

    #[test]
    fn missing_or_changed_owner_cannot_retain_a_human_or_stale_anchor() {
        let mut native = native("codex");
        native.observed_at_ms = 100;
        let mut harness = harness(&native);
        let mut owner = selected_provider("codex");
        harness.blocked_on = Some("human".into());
        harness.ask = Some("permission".into());
        let current = reduce(inputs(&native, &harness, &owner, 200));
        assert_eq!(current.reason, Reason::HumanBlocked);
        assert!(current.since.is_some());
        let mut absent = inputs(&native, &harness, &owner, 200);
        absent.current_provider = None;
        let absent = reduce(absent);
        assert_eq!(absent.reason, Reason::HumanBlocked);
        assert_eq!(absent.since, None);
        owner.provider_incarnation = "provider-b".into();
        let retired = reduce(inputs(&native, &harness, &owner, 200));
        assert_eq!(retired.status, Status::Unknown);
        assert_eq!(retired.reason, Reason::HumanBlocked);
        assert_eq!(retired.since, None);
        harness.blocked_on = None;
        harness.ask = None;
        let stale = reduce(inputs(&native, &harness, &owner, 1_000_000));
        assert_eq!(stale.reason, Reason::StaleEvidence);
        assert_eq!(stale.since, None);
        owner = selected_provider("codex");
        let stale = reduce(inputs(&native, &harness, &owner, 1_000_000));
        assert_eq!(stale.reason, Reason::StaleEvidence);
        assert!(stale.since.is_some());
        owner.ownership_sequence += 1;
        let reclaimed = reduce(inputs(&native, &harness, &owner, 1_000_000));
        assert_eq!(reclaimed.reason, Reason::StaleEvidence);
        assert_eq!(reclaimed.since, None);
    }

    #[test]
    fn oldest_candidate_cannot_supply_a_continuous_suspect_anchor() {
        let native = native("omp");
        let harness = harness(&native);
        let owner = selected_provider("omp");
        let mut absent = inputs(&native, &harness, &owner, 600_100);
        absent.pending.continuous_since_ms = None;
        let absent = reduce(absent);
        assert_eq!(absent.status, Status::Unknown);
        assert_eq!(absent.reason, Reason::MissingPendingAnchor);
        assert_eq!(absent.since, None);

        let mut incomplete = inputs(&native, &harness, &owner, 600_100);
        incomplete.pending.complete = false;
        let incomplete = reduce(incomplete);
        assert_eq!(incomplete.reason, Reason::IncompletePendingEvidence);
        assert_eq!(incomplete.since, None);

        let mut retained = inputs(&native, &harness, &owner, 600_100);
        retained.pending.continuous_since_ms = Some(600_050);
        retained.pending.oldest_at_ms = Some(600_075);
        let first = reduce(retained);
        assert_eq!(first.status, Status::Suspect);
        assert_eq!(first.since, Some(super::super::client_timestamp(600_100)));
        let mut changed_oldest = inputs(&native, &harness, &owner, 610_000);
        changed_oldest.pending.continuous_since_ms = Some(600_050);
        changed_oldest.pending.oldest_at_ms = Some(609_000);
        assert_eq!(reduce(changed_oldest).since, first.since);
    }

    #[test]
    fn an_invalid_future_pending_anchor_is_not_retained_as_health_since() {
        let native = native("omp");
        let harness = harness(&native);
        let owner = selected_provider("omp");
        let mut future = inputs(&native, &harness, &owner, 600_100);
        future.pending.continuous_since_ms = Some(700_000);
        let refused = reduce(future);
        assert_eq!(refused.reason, Reason::FutureEvidence);
        assert_eq!(refused.since, None);
    }

    #[test]
    fn matching_public_tuples_in_another_namespace_do_not_supply_an_anchor() {
        let mut native = native("codex");
        native.observed_at_ms = 100;
        let mut harness = harness(&native);
        let mut owner = selected_provider("codex");
        assert_eq!(
            reduce(inputs(&native, &harness, &owner, 200)).status,
            Status::Healthy
        );
        owner.namespace = "other".into();
        native.namespace = "other".into();
        let crossed = reduce(inputs(&native, &harness, &owner, 200));
        assert_eq!(crossed.reason, Reason::OwnershipMismatch);
        assert_eq!(crossed.since, None);
        harness.blocked_on = Some("human".into());
        let crossed = reduce(inputs(&native, &harness, &owner, 200));
        assert_eq!(crossed.reason, Reason::HumanBlocked);
        assert_eq!(crossed.since, None);
    }

    #[test]
    fn expired_immutable_inputs_recompute_at_the_exact_validity_boundary() {
        let native = native("codex");
        let harness = harness(&native);
        let owner = selected_provider("codex");
        let before = inputs(&native, &harness, &owner, 600_099);
        assert_eq!(valid_until(&before), Some(600_100));
        assert_eq!(reduce(before).status, Status::Healthy);
        let silence = reduce(inputs(&native, &harness, &owner, 600_100));
        assert_eq!(silence.status, Status::Suspect);
        assert_eq!(silence.reason, Reason::OutputSilenceWithPendingInbound);
        let stale = reduce(inputs(&native, &harness, &owner, 1_500_000));
        assert_eq!(stale.reason, Reason::StaleEvidence);
        assert_eq!(
            valid_until(&inputs(&native, &harness, &owner, 1_500_000)),
            None
        );
    }

    #[test]
    fn immutable_health_inputs_expire_at_output_then_source_boundaries() {
        let mut native = native("codex");
        let harness = harness(&native);
        let owner = selected_provider("codex");
        assert_eq!(
            valid_until(&inputs(&native, &harness, &owner, 600_099)),
            Some(600_100)
        );
        assert_eq!(
            valid_until(&inputs(&native, &harness, &owner, 600_100)),
            Some(1_500_000)
        );
        assert_eq!(
            valid_until(&inputs(&native, &harness, &owner, 1_500_000)),
            None
        );
        native.output.gap();
        assert_eq!(
            valid_until(&inputs(&native, &harness, &owner, 1_500_000)),
            None
        );
    }

    #[test]
    fn future_input_acceptance_is_also_a_time_only_boundary() {
        let mut native = native("omp");
        native.observed_at_ms = 100_000;
        let harness = harness(&native);
        let owner = selected_provider("omp");
        let before = inputs(&native, &harness, &owner, 200);
        assert_eq!(valid_until(&before), Some(40_000));
        assert_eq!(reduce(before).reason, Reason::FutureEvidence);
        assert_eq!(
            reduce(inputs(&native, &harness, &owner, 40_000)).status,
            Status::Healthy
        );
    }
}
