//! Controlled pi launch with a session-owned presence lease.
//!
//! pi has no MCP and no app-server: its integration point is an extension loaded into the
//! interactive process, and that extension reaches st2 by spawning `st2 driver pi-channel`. The
//! launch body itself is shared with omp — the family's other extension-driven harness — in
//! [`crate::pi_family_session`]; what lives here is what is genuinely pi's: its extension asset,
//! its channel variable names, and the release its harness-context arithmetic was measured on.

use std::path::Path;

use anyhow::Result;

use crate::pi_family_session::{self, HarnessKind};

/// The extension file inside this binary's immutable hook set.
const EXTENSION: &str = "pi-channel.ts";

/// The pi release the harness-context producer's arithmetic was measured against (HC-R13).
///
/// pi has no runtime version gate — unlike omp, whose wrapper refuses an unadmitted minor — so the
/// only place this repository couples itself to a pi build is `flake.nix`'s extension check, which
/// type-checks and runtime-smokes the shipped asset against exactly this tarball. That makes the
/// flake pin the gate for this constant, and `pi_channel`'s fixture asserts the two agree: a pi
/// bump that changes what `getContextUsage().tokens` means must move both together or fail.
pub const MEASURED_CONTEXT_VERSION: &str = "0.84.2";

/// The exact st2 executable the pi extension must spawn for its channel.
pub const CHANNEL_BIN: &str = "ST_PI_CHANNEL_BIN";
/// The catalog root that executable must be pointed at.
pub const CHANNEL_CATALOG: &str = "ST_PI_CHANNEL_CATALOG";
/// The host-qualified bus identity the channel binds.
pub const CHANNEL_IDENTITY: &str = "ST_PI_CHANNEL_IDENTITY";
/// The wrapper's runtime/task ID — the pty session whose liveness vouches for observed state.
pub const CHANNEL_RUNTIME_ID: &str = "ST_PI_CHANNEL_RUNTIME_ID";
/// The session incarnation token the wrapper mints. The channel adopts it so the wrapper's
/// terminal record owns — and thereby fences — the live records the channel writes.
pub const CHANNEL_SESSION: &str = "ST_PI_CHANNEL_SESSION";
/// The ownership sequence the wrapper claimed at startup — exported beside the token so the
/// channel's writes act under the same directional claim.
pub const CHANNEL_SEQ: &str = "ST_PI_CHANNEL_SEQ";

/// pi's half of the pi-family launch fork. No `verify_version`: pi gates its build in
/// `flake.nix`'s extension check rather than at launch.
pub(crate) const PI_KIND: HarnessKind = HarnessKind {
    label: "pi",
    extension: EXTENSION,
    bin_env: CHANNEL_BIN,
    catalog_env: CHANNEL_CATALOG,
    identity_env: CHANNEL_IDENTITY,
    runtime_id_env: CHANNEL_RUNTIME_ID,
    session_env: CHANNEL_SESSION,
    seq_env: CHANNEL_SEQ,
    verify_version: None,
};

/// Run one interactive pi provider and maintain its presence until it exits.
pub fn run(
    catalog_root: &Path,
    identity: String,
    runtime_id: String,
    pi_argv: Vec<String>,
) -> Result<()> {
    pi_family_session::run_for(catalog_root, identity, runtime_id, pi_argv, &PI_KIND)
}

/// Resume supervising a pi provider a predecessor driver image launched and released.
pub fn adopt(
    catalog_root: &Path,
    identity: String,
    runtime_id: String,
    pid: u32,
    session: String,
    seq: u64,
) -> Result<()> {
    pi_family_session::adopt_for(
        catalog_root,
        identity,
        runtime_id,
        &PI_KIND,
        pid,
        session,
        seq,
        true,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use crate::provider_session::{ProviderOutcome, run_provider};
    use crate::status;

    #[test]
    fn idle_pi_provider_refreshes_presence_without_channel_input() {
        let tmp = tempfile::tempdir().unwrap();
        let presence = status::status_path(tmp.path());
        status::set_state(&presence, status::State::Available).unwrap();
        let before = fs::read_to_string(&presence).unwrap();
        let stop = AtomicBool::new(false);

        run_provider(
            "pi",
            Some(&presence),
            // Deliberately cheaper than the Claude twin: this binary already runs one
            // spawn-and-poll presence test, and the codex app-server tests beside it are
            // timing-sensitive. One refresh inside the child's life is all this needs to prove.
            &["sh".into(), "-c".into(), "sleep 0.06".into()],
            &[],
            Duration::from_millis(10),
            Duration::from_millis(5),
            &stop,
            None,
        )
        .unwrap();

        let after = fs::read_to_string(&presence).unwrap();
        assert_ne!(after, before);
        assert_eq!(status::read_state(&presence), status::State::Available);
    }

    /// The observed variant reports a nonzero exit instead of judging it, which is what lets the
    /// wrapper record the terminal state before failing the launch.
    #[test]
    fn run_provider_observed_reports_the_child_exit_status() {
        let tmp = tempfile::tempdir().unwrap();
        let stop = AtomicBool::new(false);
        let outcome = crate::provider_session::run_provider_observed(
            "pi",
            Some(&status::status_path(tmp.path())),
            &["sh".into(), "-c".into(), "exit 3".into()],
            &[],
            Duration::from_secs(60),
            Duration::from_millis(5),
            &stop,
            None,
        )
        .unwrap();
        match outcome {
            ProviderOutcome::Exited(exit) => assert_eq!(exit.code(), Some(3)),
            other => panic!("expected an exit outcome, got {other:?}"),
        }
    }

    /// W6: the terminal-only observer records how the session ended but never re-stamps live
    /// state — the channel owns the heartbeat — and its token makes the write this session's.
    #[test]
    fn the_terminal_only_observer_ends_but_never_heartbeats() {
        use crate::harness_state::{self, Activity};
        let tmp = tempfile::tempdir().unwrap();
        let record = harness_state::harness_state_path(tmp.path());
        let session = harness_state::session_token();
        // Real wiring order: the wrapper's written claim first, then the channel adopts it.
        let seq = harness_state::claim(tmp.path(), "h.worker", "pi", &session).unwrap();
        let mut channel =
            harness_state::Writer::new(tmp.path(), "h.worker", "pi", Some("h.worker".to_string()))
                .with_ownership(session.clone(), seq);
        channel
            .observe(harness_state::Observation::new(
                Activity::Active,
                harness_state::BlockedOn::None,
                harness_state::InputBuffer::Unknown,
            ))
            .unwrap();
        let live = std::fs::read(&record).unwrap();

        let observer = crate::provider_session::SessionObserver::terminal_only(
            tmp.path(),
            "h.worker",
            "pi",
            "h.worker",
            &session,
            seq,
        );
        observer.heartbeat();
        assert_eq!(std::fs::read(&record).unwrap(), live, "no heartbeat");

        observer.ended("signal 9");
        let observed = harness_state::read(&record, None).unwrap();
        assert_eq!(observed.state, Activity::Ended);
        assert_eq!(observed.exit.as_deref(), Some("signal 9"));
    }
}

/// Launch a native graph-owned session without legacy presence transport.
pub fn run_native(
    paths: &crate::driver_paths::Paths,
    identity: String,
    runtime_id: String,
    argv: Vec<String>,
) -> Result<()> {
    let environment = paths.environment(&identity);
    pi_family_session::run_for_paths(
        &paths.root,
        &paths.agent_dir,
        identity,
        runtime_id,
        argv,
        &PI_KIND,
        &environment,
        &[],
        None,
        false,
    )
}

/// Adopt a native graph-owned session without legacy presence transport.
#[allow(clippy::too_many_arguments)]
pub fn adopt_native(
    paths: &crate::driver_paths::Paths,
    identity: String,
    runtime_id: String,
    pid: u32,
    session: String,
    seq: u64,
) -> Result<()> {
    pi_family_session::adopt_paths(
        &paths.agent_dir,
        identity,
        runtime_id,
        &PI_KIND,
        pid,
        session,
        seq,
        false,
    )
}
