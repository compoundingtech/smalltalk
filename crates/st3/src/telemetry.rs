//! st's harness telemetry entrypoints. Hooks never contact an OpenTelemetry collector:
//! they hand bounded signals to the local daemon, whose observation exporter owns retries.

use std::collections::BTreeMap;
use std::io::IsTerminal as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

use crate::driver_hook::HookEnv;
use crate::model::ClaimInput;

pub const KIND: &str = "harness.telemetry";
/// This bounds daemon admission, independently of collector availability or outage retries.
pub const HOOK_SUBMIT_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_LOGS: usize = 16;

fn stderr_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
}

/// A driver or timer-driven status line has local diagnostics and no per-process exporters.
/// Driver state/diagnostic records already reach the daemon through its observation path.
pub fn local_only() {
    let _ = tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(std::io::stderr().is_terminal())
                .with_writer(std::io::stderr)
                .with_filter(stderr_filter()),
        ),
    );
}

#[derive(Clone, Default)]
struct WarningCapture(Arc<Mutex<Vec<Value>>>);

impl<S: tracing::Subscriber> Layer<S> for WarningCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
        let severity = match *event.metadata().level() {
            tracing::Level::WARN => "WARN",
            tracing::Level::ERROR => "ERROR",
            _ => return,
        };
        let mut logs = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if logs.len() >= MAX_LOGS {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        logs.push(json!({
            "severity": severity,
            "target": event.metadata().target(),
            "fields": fields.0,
        }));
    }
}

#[derive(Default)]
struct Fields(BTreeMap<String, Value>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if self.0.len() < 16 {
            let text: String = format!("{value:?}").chars().take(2048).collect();
            self.0.insert(field.name().into(), Value::String(text));
        }
    }
}

/// Run the event hook unchanged, collecting the invocation only at its actual application
/// point. Stderr filtering cannot silence the warning capture. Telemetry cannot change its exit.
pub fn hook(env: &dyn HookEnv, run: impl FnOnce() -> u8) -> u8 {
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let clock = Instant::now();
    let capture = WarningCapture::default();
    let subscriber = tracing_subscriber::registry()
        .with(
            capture
                .clone()
                .with_filter(tracing_subscriber::filter::LevelFilter::WARN),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(std::io::stderr().is_terminal())
                .with_writer(std::io::stderr)
                .with_filter(stderr_filter()),
        );
    let (code, invocations) = tracing::subscriber::with_default(subscriber, || {
        st_drivers::metrics::capture_hook_invocations(run)
    });
    let logs = capture
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if invocations.is_empty() && logs.is_empty() && code == 0 {
        return code;
    }
    let subject = env
        .var("ST3_SUBJECT")
        .or_else(|| env.var("ST_AGENT"))
        .filter(|subject| subject.starts_with("agent/"));
    let incarnation =
        st_drivers::contracts::env_with(st_drivers::claude_session::SESSION_ENV, &|name| {
            env.raw_var(name)
        });
    if let (Some(subject), Some(incarnation)) = (subject, incarnation) {
        let claim = ClaimInput {
            subject: subject.clone(),
            kind: KIND.into(),
            actor: Some(subject),
            fields: BTreeMap::from([
                ("driver".into(), json!("claude")),
                ("unit".into(), json!("hook")),
                ("incarnation_id".into(), json!(incarnation)),
                (
                    "signals".into(),
                    json!({
                        "started_at_unix_nano": started.to_string(),
                        "ended_at_unix_nano": (started + clock.elapsed().as_nanos()).to_string(),
                        "exit_code": code,
                        "hook_invocations": invocations.iter().map(|invocation| json!({
                            "hook": invocation.hook, "event": invocation.event,
                        })).collect::<Vec<_>>(),
                        "logs": logs,
                    }),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("hook-telemetry:{}", uuid::Uuid::now_v7())),
        };
        // Best effort, as the retired exporter was. Binding failures use their existing
        // diagnostic operation; a missing/busy daemon here never holds up or fails the hook.
        let _ = submit(env, &claim);
    }
    code
}

fn submit(env: &dyn HookEnv, claim: &ClaimInput) -> anyhow::Result<()> {
    let endpoint = match env.var("ST3_ENDPOINT") {
        Some(endpoint) => crate::client::Endpoint::parse(endpoint),
        None => {
            crate::client::Endpoint::Unix(crate::config::Config::load_unvalidated(None)?.socket)
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let client = crate::client::Client::new(endpoint);
        tokio::time::timeout(
            HOOK_SUBMIT_TIMEOUT,
            client.post::<_, Value>("/v1/claims", claim),
        )
        .await??;
        Ok(())
    })
}
