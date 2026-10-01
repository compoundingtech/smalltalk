//! `st driver-hook NAME [ARGS]`: the st3 binary answering a hook its own seat's harness runs.
//!
//! Every script in st3's hook set execs this, hidden from `--help`, with the harness's payload on
//! stdin. The arguments and environment are the ones the seat's driver exports: the event name in
//! the first argument, `CATALOG` (else `ST_ROOT`) for the driver's private catalog,
//! `ST2_CLAUDE_IDENTITY` (else `ST_AGENT`), and the `ST2_CLAUDE_*` wrapper-session variables.
//! Behaviour comes from the same library functions st2's CLI used, so no `st2` program runs.
//!
//! Observation fails open, since a hook the harness waits on must not stop it. One case is loud:
//! a SessionStart that leaves no native-session binding for the current wrapper session, because
//! without it st cannot find the seat's transcript. That hook exits non-zero, prints why, and
//! records a `harness.diagnostic` claim. A mandatory resume binding failure also exits non-zero,
//! as it always has.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde_json::Value;

/// Active hooks this subcommand answers. New hook sets hold one script per name.
pub const HOOKS: [&str; 2] = ["claude-observe", "claude-statusline"];

/// The subcommand name st3's hook scripts exec.
pub const SUBCOMMAND: &str = "driver-hook";

/// The diagnostic code a Claude seat records when its SessionStart hook bound no native session.
pub const UNBOUND_CODE: &str = "claude-session-unbound";

/// A fault the hook reports to the daemon as a `harness.diagnostic` claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub subject: String,
    pub code: String,
    pub reason: String,
    pub idempotency_key: String,
}

/// What a hook sees of its process environment.
pub trait HookEnv {
    fn var(&self, name: &str) -> Option<String>;
}

/// The real process environment. Empty values count as unset, as the scripts treated them.
pub struct ProcessEnv;

impl HookEnv for ProcessEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|value| !value.is_empty())
    }
}

impl HookEnv for BTreeMap<String, String> {
    fn var(&self, name: &str) -> Option<String> {
        self.get(name).filter(|value| !value.is_empty()).cloned()
    }
}

/// Run hook `name` and return the process exit code.
pub fn run(
    name: &str,
    args: &[String],
    env: &dyn HookEnv,
    stdin: &mut dyn Read,
    report: &mut dyn FnMut(Diagnostic),
) -> u8 {
    // Harness-session state stays beneath st3's driver directory, never st2's.
    if let Some(drivers) = env.var("ST3_DRIVER_STATE_DIR") {
        st_drivers::run::use_harness_state_root(PathBuf::from(drivers).join("sessions"));
    }
    match name {
        "claude-observe" => claude_observe(args, env, stdin, report),
        "claude-statusline" => {
            let identity = env
                .var("ST2_CLAUDE_IDENTITY")
                .or_else(|| env.var("ST_AGENT"));
            let root = env.var("CATALOG").or_else(|| env.var("ST_ROOT"));
            let (Some(identity), Some(root)) = (identity, root) else {
                // Claude writes the payload to this process; drain it so Claude never sees EPIPE.
                let _ = std::io::copy(stdin, &mut std::io::sink());
                return 0;
            };
            let root = PathBuf::from(&root);
            let root = root.canonicalize().unwrap_or(root);
            if env.var("ST3_MAILBOX_TRANSPORT").as_deref() == Some("push") {
                // Every render reads graph authority; no display-name environment snapshot.
                if let Ok(label) = live_claude_label(env, &identity) {
                    use std::io::Write as _;
                    let _ = write!(std::io::stdout(), "{label} | ");
                    let _ = std::io::stdout().flush();
                }
            }
            // The tee records fail-open and chains to the operator's renderer itself.
            match st_drivers::claude_session::run_statusline(&root, &identity) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("st: the Claude status line failed: {error:#}");
                    1
                }
            }
        }
        other => {
            // A binary replacement must still answer scripts held by running old seats.
            // New sets contain none of these aliases, and direct invocations are rejected.
            if matches!(
                other,
                "claude-session-start"
                    | "claude-pre-compact"
                    | "claude-stop-failure"
                    | "codex-session-start"
                    | "codex-pre-compact"
                    | "codex-stop"
            ) && env
                .var("ST_HOOKS")
                .is_some_and(|dir| crate::hooks::older_set_contains_hook(Path::new(&dir), other))
            {
                let _ = std::io::copy(stdin, &mut std::io::sink());
                return 0;
            }
            let _ = std::io::copy(stdin, &mut std::io::sink());
            eprintln!(
                "st: unknown driver hook `{other}`; this binary answers {}",
                HOOKS.join(", ")
            );
            1
        }
    }
}

fn live_claude_label(env: &dyn HookEnv, identity: &str) -> Result<String> {
    let endpoint = match env.var("ST3_ENDPOINT") {
        Some(endpoint) => crate::client::Endpoint::parse(endpoint),
        None => {
            crate::client::Endpoint::Unix(crate::config::Config::load_unvalidated(None)?.socket)
        }
    };
    let subject = env
        .var("ST3_SUBJECT")
        .or_else(|| env.var("ST_AGENT"))
        .unwrap_or_else(|| format!("agent/{identity}"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let status: crate::model::StatusResponse = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            crate::client::Client::new(endpoint).get(&format!(
                "/v1/status?subject={}",
                urlencoding::encode(&subject)
            )),
        )
        .await??;
        let desired = status
            .subjects
            .into_iter()
            .find(|seat| seat.subject == subject)
            .and_then(|seat| seat.desired)
            .context("seat has no desired record")?;
        let member = serde_json::from_value::<crate::model::MemberSpec>(desired.clone()).ok();
        Ok(crate::mailbox::seat_label(&crate::model::DesiredSubject {
            subject,
            kind: "agent".into(),
            desired,
            member,
            owner_run: None,
            owner_generation: None,
            owner_step: None,
        }))
    })
}

fn claude_observe(
    args: &[String],
    env: &dyn HookEnv,
    stdin: &mut dyn Read,
    report: &mut dyn FnMut(Diagnostic),
) -> u8 {
    let mut raw = String::new();
    let _ = stdin.read_to_string(&mut raw);
    let event = args.first().map(String::as_str).unwrap_or_default();
    let session_start = event == "SessionStart";
    let mandatory = session_start
        && (env
            .var(st_drivers::claude_session::RESUME_GENERATION_ENV)
            .is_some()
            || env
                .var(st_drivers::claude_session::EXPECTED_NATIVE_SESSION_ENV)
                .is_some());
    let identity = env
        .var("ST2_CLAUDE_IDENTITY")
        .or_else(|| env.var("ST_AGENT"));
    // CATALOG first: it names the catalog that declares the agent, while ST_ROOT can be a bus root.
    let root = env.var("CATALOG").or_else(|| env.var("ST_ROOT"));
    let (Some(identity), Some(root)) = (identity, root) else {
        if session_start || mandatory {
            return unbound(
                env,
                report,
                "the SessionStart hook has no agent identity or driver catalog in its environment",
            );
        }
        return 0;
    };
    if event.is_empty() {
        eprintln!("st: claude-observe needs the Claude hook event name");
        return 0;
    }
    let root = PathBuf::from(&root);
    let root = root.canonicalize().unwrap_or(root);
    let runtime_id = env
        .var(st_drivers::claude_session::RUNTIME_ID_ENV)
        .unwrap_or_else(|| identity.clone());
    if let Err(error) = st_drivers::claude_session::run_observe_payload(
        &root,
        &identity,
        Some(&runtime_id),
        event,
        &raw,
        &|name| env.var(name),
    ) {
        if mandatory {
            eprintln!("st: the mandatory Claude SessionStart binding failed: {error:#}");
            return 1;
        }
        eprintln!("st: Claude {event} was not recorded: {error:#}");
    }
    if !session_start {
        return 0;
    }
    match session_start_binding(&root, &identity, env, &raw) {
        Ok(()) => 0,
        Err(reason) => unbound(env, report, &reason),
    }
}

/// Check that the SessionStart just applied bound this wrapper session to Claude's session.
fn session_start_binding(
    root: &Path,
    identity: &str,
    env: &dyn HookEnv,
    raw: &str,
) -> std::result::Result<(), String> {
    let wrapper = env
        .var(st_drivers::claude_session::SESSION_ENV)
        .ok_or("the SessionStart hook has no wrapper session (ST2_CLAUDE_SESSION)")?;
    let payload: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let native = payload["session_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("Claude's SessionStart payload names no session_id")?;
    let agent_dir = st_drivers::message::resolve_declared_dir(root, identity, &st_drivers::run::detect_host())
        .map_err(|error| format!("the driver catalog does not resolve: {error:#}"))?
        .ok_or_else(|| {
            format!(
                "the driver catalog {} does not declare `{identity}`",
                root.display()
            )
        })?;
    match crate::hooks::claude_binding(&agent_dir, &wrapper) {
        Some(bound) if bound == native => Ok(()),
        Some(bound) => Err(format!(
            "the native-session binding names Claude session {bound}, not {native}"
        )),
        None => Err(format!(
            "no native-session binding for wrapper session {wrapper} was written in {}",
            agent_dir.display()
        )),
    }
}

fn unbound(env: &dyn HookEnv, report: &mut dyn FnMut(Diagnostic), reason: &str) -> u8 {
    let reason = format!("Claude started, but st cannot find this seat's transcript: {reason}");
    eprintln!("st: {reason}");
    if let Some(subject) = env
        .var("ST3_SUBJECT")
        .or_else(|| env.var("ST_AGENT"))
        .filter(|subject| subject.starts_with("agent/"))
    {
        let session = env
            .var(st_drivers::claude_session::SESSION_ENV)
            .unwrap_or_else(|| "unknown".into());
        report(Diagnostic {
            idempotency_key: format!("{UNBOUND_CODE}-hook:{subject}:{session}"),
            subject,
            code: UNBOUND_CODE.into(),
            reason,
        });
    }
    1
}

/// Record `diagnostic` with the daemon at `ST3_ENDPOINT` (else this user's configured socket).
pub fn post_diagnostic(env: &dyn HookEnv, diagnostic: &Diagnostic) -> Result<()> {
    let endpoint = match env.var("ST3_ENDPOINT") {
        Some(endpoint) => crate::client::Endpoint::parse(endpoint),
        None => {
            crate::client::Endpoint::Unix(crate::config::Config::load_unvalidated(None)?.socket)
        }
    };
    let claim = crate::model::ClaimInput {
        subject: diagnostic.subject.clone(),
        kind: "harness.diagnostic".into(),
        actor: Some(diagnostic.subject.clone()),
        fields: BTreeMap::from([
            ("severity".into(), Value::String("error".into())),
            ("status".into(), Value::String("failed".into())),
            ("code".into(), Value::String(diagnostic.code.clone())),
            ("reason".into(), Value::String(diagnostic.reason.clone())),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(diagnostic.idempotency_key.clone()),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let client = crate::client::Client::new(endpoint);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.post::<_, Value>("/v1/claims", &claim),
        )
        .await
        .context("the st daemon did not answer within 5 seconds")?
        .map(|_| ())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(root: &Path) -> (PathBuf, BTreeMap<String, String>) {
        let catalog = root.join("catalog");
        let host = st_drivers::run::detect_host();
        let agent_dir = catalog.join("agents").join(&host).join("seat");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            catalog.join("catalog.kdl"),
            "catalog { pty-root \"/tmp/st3-native\" }\n",
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("agent.kdl"),
            format!(
                "agent \"example/seat\" {{\n  identity \"example/seat\"\n  host {host:?}\n  workspace \"/tmp\"\n  command \"true\"\n}}\n"
            ),
        )
        .unwrap();
        let env = BTreeMap::from([
            ("CATALOG".to_owned(), catalog.display().to_string()),
            ("ST2_CLAUDE_IDENTITY".to_owned(), "example/seat".to_owned()),
            ("ST2_CLAUDE_RUNTIME_ID".to_owned(), "example/seat".to_owned()),
            ("ST2_CLAUDE_SESSION".to_owned(), "wrapper-1".to_owned()),
            ("ST2_CLAUDE_SESSION_SEQ".to_owned(), "1".to_owned()),
            ("ST3_SUBJECT".to_owned(), "agent/example/seat".to_owned()),
            (
                "ST3_DRIVER_STATE_DIR".to_owned(),
                root.join("drivers").display().to_string(),
            ),
        ]);
        (agent_dir, env)
    }

    fn hook(
        name: &str,
        args: &[&str],
        env: &BTreeMap<String, String>,
        stdin: &str,
    ) -> (u8, Vec<Diagnostic>) {
        let mut reported = Vec::new();
        let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let code = run(name, &args, env, &mut stdin.as_bytes(), &mut |diagnostic| {
            reported.push(diagnostic)
        });
        (code, reported)
    }

    #[test]
    fn session_start_binds_the_wrapper_session_to_claudes_session() {
        let root = tempfile::tempdir().unwrap();
        let (agent_dir, env) = seat(root.path());
        let (code, reported) = hook(
            "claude-observe",
            &["SessionStart"],
            &env,
            r#"{"session_id":"native-1","transcript_path":"/nowhere.jsonl","source":"startup"}"#,
        );
        assert_eq!(code, 0);
        assert!(reported.is_empty(), "{reported:?}");
        assert_eq!(
            crate::hooks::claude_binding(&agent_dir, "wrapper-1").as_deref(),
            Some("native-1")
        );
        // Later events are observed and never report.
        let (code, reported) = hook(
            "claude-observe",
            &["UserPromptSubmit"],
            &env,
            r#"{"session_id":"native-1","prompt":"hi"}"#,
        );
        assert_eq!(code, 0);
        assert!(reported.is_empty());
        // Nothing lands in st2's state directory.
        assert!(!root.path().join("st2").exists());
    }

    #[test]
    fn a_session_start_that_binds_nothing_is_loud() {
        let root = tempfile::tempdir().unwrap();
        let (_agent_dir, mut env) = seat(root.path());
        // Claude named no session, so nothing can be bound.
        let (code, reported) = hook("claude-observe", &["SessionStart"], &env, "{}");
        assert_eq!(code, 1);
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0].subject, "agent/example/seat");
        assert_eq!(reported[0].code, UNBOUND_CODE);
        assert!(reported[0].reason.contains("session_id"), "{reported:?}");

        // A seat whose driver exported no wrapper session cannot bind either.
        env.remove("ST2_CLAUDE_SESSION");
        let (code, reported) = hook(
            "claude-observe",
            &["SessionStart"],
            &env,
            r#"{"session_id":"native-1"}"#,
        );
        assert_eq!(code, 1);
        assert!(reported[0].reason.contains("ST2_CLAUDE_SESSION"));

        // A hook with no catalog at all is loud at SessionStart, quiet otherwise.
        env.remove("CATALOG");
        assert_eq!(hook("claude-observe", &["SessionStart"], &env, "{}").0, 1);
        assert_eq!(hook("claude-observe", &["Stop"], &env, "{}").0, 0);
    }

    #[test]
    fn only_a_mandatory_resume_failure_propagates_from_observation() {
        let root = tempfile::tempdir().unwrap();
        let (_agent_dir, mut env) = seat(root.path());
        // An unwritable observation on an ordinary event fails open.
        env.insert(
            "CATALOG".into(),
            root.path().join("missing").display().to_string(),
        );
        assert_eq!(hook("claude-observe", &["Stop"], &env, "{}").0, 0);
        // A mandatory resume fence propagates its failure, as st2's hook did.
        env.insert(
            st_drivers::claude_session::RESUME_GENERATION_ENV.into(),
            "3".into(),
        );
        env.insert(
            st_drivers::claude_session::EXPECTED_NATIVE_SESSION_ENV.into(),
            "native-9".into(),
        );
        assert_eq!(hook("claude-observe", &["SessionStart"], &env, "{}").0, 1);
    }

    #[test]
    fn retired_hooks_require_an_intact_older_set() {
        let root = tempfile::tempdir().unwrap();
        let mut env = BTreeMap::new();
        let mut files = BTreeMap::new();
        let names = [
            "claude-session-start",
            "claude-pre-compact",
            "claude-stop-failure",
            "codex-session-start",
            "codex-pre-compact",
            "codex-stop",
        ];
        let script = b"#!/bin/sh\nexit 0\n";
        use sha2::{Digest as _, Sha256};
        for name in names {
            files.insert(
                format!("{name}.sh"),
                format!("sha256:{}", hex::encode(Sha256::digest(script))),
            );
        }
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schema": 1, "owner": "st3", "files": files,
        }))
        .unwrap();
        let older = root
            .path()
            .join(format!("sha256-{}", hex::encode(Sha256::digest(&manifest))));
        std::fs::create_dir(&older).unwrap();
        std::fs::write(older.join(crate::hooks::MANIFEST), &manifest).unwrap();
        let current = crate::hooks::ensure_installed(root.path()).unwrap();
        for name in names {
            std::fs::write(older.join(format!("{name}.sh")), script).unwrap();
            assert_eq!(hook(name, &[], &env, "{}").0, 1);
            env.insert("ST_HOOKS".into(), current.display().to_string());
            assert_eq!(hook(name, &[], &env, "{}").0, 1);
            assert!(!current.join(format!("{name}.sh")).exists());
            env.insert("ST_HOOKS".into(), older.display().to_string());
            assert_eq!(hook(name, &[], &env, "{}"), (0, Vec::new()));
            std::fs::write(older.join(format!("{name}.sh")), "changed").unwrap();
            assert_eq!(hook(name, &[], &env, "{}").0, 1);
            env.clear();
        }
        assert_eq!(hook("st2-boot", &[], &env, "").0, 1);
    }

    #[test]
    fn every_hook_has_a_script_in_the_set() {
        for name in HOOKS {
            let script = format!("{name}.sh");
            let (_, bytes) = crate::hooks::FILES
                .iter()
                .find(|(file, _)| *file == script)
                .unwrap_or_else(|| panic!("{script} is not in the hook set"));
            let text = std::str::from_utf8(bytes).unwrap();
            assert!(text.contains(&format!("exec \"$ST3_BIN\" {SUBCOMMAND} {name}")));
        }
    }
}
