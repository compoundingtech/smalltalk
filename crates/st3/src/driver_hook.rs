//! `st driver-hook NAME [ARGS]`: the st3 binary answering a hook its own seat's harness runs.
//!
//! Every script in st3's hook set execs this, hidden from `--help`, with the harness's payload on
//! stdin. The arguments and environment are the ones the seat's driver exports: the event name in
//! the first argument, resolved `ST_DRIVER_*` paths and identity, and the `ST_CLAUDE_*`
//! wrapper-session fence. Already-running seats retain their catalog discovery fallback.
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
    fn raw_var(&self, name: &str) -> Option<String> {
        self.var(name)
    }
}

/// The real process environment. Empty values count as unset, as the scripts treated them.
pub struct ProcessEnv;

impl HookEnv for ProcessEnv {
    fn raw_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|value| !value.is_empty())
    }
}

impl HookEnv for BTreeMap<String, String> {
    fn raw_var(&self, name: &str) -> Option<String> {
        self.get(name).cloned()
    }
    fn var(&self, name: &str) -> Option<String> {
        self.get(name).filter(|value| !value.is_empty()).cloned()
    }
}

fn hook_var(env: &dyn HookEnv, name: &str) -> Option<String> {
    st_drivers::contracts::env_with(name, &|key| env.raw_var(key))
}

fn hook_identity(env: &dyn HookEnv) -> Option<String> {
    env.var(st_drivers::driver_paths::IDENTITY_ENV)
        .or_else(|| hook_var(env, "ST_CLAUDE_IDENTITY"))
        .or_else(|| {
            env.var("ST_AGENT").map(|subject| {
                subject
                    .strip_prefix("agent/")
                    .unwrap_or(&subject)
                    .to_owned()
            })
        })
}

fn hook_paths(env: &dyn HookEnv, identity: &str) -> Result<st_drivers::driver_paths::Paths> {
    if let Some(paths) =
        st_drivers::driver_paths::Paths::from_environment(identity, &|name| env.raw_var(name))?
    {
        return Ok(paths);
    }
    // Compatibility for providers launched before explicit paths. Their immutable hook sets
    // still exec the replaced binary, and their declarations stay intact until restart.
    let root = env
        .var("CATALOG")
        .or_else(|| env.var("ST_ROOT"))
        .context("the old hook has no driver catalog")?;
    let root = PathBuf::from(root);
    let root = root.canonicalize().unwrap_or(root);
    let agent_dir = st_drivers::message::resolve_declared_dir(
        &root,
        identity,
        &st_drivers::run::detect_host(),
    )?
    .context("the old hook's identity is not declared")?;
    let session_dir = st_drivers::claude_session::state_dir(&root, identity);
    Ok(st_drivers::driver_paths::Paths {
        root,
        agent_dir,
        session_dir,
    })
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
            let identity = hook_identity(env);
            let Some(identity) = identity else {
                let _ = std::io::copy(stdin, &mut std::io::sink());
                return 0;
            };
            let paths = match hook_paths(env, &identity) {
                Ok(paths) => paths,
                Err(error) => {
                    let _ = std::io::copy(stdin, &mut std::io::sink());
                    eprintln!("st: Claude status line paths are unavailable: {error:#}");
                    return 0;
                }
            };
            if env.var("ST3_MAILBOX_TRANSPORT").as_deref() == Some("push") {
                // Every render reads graph authority; no display-name environment snapshot.
                if let Ok(label) = live_claude_label(env, &identity) {
                    use std::io::Write as _;
                    let _ = write!(std::io::stdout(), "{label} | ");
                    let _ = std::io::stdout().flush();
                }
            }
            // The tee records fail-open and chains to the operator's renderer itself.
            let rendered = if env.raw_var(st_drivers::driver_paths::ROOT_ENV).is_some() {
                st_drivers::claude_session::run_statusline_paths(&paths.agent_dir, &identity)
            } else {
                st_drivers::claude_session::run_statusline(&paths.root, &identity)
            };
            match rendered {
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
    let persona_short = env.var("AGENT_PERSONA_SHORT");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let client = crate::client::Client::new(endpoint);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        // The seat's own desired record, read on every render so renames show at once.
        if let Ok(Ok(seat)) = tokio::time::timeout_at(
            deadline,
            client.get::<crate::model::DesiredSubject>(&format!("/v1/desired/{subject}")),
        )
        .await
        {
            return Ok(crate::mailbox::seat_label(&seat, persona_short.as_deref()));
        }
        // A daemon older than `/v1/desired` answers from its status reduction.
        let status: crate::model::StatusResponse = tokio::time::timeout_at(
            deadline,
            client.get(&format!(
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
        // Status exposes the canonical KDL node, not the supervisor's MemberSpec. Read its
        // `name` child on every render so renames and clearing a name take effect immediately.
        let display_name = desired
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|child| child.get("name").and_then(Value::as_str) == Some("name"))
            .and_then(|child| child.pointer("/arguments/0"))
            .and_then(Value::as_str);
        let desired = serde_json::json!({"display_name": display_name});
        Ok(crate::mailbox::seat_label(
            &crate::model::DesiredSubject {
                subject,
                kind: "agent".into(),
                desired,
                member: None,
                owner_run: None,
                owner_generation: None,
                owner_step: None,
            },
            persona_short.as_deref(),
        ))
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
        && (hook_var(env, st_drivers::claude_session::RESUME_GENERATION_ENV).is_some()
            || hook_var(env, st_drivers::claude_session::EXPECTED_NATIVE_SESSION_ENV).is_some());
    let identity = hook_identity(env);
    let Some(identity) = identity else {
        if session_start || mandatory {
            return unbound(
                env,
                report,
                "the SessionStart hook has no agent identity in its environment",
            );
        }
        return 0;
    };
    let paths = match hook_paths(env, &identity) {
        Ok(paths) => paths,
        Err(error) => {
            if session_start || mandatory {
                return unbound(
                    env,
                    report,
                    &format!("the SessionStart hook has no valid driver paths: {error:#}"),
                );
            }
            return 0;
        }
    };
    if event.is_empty() {
        eprintln!("st: claude-observe needs the Claude hook event name");
        return 0;
    }
    let runtime_id = hook_var(env, st_drivers::claude_session::RUNTIME_ID_ENV)
        .unwrap_or_else(|| identity.clone());
    let observed = if env.raw_var(st_drivers::driver_paths::ROOT_ENV).is_some() {
        st_drivers::claude_session::run_observe_payload_paths(
            &paths,
            &identity,
            &runtime_id,
            event,
            &raw,
            &|name| env.raw_var(name),
        )
    } else {
        st_drivers::claude_session::run_observe_payload(
            &paths.root,
            &identity,
            Some(&runtime_id),
            event,
            &raw,
            &|name| env.raw_var(name),
        )
    };
    if let Err(error) = observed {
        if mandatory {
            eprintln!("st: the mandatory Claude SessionStart binding failed: {error:#}");
            return 1;
        }
        eprintln!("st: Claude {event} was not recorded: {error:#}");
    }
    if event == "PermissionRequest" {
        answer_permission_from_st(env, &identity, &paths.agent_dir, &raw);
        return 0;
    }
    if !session_start {
        return 0;
    }
    match session_start_binding(&paths.agent_dir, env, &raw) {
        Ok(()) => 0,
        Err(reason) => unbound(env, report, &reason),
    }
}

/// How long a permission hook waits for a person's answer from a client. Claude shows its own
/// dialog meanwhile, and an answer there wins at once, so the wait only bounds the hook.
const PROMPT_ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(600);
const PROMPT_ANSWER_POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// Let a person answer a Claude permission prompt from a client. The hook waits while Claude
/// shows its dialog; if the person answers in st first, the hook returns that decision, which
/// closes the dialog (measured on Claude Code 2.1.296). A question prompt is answered in the
/// terminal only.
fn answer_permission_from_st(env: &dyn HookEnv, identity: &str, agent_dir: &Path, raw: &str) {
    let payload: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    if payload.get("tool_name").and_then(Value::as_str) == Some("AskUserQuestion") {
        return;
    }
    // This hook's own observation, as its state record names it: only an answer to that exact
    // prompt is this hook's, never one to an earlier prompt or a parallel hook's.
    let Some((ownership, transition)) = st_drivers::harness_state::read(
        &st_drivers::harness_state::harness_state_path(agent_dir),
        None,
    )
    .filter(|observed| observed.blocked_on == st_drivers::harness_state::BlockedOn::Human)
    .and_then(|observed| observed.ownership_sequence.zip(observed.transition_sequence)) else {
        return;
    };
    let endpoint = match env.var("ST3_ENDPOINT") {
        Some(endpoint) => crate::client::Endpoint::parse(endpoint),
        None => match crate::config::Config::load_unvalidated(None) {
            Ok(config) => crate::client::Endpoint::Unix(config.socket),
            Err(_) => return,
        },
    };
    let subject = env
        .var("ST3_SUBJECT")
        .or_else(|| env.var("ST_AGENT"))
        .unwrap_or_else(|| format!("agent/{identity}"));
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let client = crate::client::Client::new(endpoint);
    let path = format!(
        "/v1/harness-prompts/state?agent={}&ownership={ownership}&transition={transition}",
        urlencoding::encode(&subject)
    );
    let answer = wait_for_prompt_answer(
        || {
            runtime.block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    client.get::<crate::store::NativePromptState>(&path),
                )
                .await
                .ok()
                .and_then(Result::ok)
            })
        },
        PROMPT_ANSWER_WAIT,
        PROMPT_ANSWER_POLL,
    );
    if let Some(output) = answer.as_deref().and_then(permission_decision) {
        println!("{output}");
    }
}

/// Poll `state` until the prompt is answered (its answer), gone, or `wait` runs out (none).
fn wait_for_prompt_answer(
    mut state: impl FnMut() -> Option<crate::store::NativePromptState>,
    wait: std::time::Duration,
    poll: std::time::Duration,
) -> Option<String> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        match state() {
            Some(crate::store::NativePromptState::Answered { answer }) => return Some(answer),
            Some(crate::store::NativePromptState::Gone) => return None,
            Some(crate::store::NativePromptState::Open) | None => {}
        }
        if std::time::Instant::now() + poll > deadline {
            return None;
        }
        std::thread::sleep(poll);
    }
}

/// Claude's `PermissionRequest` hook output for a person's answer.
fn permission_decision(answer: &str) -> Option<String> {
    let decision = match answer {
        "allow" => serde_json::json!({"behavior": "allow"}),
        "deny" => serde_json::json!({"behavior": "deny", "message": "The person denied this from st."}),
        _ => return None,
    };
    Some(
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "PermissionRequest", "decision": decision}})
        .to_string(),
    )
}

/// Check that the SessionStart just applied bound this wrapper session to Claude's session.
fn session_start_binding(
    agent_dir: &Path,
    env: &dyn HookEnv,
    raw: &str,
) -> std::result::Result<(), String> {
    let wrapper = hook_var(env, st_drivers::claude_session::SESSION_ENV)
        .ok_or("the SessionStart hook has no wrapper session (ST_CLAUDE_SESSION)")?;
    let payload: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let native = payload["session_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("Claude's SessionStart payload names no session_id")?;
    match crate::hooks::claude_binding(agent_dir, &wrapper) {
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
        let session = hook_var(env, st_drivers::claude_session::SESSION_ENV)
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
            ("ST_CLAUDE_IDENTITY".to_owned(), "example/seat".to_owned()),
            ("ST_CLAUDE_RUNTIME_ID".to_owned(), "example/seat".to_owned()),
            ("ST_CLAUDE_SESSION".to_owned(), "wrapper-1".to_owned()),
            ("ST_CLAUDE_SESSION_SEQ".to_owned(), "1".to_owned()),
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
    fn claude_live_label_reads_canonical_status_names_and_reflects_rename_and_clear() {
        use axum::{Json, Router, routing::get};
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let desired = std::sync::Arc::new(std::sync::Mutex::new(serde_json::json!({
            "name":"agent", "arguments":["eval.worker"],
            "children":[{"name":"name", "arguments":["Quartz"], "children":[]}],
        })));
        let captured = desired.clone();
        let app = Router::new().route(
            "/v1/status",
            get(move || {
                let captured = captured.clone();
                async move {
                    Json(serde_json::json!({"api_version":"st3.v1", "value": {
                        "store_index":1, "pending_actions":[], "subjects":[{
                            "subject":"agent/eval.worker", "kind":"agent", "desired_token":null,
                            "desired_revision":null, "desired":captured.lock().unwrap().clone(),
                            "actual":null, "conflicts":[], "claims":[], "owner_run":null,
                            "gap":null, "reachability":"unknown", "reason":null,
                        }],
                    }}))
                }
            }),
        );
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = runtime.spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut env = BTreeMap::from([
            ("ST3_ENDPOINT".into(), format!("http://{address}")),
            ("ST3_SUBJECT".into(), "agent/eval.worker".into()),
        ]);
        assert_eq!(live_claude_label(&env, "eval.worker").unwrap(), "Quartz");
        env.insert("AGENT_PERSONA_SHORT".into(), "gen".into());
        assert_eq!(live_claude_label(&env, "eval.worker").unwrap(), "Quartz[gen]");
        desired.lock().unwrap()["children"][0]["arguments"][0] = serde_json::json!("Indigo\u{7}");
        assert_eq!(live_claude_label(&env, "eval.worker").unwrap(), "Indigo[gen]");
        desired.lock().unwrap()["children"] = serde_json::json!([]);
        assert_eq!(
            live_claude_label(&env, "eval.worker").unwrap(),
            "eval.worker[gen]"
        );
        server.abort();
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
    fn native_hooks_record_without_catalog_discovery_and_keep_session_fencing() {
        let root = tempfile::tempdir().unwrap();
        let paths = st_drivers::driver_paths::Paths {
            root: root.path().to_path_buf(),
            agent_dir: root.path().join("observations"),
            session_dir: root.path().join("sessions/claude"),
        };
        let mut env: BTreeMap<_, _> = paths.environment("example/seat").into_iter().collect();
        env.extend([
            ("ST_CLAUDE_RUNTIME_ID".into(), "example/seat".into()),
            ("ST_CLAUDE_SESSION".into(), "current".into()),
            ("ST_CLAUDE_SESSION_SEQ".into(), "1".into()),
            // Poisoned inherited discovery settings must never be read.
            (
                "CATALOG".into(),
                root.path().join("missing").display().to_string(),
            ),
        ]);
        let seq =
            st_drivers::harness_state::claim(&paths.agent_dir, "example/seat", "claude", "current")
                .unwrap();
        env.insert("ST_CLAUDE_SESSION_SEQ".into(), seq.to_string());
        assert_eq!(
            hook(
                "claude-observe",
                &["UserPromptSubmit"],
                &env,
                r#"{"session_id":"native-1","prompt":"hello"}"#
            )
            .0,
            0
        );
        let record_path = st_drivers::harness_state::harness_state_path(&paths.agent_dir);
        let current = std::fs::read(&record_path).unwrap();
        let observed = st_drivers::harness_state::read(&record_path, None).unwrap();
        assert_eq!(observed.state, st_drivers::harness_state::Activity::Active);
        assert_eq!(observed.evidence_incarnation.as_deref(), Some("current"));
        st_drivers::harness_state::claim(&paths.agent_dir, "example/seat", "claude", "successor")
            .unwrap();
        let successor = std::fs::read(&record_path).unwrap();
        assert_ne!(current, successor);
        assert_eq!(
            hook(
                "claude-observe",
                &["Stop"],
                &env,
                r#"{"session_id":"native-1"}"#
            )
            .0,
            0
        );
        assert_eq!(std::fs::read(&record_path).unwrap(), successor);
        assert!(!root.path().join("catalog").exists());
        assert!(!paths.agent_dir.join("agent.kdl").exists());
        env.remove(st_drivers::driver_paths::SESSION_DIR_ENV);
        let (code, reported) = hook(
            "claude-observe",
            &["SessionStart"],
            &env,
            r#"{"session_id":"native-1"}"#,
        );
        assert_eq!(code, 1);
        // No reporting identity was supplied, but the hook still refuses the partial contract.
        assert!(reported.is_empty());
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
        env.remove("ST_CLAUDE_SESSION");
        let (code, reported) = hook(
            "claude-observe",
            &["SessionStart"],
            &env,
            r#"{"session_id":"native-1"}"#,
        );
        assert_eq!(code, 1);
        assert!(reported[0].reason.contains("ST_CLAUDE_SESSION"));

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
    fn a_permission_hook_returns_the_persons_answer_and_nothing_else() {
        use crate::store::NativePromptState;
        let tick = std::time::Duration::from_millis(1);
        let wait = std::time::Duration::from_secs(5);
        let mut states = vec![
            None,
            Some(NativePromptState::Open),
            Some(NativePromptState::Answered {
                answer: "allow".into(),
            }),
        ]
        .into_iter();
        assert_eq!(
            super::wait_for_prompt_answer(|| states.next().flatten(), wait, tick),
            Some("allow".into())
        );
        // Answered in the terminal: nothing to say, and the hook stops waiting.
        assert_eq!(
            super::wait_for_prompt_answer(|| Some(NativePromptState::Gone), wait, tick),
            None
        );
        // Nobody answers in time: the hook says nothing and Claude's dialog stays.
        assert_eq!(
            super::wait_for_prompt_answer(
                || Some(NativePromptState::Open),
                std::time::Duration::from_millis(5),
                tick
            ),
            None
        );
        let allow: Value = serde_json::from_str(&super::permission_decision("allow").unwrap())
            .unwrap();
        assert_eq!(allow["hookSpecificOutput"]["hookEventName"], "PermissionRequest");
        assert_eq!(allow["hookSpecificOutput"]["decision"]["behavior"], "allow");
        let deny: Value =
            serde_json::from_str(&super::permission_decision("deny").unwrap()).unwrap();
        assert_eq!(deny["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert!(super::permission_decision("maybe").is_none());
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
    #[test]
    fn legacy_hook_environment_survives_upgrade_and_current_values_win() {
        let root = tempfile::tempdir().unwrap();
        let (agent_dir, env) = seat(root.path());
        let mut env = env
            .into_iter()
            .map(|(key, value)| {
                (
                    if key.starts_with("ST_CLAUDE_") {
                        key.replacen("ST_", "ST2_", 1)
                    } else {
                        key
                    },
                    value,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let payload =
            r#"{"session_id":"native-1","transcript_path":"/nowhere.jsonl","source":"startup"}"#;
        assert_eq!(
            hook("claude-observe", &["SessionStart"], &env, payload).0,
            0
        );
        assert_eq!(
            crate::hooks::claude_binding(&agent_dir, "wrapper-1").as_deref(),
            Some("native-1")
        );
        env.insert("ST_CLAUDE_SESSION".into(), "wrapper-2".into());
        env.insert(
            "ST2_CLAUDE_RESUME_GENERATION".into(),
            "invalid-old-fence".into(),
        );
        env.insert("ST_CLAUDE_RESUME_GENERATION".into(), String::new());
        assert_eq!(
            hook("claude-observe", &["SessionStart"], &env, payload).0,
            0
        );
        assert_eq!(
            crate::hooks::claude_binding(&agent_dir, "wrapper-2").as_deref(),
            Some("native-1")
        );
        env.insert("ST_CLAUDE_SESSION".into(), String::new());
        assert_eq!(hook_var(&env, "ST_CLAUDE_SESSION"), None);
    }
}
