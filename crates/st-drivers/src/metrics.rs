//! RED-minimal metric set for st2, per the interview Q5 decision recorded in
//! `docs/vrs/06-observability/open-questions.md`.
//!
//! Every label value comes from a bounded enum (`result`, `driver`, hook registry name +
//! normalized event); identifiers never become metric labels — those live in span attributes.
//!
//! Zero-overhead no-op unless a real meter provider is installed by
//! [`crate::telemetry::Telemetry::init`]: every record function checks [`enabled`] first and
//! returns before touching any instrument or allocating a label string. With no provider
//! installed, `opentelemetry::global` hands out a silent no-op meter anyway — this early-out
//! just keeps the disabled case allocation-free.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, Meter};

use crate::driver_diagnostic::{Driver, Reason, Source, Stage, Support};
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether a real meter provider is installed. False → recording is a free no-op.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Flip recording on when [`crate::telemetry::Telemetry::init`] installs the provider.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter("st2"));

static RECONCILE_PASSES: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("reconcile_passes_total")
        .with_description("Reconcile passes by outcome")
        .with_unit("1")
        .build()
});
static RECONCILE_PASS_DURATION: LazyLock<Histogram<f64>> = LazyLock::new(|| {
    METER
        .f64_histogram("reconcile_pass_duration_seconds")
        .with_description("Wall-clock duration of one reconcile pass")
        .with_unit("s")
        .build()
});
static SESSION_START_DURATION: LazyLock<Histogram<f64>> = LazyLock::new(|| {
    METER
        .f64_histogram("session_start_duration_seconds")
        .with_description("Latency of one task session spawn")
        .with_unit("s")
        .build()
});
static TASK_LAUNCHES: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("task_launches_total")
        .with_description("Task sessions launched, by driver")
        .with_unit("1")
        .build()
});
static TASK_REAPS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("task_reaps_total")
        .with_description("Dead sessions reaped for restart, by driver")
        .with_unit("1")
        .build()
});
static HOOK_INVOCATIONS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("hook_invocations_total")
        .with_description("Lifecycle hook invocations applied in-process, by hook and event")
        .with_unit("1")
        .build()
});
static MESSAGE_DELIVERIES: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("message_deliveries_total")
        .with_description("Bus deliveries onto a recipient inbox, by outcome")
        .with_unit("1")
        .build()
});
static CRASH_LOOPS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("crash_loops_total")
        .with_description("Tasks parked as crash-looping past their restart budget")
        .with_unit("1")
        .build()
});
static DRIVER_DIAGNOSTICS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("driver_diagnostic_transitions_total")
        .with_description("Native driver diagnostic failures and recoveries by bounded boundary")
        .with_unit("1")
        .build()
});
static RESOURCE_OBSERVE_REQUESTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("resource_observe_requests_total")
        .with_description("Demand-observation request lifecycle transitions by bounded outcome")
        .with_unit("1")
        .build()
});
static RESOURCE_OBSERVE_DISPATCH: LazyLock<Histogram<f64>> = LazyLock::new(|| {
    METER
        .f64_histogram("resource_observe_dispatch_seconds")
        .with_description("Latency from request publication to provider-runtime dispatch")
        .with_unit("s")
        .build()
});
static RESOURCE_OBSERVE_SETTLE: LazyLock<Histogram<f64>> = LazyLock::new(|| {
    METER
        .f64_histogram("resource_observe_settle_seconds")
        .with_description("Latency from provider-runtime dispatch to durable result receipt")
        .with_unit("s")
        .build()
});

/// One reconcile pass finished. `failed` = the pass collected errors.
pub fn record_reconcile_pass(duration: Duration, failed: bool) {
    if !enabled() {
        return;
    }
    RECONCILE_PASS_DURATION.record(duration.as_secs_f64(), &[]);
    RECONCILE_PASSES.add(
        1,
        &[opentelemetry::KeyValue::new(
            "result",
            if failed { "fail" } else { "pass" },
        )],
    );
}

/// One task session spawn succeeded.
pub fn record_session_start(duration: Duration, driver: &'static str) {
    if !enabled() {
        return;
    }
    SESSION_START_DURATION.record(duration.as_secs_f64(), &[]);
    TASK_LAUNCHES.add(1, &[opentelemetry::KeyValue::new("driver", driver)]);
}

/// One dead session was reaped so its replacement can start.
pub fn record_task_reap(driver: &'static str) {
    if !enabled() {
        return;
    }
    TASK_REAPS.add(1, &[opentelemetry::KeyValue::new("driver", driver)]);
}

/// One lifecycle-hook invocation reached its single in-process application point.
/// Unknown event names collapse to `other` so the label stays bounded.
pub fn record_hook_invocation(hook: &'static str, event: &str) {
    if !enabled() {
        return;
    }
    HOOK_INVOCATIONS.add(
        1,
        &[
            opentelemetry::KeyValue::new("hook", hook),
            opentelemetry::KeyValue::new("event", normalize_hook_event(event)),
        ],
    );
}

/// One bus delivery attempt onto a recipient inbox finished. `failed` = the attempt errored.
pub fn record_message_delivery(failed: bool) {
    if !enabled() {
        return;
    }
    MESSAGE_DELIVERIES.add(
        1,
        &[opentelemetry::KeyValue::new(
            "result",
            if failed { "fail" } else { "pass" },
        )],
    );
}

/// A task was parked as crash-looping past its restart budget.
pub fn record_crash_loop() {
    if !enabled() {
        return;
    }
    CRASH_LOOPS.add(1, &[]);
}
/// One native-driver boundary entered failure or recovered. Every label is a closed enum; raw
/// versions and agent/session/message identity stay on spans and logs.
pub fn record_driver_diagnostic(
    driver: Driver,
    stage: Stage,
    reason: Reason,
    source: Source,
    support: Support,
    recovered: bool,
) {
    if !enabled() {
        return;
    }
    DRIVER_DIAGNOSTICS.add(
        1,
        &driver_diagnostic_attributes(driver, stage, reason, source, support, recovered),
    );
}
pub fn record_resource_observe_request(outcome: &'static str) {
    if !enabled() {
        return;
    }
    RESOURCE_OBSERVE_REQUESTS.add(
        1,
        &[opentelemetry::KeyValue::new(
            "outcome",
            normalize_resource_observe_outcome(outcome),
        )],
    );
}

pub fn record_resource_observe_dispatch(duration: Duration) {
    if !enabled() {
        return;
    }
    RESOURCE_OBSERVE_DISPATCH.record(duration.as_secs_f64(), &[]);
}

pub fn record_resource_observe_settle(duration: Duration) {
    if !enabled() {
        return;
    }
    RESOURCE_OBSERVE_SETTLE.record(duration.as_secs_f64(), &[]);
}

fn driver_diagnostic_attributes(
    driver: Driver,
    stage: Stage,
    reason: Reason,
    source: Source,
    support: Support,
    recovered: bool,
) -> [opentelemetry::KeyValue; 6] {
    [
        opentelemetry::KeyValue::new("driver", driver.as_str()),
        opentelemetry::KeyValue::new("stage", stage.as_str()),
        opentelemetry::KeyValue::new("reason", reason.as_str()),
        opentelemetry::KeyValue::new("source", source.as_str()),
        opentelemetry::KeyValue::new("support", support.as_str()),
        opentelemetry::KeyValue::new("outcome", if recovered { "recovery" } else { "failure" }),
    ]
}
fn normalize_resource_observe_outcome(outcome: &str) -> &'static str {
    match outcome {
        "accepted" => "accepted",
        "backpressured" => "backpressured",
        "settledUnchanged" => "settledUnchanged",
        "settledChanged" => "settledChanged",
        "settledFailed" => "settledFailed",
        "absentBinding" => "absentBinding",
        "staleGeneration" => "staleGeneration",
        "providerUnavailable" => "providerUnavailable",
        _ => "other",
    }
}

/// The bounded Claude hook-event vocabulary st2 applies; anything else is `other`.
fn normalize_hook_event(event: &str) -> &'static str {
    match event {
        "SessionStart" => "SessionStart",
        "UserPromptSubmit" => "UserPromptSubmit",
        "PreToolUse" => "PreToolUse",
        "PostToolUse" => "PostToolUse",
        "PermissionRequest" => "PermissionRequest",
        "Stop" => "Stop",
        "StopFailure" => "StopFailure",
        "SubagentStop" => "SubagentStop",
        "PreCompact" => "PreCompact",
        "PostCompact" => "PostCompact",
        "Notification" => "Notification",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default_and_recording_is_a_no_op() {
        // No meter provider installed in unit tests: enabled() is false and every record call
        // must return without panicking or touching instruments.
        assert!(!enabled());
        set_enabled(false);
        record_reconcile_pass(Duration::from_millis(5), false);
        record_session_start(Duration::from_millis(5), "exec");
        record_task_reap("codex");
        record_hook_invocation("claude-observe", "SomethingUnheardOf");
        record_message_delivery(true);
        record_crash_loop();
        record_resource_observe_request("accepted");
        record_resource_observe_dispatch(Duration::from_millis(1));
        record_resource_observe_settle(Duration::from_millis(1));
        assert!(!enabled());

        record_driver_diagnostic(
            Driver::OpenCode,
            Stage::Seed,
            Reason::UnknownStatus,
            Source::StatusSnapshot,
            Support::Supported,
            false,
        );
    }

    #[test]
    fn unknown_hook_events_collapse_to_other() {
        assert_eq!(normalize_hook_event("SessionStart"), "SessionStart");
        assert_eq!(normalize_hook_event("TotallyNewEvent"), "other");
    }
    #[test]
    fn resource_observe_outcomes_use_the_bounded_wire_vocabulary() {
        assert_eq!(
            normalize_resource_observe_outcome("settledUnchanged"),
            "settledUnchanged"
        );
        assert_eq!(
            normalize_resource_observe_outcome("settled-unchanged"),
            "other"
        );
        assert_eq!(normalize_resource_observe_outcome("h.worker"), "other");
    }

    #[test]
    fn driver_diagnostic_metric_attributes_are_exactly_the_bounded_axes() {
        let attributes = driver_diagnostic_attributes(
            Driver::Claude,
            Stage::ProviderAuth,
            Reason::ProviderAuthRejected,
            Source::TurnResult,
            Support::Unknown,
            true,
        );
        let keys: Vec<&str> = attributes
            .iter()
            .map(|attribute| attribute.key.as_str())
            .collect();
        assert_eq!(
            keys,
            ["driver", "stage", "reason", "source", "support", "outcome"]
        );
        let rendered = format!("{attributes:?}");
        for forbidden in ["1.18.19", "h.worker", "ses_", "msg_"] {
            assert!(!rendered.contains(forbidden), "{rendered}");
        }
    }
}
