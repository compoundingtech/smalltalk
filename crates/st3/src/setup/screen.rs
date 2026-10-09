//! Setup as one screen. This file decides what the checklist shows and does the work when the
//! person presses its one button; the screen itself is `stui::checklist`.
//!
//! The line-mode setup stays for pipes, `--yes` and anything that is not a terminal. Both modes
//! run the same stages (save the names, install st, start it, set up each agent, start the
//! Assistant); only who answers the questions differs.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use stui::checklist::{self, Access, Facts, Field, Form, Outcome, Reporter, Row, Run, Tri};

use super::{
    PreparedSetup, SetupArgs, capture, check_login_path, daemon_ready, ensure_claude_channel,
    install_binaries, merge_config, said, start_daemon, validate_name,
};
use crate::config::Config;
use crate::environment::HARNESSES;

/// Everything the screen needs to know about this run of setup.
pub(super) struct Start {
    pub args: SetupArgs,
    pub path: PathBuf,
    pub config: Config,
    pub previous_node: String,
    pub existing_store: bool,
    pub default_person: String,
    pub default_node: String,
}

/// What the worker leaves behind for the caller.
#[derive(Default)]
struct Result_ {
    /// Whether the names were saved: setup went far enough to count.
    saved: bool,
    harness: Option<String>,
    started_assistant: bool,
    initial_subject: Option<String>,
    /// Lines setup said while the screen was up, for the terminal afterwards.
    log: Vec<String>,
    /// Harnesses that ended ready, in the order they did.
    ready: Vec<String>,
}

pub(super) async fn run(start: Start) -> Result<PreparedSetup> {
    println!("Looking for your agents…");
    let environment = crate::environment::snapshot()?;
    let mut form = form(&start, &environment);
    if let Some(requested) = &start.args.harness {
        for row in &mut form.rows {
            row.selected = &row.id == requested && row.facts.found.is_some();
        }
        anyhow::ensure!(
            requested == "none" || form.rows.iter().any(|row| &row.id == requested && row.selected),
            "{requested} is not installed on the daemon's login PATH; install it and open a new login shell, then run st setup"
        );
    }

    let shared = Mutex::new(Result_::default());
    let config = Mutex::new(start.config.clone());
    let executable = std::env::current_exe()?;
    let finished = checklist::run(
        form,
        &|field, value| {
            validate_name(value, field == Field::Machine).map_err(|error| error.to_string())
        },
        |run, reporter| work(&start, &shared, &config, &executable, run, reporter),
        |id| hand_over(id),
        &|| {
            crate::environment::refresh();
            crate::environment::snapshot()
                .map(|environment| rows(&environment))
                .unwrap_or_default()
        },
    )?;
    let outcome = shared.into_inner().unwrap_or_default();
    // The screen is gone; what setup said and how every row ended stays in the scrollback.
    for line in &outcome.log {
        println!("{line}");
    }
    for row in &finished.form.rows {
        if !row.selected {
            continue;
        }
        match &row.outcome {
            Some(Outcome::Ready(note)) => println!("{}: ready. {note}", row.name),
            Some(Outcome::NeedsLogin(note)) => println!("{}: needs you to sign in. {note}", row.name),
            Some(Outcome::Failed(error)) => println!("{}: failed. {error}", row.name),
            None => {}
        }
    }
    anyhow::ensure!(
        outcome.saved,
        "setup was cancelled; nothing was changed. Run st setup when you are ready"
    );
    println!("Your agents run without permission prompts inside their own workspaces.");
    Ok(PreparedSetup {
        config: config.into_inner().unwrap_or(start.config),
        harness: outcome.harness,
        initial_subject: outcome.initial_subject,
    })
}

// ---- what the screen shows ----------------------------------------------------------------

fn form(start: &Start, environment: &BTreeMap<String, String>) -> Form {
    let rows = rows(environment);
    let codex = rows
        .iter()
        .any(|row| row.id == "codex" && row.facts.found.is_some())
        .then(|| match start.config.codex_access.as_deref() {
            Some("workspace") => Access::Workspace,
            Some("full") => Access::Full,
            _ => Access::Ask,
        });
    let home = home(environment);
    let login_note = if cfg!(target_os = "macos") {
        format!(
            "Installs a launch agent for st in {}/Library/LaunchAgents. No admin rights. Remove it with st service uninstall.",
            home.display()
        )
    } else {
        let config_home = environment
            .get("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        format!(
            "Installs a systemd user service at {}/systemd/user/st3.service. No admin rights. Remove it with st service uninstall.",
            config_home.display()
        )
    };
    Form {
        name: start
            .args
            .person
            .clone()
            .unwrap_or_else(|| start.default_person.clone()),
        machine: start
            .args
            .node
            .clone()
            .unwrap_or_else(|| start.default_node.clone()),
        login_label: "Start st when I log in".into(),
        login: start.args.service.unwrap_or(true),
        login_note,
        linger: lingering_offered(),
        rows,
        codex,
    }
}

/// "Keep running after I log out" is offered only where it can be turned on without sudo.
fn lingering_offered() -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        let user = std::env::var("USER").ok().filter(|user| !user.is_empty())?;
        let shown = Command::new("loginctl")
            .args(["show-user", &user, "--property=Linger", "--value"])
            .stdin(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())?;
        (String::from_utf8_lossy(&shown.stdout).trim() == "no").then_some(true)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn home(environment: &BTreeMap<String, String>) -> PathBuf {
    environment
        .get("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("~"))
}

/// A row for every harness st supports, found or not.
pub(super) fn rows(environment: &BTreeMap<String, String>) -> Vec<Row> {
    let home = home(environment);
    HARNESSES
        .iter()
        .map(|&id| {
            let found = st_runtime::resolve_executable(id, environment).ok();
            let state = signin(id, environment, &home);
            let (name, sets_up, install_hint) = describe(id, environment, &home);
            Row {
                id: id.into(),
                name: name.into(),
                facts: Facts {
                    found: found.as_ref().map(|path| path.display().to_string()),
                    // Nothing is installed for it yet, and nothing has checked.
                    integration: Tri::Unknown,
                    signed_in: state.signed_in,
                },
                sets_up,
                install_hint,
                selected: found.is_some(),
                outcome: None,
            }
        })
        .collect()
}

/// A harness's display name, what setup does for it with the exact paths, and how to get it.
fn describe(
    id: &str,
    environment: &BTreeMap<String, String>,
    home: &Path,
) -> (&'static str, String, String) {
    let data = environment
        .get("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let dir = |variable: &str, default: &str| {
        environment
            .get(variable)
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(default))
    };
    match id {
        "claude" => (
            "Claude",
            format!(
                "Adds the st channel plugin to Claude through its own plugin command (user scope, no admin rights). Writes the plugin files under {} and registers them in {}.",
                data.join("st2/claude-channel/marketplace").display(),
                dir("CLAUDE_CONFIG_DIR", ".claude").display()
            ),
            "Install Claude Code with its own installer (npm install -g @anthropic-ai/claude-code), open a new terminal, then press r to look again.".into(),
        ),
        "codex" => (
            "Codex",
            format!(
                "Checks that Codex starts and is signed in. Writes nothing under {}: seats start Codex with st's own launch arguments.",
                dir("CODEX_HOME", ".codex").display()
            ),
            "Install Codex with its own installer (npm install -g @openai/codex), open a new terminal, then press r to look again.".into(),
        ),
        "opencode" => (
            "OpenCode",
            format!(
                "Checks that OpenCode starts and is signed in. Writes nothing under {}: st starts its server for each seat.",
                home.join(".config/opencode").display()
            ),
            "Install OpenCode with its own installer (npm install -g opencode-ai), open a new terminal, then press r to look again.".into(),
        ),
        "pi" => (
            "Pi",
            "Checks that Pi starts. Writes nothing globally: st loads its bundled extension for each seat.".into(),
            "Install Pi with its own installer so it is on your login PATH, open a new terminal, then press r to look again.".into(),
        ),
        _ => (
            "Omp",
            "Checks that Omp starts. Writes nothing globally: st loads its bundled extension for each seat.".into(),
            "Install Omp with its own installer so it is on your login PATH, open a new terminal, then press r to look again.".into(),
        ),
    }
}

// ---- what is true of a harness -----------------------------------------------------------

/// Two facts about a harness's own first run, read from the files it keeps (never printed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Signin {
    /// Its first launch (theme, welcome) is finished.
    pub first_run: Tri,
    pub signed_in: Tri,
}

fn read_small(path: &Path) -> Option<String> {
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(8 << 20)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

pub(super) fn signin(id: &str, environment: &BTreeMap<String, String>, home: &Path) -> Signin {
    let unknown = Signin {
        first_run: Tri::Unknown,
        signed_in: Tri::Unknown,
    };
    let variable = |name: &str| environment.get(name).is_some_and(|value| !value.is_empty());
    match id {
        "claude" => {
            let file = environment
                .get("CLAUDE_CONFIG_DIR")
                .filter(|dir| !dir.is_empty())
                .map(|dir| PathBuf::from(dir).join(".claude.json"))
                .unwrap_or_else(|| home.join(".claude.json"));
            let key = variable("ANTHROPIC_API_KEY") || variable("ANTHROPIC_AUTH_TOKEN");
            let Some(text) = read_small(&file) else {
                // No config at all: Claude has never run here.
                return Signin {
                    first_run: Tri::No,
                    signed_in: if key { Tri::Unknown } else { Tri::No },
                };
            };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
                return unknown;
            };
            let first_run = json
                .get("hasCompletedOnboarding")
                .and_then(serde_json::Value::as_bool)
                .map_or(Tri::No, |done| if done { Tri::Yes } else { Tri::No });
            let signed_in = if json.get("oauthAccount").is_some_and(serde_json::Value::is_object) {
                Tri::Yes
            } else if key {
                Tri::Unknown
            } else {
                Tri::No
            };
            Signin { first_run, signed_in }
        }
        "codex" => {
            let dir = environment
                .get("CODEX_HOME")
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex"));
            let has_login = read_small(&dir.join("auth.json")).is_some_and(|text| text.trim().len() > 2);
            Signin {
                first_run: Tri::Unknown,
                signed_in: if has_login {
                    Tri::Yes
                } else if variable("OPENAI_API_KEY") {
                    Tri::Unknown
                } else {
                    Tri::No
                },
            }
        }
        "opencode" => {
            let data = environment
                .get("XDG_DATA_HOME")
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share"));
            let has_login = read_small(&data.join("opencode/auth.json"))
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .and_then(|json| json.as_object().map(|object| !object.is_empty()))
                .unwrap_or(false);
            Signin {
                first_run: Tri::Unknown,
                signed_in: if has_login { Tri::Yes } else { Tri::No },
            }
        }
        _ => unknown,
    }
}

fn needs_you(state: Signin) -> bool {
    state.first_run == Tri::No || state.signed_in == Tri::No
}

fn sign_in_words(id: &str) -> &'static str {
    match id {
        "claude" => "Claude is installed, but its first launch or sign-in is not finished. Run claude in your own terminal, pick a theme and sign in, then retry.",
        "codex" => "Codex is installed, but not signed in. Run codex login in your own terminal, then retry.",
        _ => "OpenCode is installed, but not signed in. Run opencode auth login in your own terminal, then retry.",
    }
}

/// Run a harness's own first launch in this terminal, so the person can finish it, and return
/// when they leave it.
fn hand_over(id: &str) -> Result<(), String> {
    let environment = crate::environment::snapshot().map_err(|error| format!("{error:#}"))?;
    let program = st_runtime::resolve_executable(id, &environment).map_err(|error| format!("{error:#}"))?;
    let (arguments, note): (&[&str], &str) = match id {
        "claude" => (
            &[],
            "Claude is starting so you can pick a theme and sign in. Leave it with /exit when you are done, and setup carries on.",
        ),
        "codex" => (&["login"], "Codex is asking you to sign in. Setup carries on when it finishes."),
        "opencode" => (
            &["auth", "login"],
            "OpenCode is asking you to sign in. Setup carries on when it finishes.",
        ),
        _ => return Ok(()),
    };
    println!("\n{note}\n");
    let mut command = Command::new(program);
    command.args(arguments);
    for (name, value) in &environment {
        // The terminal in front of the person describes itself better than a login shell does.
        if !matches!(name.as_str(), "TERM" | "COLUMNS" | "LINES") {
            command.env(name, value);
        }
    }
    command
        .status()
        .map(|_| ())
        .map_err(|error| format!("could not start {id}: {error}"))
}

/// Start a program with `--version` and say what it answered; a program that cannot start is the
/// error, whole.
fn starts(program: &Path, environment: &BTreeMap<String, String>) -> Result<String, String> {
    let mut child = Command::new(program)
        .arg("--version")
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{} could not be started: {error}", program.display()))?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} did not answer --version within 15 seconds", program.display()));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    let output = child.wait_with_output().map_err(|error| error.to_string())?;
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim().to_owned();
    if output.status.success() {
        Ok(text(&output.stdout).lines().next().unwrap_or_default().to_owned())
    } else {
        Err(format!(
            "{} --version failed ({}): {}{}",
            program.display(),
            output.status,
            text(&output.stderr),
            text(&output.stdout)
        ))
    }
}

// ---- what pressing the button does -------------------------------------------------------

fn work(
    start: &Start,
    shared: &Mutex<Result_>,
    config: &Mutex<Config>,
    executable: &Path,
    run: Run,
    reporter: &Reporter,
) -> Result<(), String> {
    // Everything setup says while the screen is up goes to its footer and then the scrollback.
    let log = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
    let sink_log = log.clone();
    let sink_reporter = reporter.clone();
    capture(Some(Box::new(move |line| {
        sink_reporter.step(line.lines().next().unwrap_or_default().to_owned());
        if let Ok(mut log) = sink_log.lock() {
            log.push(line);
        }
    })));
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
        .and_then(|runtime| {
            runtime
                .block_on(stages(start, shared, config, executable, run, reporter))
                .map_err(|error| format!("{error:#}"))
        });
    capture(None);
    if let (Ok(mut said), Ok(mut shared)) = (log.lock(), shared.lock()) {
        shared.log.append(&mut said);
    }
    result
}

async fn stages(
    start: &Start,
    shared: &Mutex<Result_>,
    config: &Mutex<Config>,
    executable: &Path,
    run: Run,
    reporter: &Reporter,
) -> Result<()> {
    let form = &run.form;
    validate_name(&form.name, false).context("your name")?;
    validate_name(&form.machine, true).context("this machine's name")?;
    anyhow::ensure!(
        !start.existing_store || form.machine == start.previous_node,
        "this store belongs to `{}`; setup cannot rename its machine",
        start.previous_node
    );
    let mut next = start.config.clone();
    next.person = Some(format!("person/{}", form.name));
    next.node = form.machine.clone();
    next.codex_access = form.codex.map(|access| access.as_str().to_owned());
    next.validate()?;

    reporter.step("Saving your name and this machine's name");
    merge_config(&start.path, &next)?;
    *config.lock().expect("config lock") = next.clone();
    shared.lock().expect("result lock").saved = true;
    said(format!("Saved {}", start.path.display()));
    if let Some(kib) = crate::read_cache::override_kib() {
        said(format!("Read cache: {kib} KiB per reader."));
    }

    let executable = if start.args.install.unwrap_or(true) {
        reporter.step("Installing st");
        install_binaries(executable)?
    } else {
        executable.to_owned()
    };
    check_login_path(&executable);

    if !daemon_ready(&next).await && start.args.start.unwrap_or(true) {
        reporter.step("Starting st");
        start_daemon(
            &next,
            &executable,
            &start.path,
            form.login,
            form.linger.unwrap_or(false),
        )
        .await?;
    } else if !daemon_ready(&next).await {
        said("Configuration saved; the daemon remains stopped.");
    }
    if run.skip_agents {
        return Ok(());
    }

    let wanted = |row: &Row| {
        row.selected
            && row.facts.found.is_some()
            && run.only.as_ref().is_none_or(|only| only.contains(&row.id))
    };
    for row in form.rows.iter().filter(|row| wanted(row)) {
        let executable = executable.clone();
        let (id, name) = (row.id.clone(), row.name.clone());
        let worker = reporter.clone();
        reporter.step(format!("Setting up {name}"));
        let outcome = tokio::task::spawn_blocking(move || set_up_row(&id, &executable, &worker))
            .await
            .map_err(|error| anyhow::anyhow!("{name}: {error}"))?;
        if matches!(outcome, Outcome::Ready(_)) {
            shared.lock().expect("result lock").ready.push(row.id.clone());
        }
        reporter.row(&row.id, outcome);
    }

    // The Assistant starts while the screen is still up, once something is ready to run it.
    let (harness, started) = {
        let shared = shared.lock().expect("result lock");
        (shared.ready.first().cloned(), shared.started_assistant)
    };
    if let (Some(harness), false) = (harness, started) {
        if daemon_ready(&next).await {
            reporter.step("Starting the Assistant");
            let subject = crate::onboarding::start(&next, &harness, start.args.onboarding).await?;
            let mut shared = shared.lock().expect("result lock");
            shared.started_assistant = true;
            shared.harness = Some(harness);
            shared.initial_subject = subject;
        } else {
            anyhow::ensure!(
                !start.args.onboarding,
                "rerunning onboarding needs a running daemon; pass --start true"
            );
            said("Start the st daemon and run st setup to begin onboarding.");
        }
    }
    Ok(())
}

/// Set up one agent and say how it ended. Runs on its own thread: it starts programs and may hand
/// the terminal to the person.
fn set_up_row(id: &str, executable: &Path, reporter: &Reporter) -> Outcome {
    let environment = match crate::environment::snapshot() {
        Ok(environment) => environment,
        Err(error) => return Outcome::Failed(format!("{error:#}")),
    };
    let home = home(&environment);
    let Ok(program) = st_runtime::resolve_executable(id, &environment) else {
        return Outcome::Failed(format!("{id} is no longer on the login PATH"));
    };
    let mut facts = Facts {
        found: Some(program.display().to_string()),
        integration: Tri::Unknown,
        signed_in: Tri::Unknown,
    };
    let mut note = String::new();
    if id == "claude" {
        if let Err(error) = ensure_claude_channel(executable) {
            return Outcome::Failed(format!("The st channel plugin could not be installed: {error}"));
        }
        facts.integration = Tri::Yes;
        note.push_str(if st_drivers::claude_channel::st3_policy_available() {
            "The st channel is installed and the managed channel policy is present."
        } else {
            "The st channel is installed. Seats start with Claude's development-channel flag, and st accepts its one-time dialog."
        });
    } else {
        match starts(&program, &environment) {
            Ok(version) => {
                facts.integration = Tri::Yes;
                note.push_str(&format!("{} starts{}.", id, if version.is_empty() { String::new() } else { format!(" ({version})") }));
            }
            Err(error) => return Outcome::Failed(error),
        }
    }
    let mut state = signin(id, &environment, &home);
    if needs_you(state) && matches!(id, "claude" | "codex" | "opencode") {
        reporter.facts(id, Facts { signed_in: state.signed_in, ..facts.clone() });
        reporter.handover(id);
        crate::environment::refresh();
        state = signin(id, &environment, &home);
    }
    facts.signed_in = state.signed_in;
    reporter.facts(id, facts);
    if needs_you(state) {
        Outcome::NeedsLogin(sign_in_words(id).into())
    } else {
        let tail = match state.signed_in {
            Tri::Yes => " You are signed in.",
            _ => " Sign-in is checked when the first agent starts.",
        };
        Outcome::Ready(format!("{note}{tail}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(home: &Path) -> BTreeMap<String, String> {
        BTreeMap::from([("HOME".to_owned(), home.display().to_string())])
    }

    #[test]
    fn claude_is_signed_in_only_when_its_config_says_so() {
        let home = tempfile::tempdir().unwrap();
        let env = environment(home.path());
        let state = signin("claude", &env, home.path());
        assert_eq!((state.first_run, state.signed_in), (Tri::No, Tri::No), "never run");
        std::fs::write(home.path().join(".claude.json"), r#"{"hasCompletedOnboarding":true}"#).unwrap();
        let state = signin("claude", &env, home.path());
        assert_eq!((state.first_run, state.signed_in), (Tri::Yes, Tri::No), "first run done, no account");
        std::fs::write(
            home.path().join(".claude.json"),
            r#"{"hasCompletedOnboarding":true,"oauthAccount":{"emailAddress":"ada@example.com"}}"#,
        )
        .unwrap();
        let state = signin("claude", &env, home.path());
        assert_eq!((state.first_run, state.signed_in), (Tri::Yes, Tri::Yes));
        assert!(!needs_you(state));
        std::fs::write(home.path().join(".claude.json"), "{ not json").unwrap();
        assert_eq!(signin("claude", &env, home.path()).signed_in, Tri::Unknown, "unreadable is unknown, not signed out");
    }

    #[test]
    fn an_api_key_in_the_login_environment_is_not_called_signed_out() {
        let home = tempfile::tempdir().unwrap();
        let mut env = environment(home.path());
        env.insert("ANTHROPIC_API_KEY".into(), "sk-example".into());
        assert_eq!(signin("claude", &env, home.path()).signed_in, Tri::Unknown);
        let mut env = environment(home.path());
        env.insert("OPENAI_API_KEY".into(), "sk-example".into());
        assert_eq!(signin("codex", &env, home.path()).signed_in, Tri::Unknown);
    }

    #[test]
    fn claude_config_dir_moves_where_the_config_is_read() {
        let home = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(
            other.path().join(".claude.json"),
            r#"{"hasCompletedOnboarding":true,"oauthAccount":{}}"#,
        )
        .unwrap();
        let mut env = environment(home.path());
        env.insert("CLAUDE_CONFIG_DIR".into(), other.path().display().to_string());
        assert_eq!(signin("claude", &env, home.path()).signed_in, Tri::Yes);
    }

    #[test]
    fn codex_and_opencode_read_their_own_login_files() {
        let home = tempfile::tempdir().unwrap();
        let env = environment(home.path());
        assert_eq!(signin("codex", &env, home.path()).signed_in, Tri::No);
        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::fs::write(home.path().join(".codex/auth.json"), r#"{"tokens":{}}"#).unwrap();
        assert_eq!(signin("codex", &env, home.path()).signed_in, Tri::Yes);
        assert_eq!(signin("opencode", &env, home.path()).signed_in, Tri::No);
        std::fs::create_dir_all(home.path().join(".local/share/opencode")).unwrap();
        std::fs::write(home.path().join(".local/share/opencode/auth.json"), r#"{"example":{"type":"api"}}"#).unwrap();
        assert_eq!(signin("opencode", &env, home.path()).signed_in, Tri::Yes);
        // Pi and Omp keep sign-in in ways st does not read: it says it does not know.
        assert_eq!(signin("pi", &env, home.path()).signed_in, Tri::Unknown);
        assert_eq!(signin("omp", &env, home.path()).signed_in, Tri::Unknown);
    }

    #[test]
    fn every_supported_harness_has_a_row_and_only_found_ones_start_selected() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["claude", "omp"] {
            let path = bin.join(name);
            std::fs::write(&path, "#!/bin/sh\necho 1.0\n").unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        }
        let mut env = environment(home.path());
        env.insert("PATH".into(), bin.display().to_string());
        let rows = rows(&env);
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["claude", "codex", "opencode", "pi", "omp"]
        );
        let found = rows.iter().filter(|row| row.facts.found.is_some()).map(|row| row.id.as_str()).collect::<Vec<_>>();
        assert_eq!(found, ["claude", "omp"]);
        assert!(rows.iter().all(|row| row.selected == row.facts.found.is_some()));
        assert!(rows.iter().all(|row| row.facts.integration == Tri::Unknown), "nothing has checked an integration yet");
        let claude = &rows[0];
        assert!(claude.sets_up.contains("claude-channel/marketplace"), "{}", claude.sets_up);
        assert!(claude.sets_up.contains(&format!("{}/.claude", home.path().display())), "{}", claude.sets_up);
        assert!(rows[1].install_hint.contains("press r"), "a missing agent says how to get it");
    }

    #[test]
    fn a_program_that_cannot_start_is_reported_whole() {
        let home = tempfile::tempdir().unwrap();
        let broken = home.path().join("codex");
        std::fs::write(&broken, "#!/bin/sh\necho 'cannot load libexample.so' >&2\nexit 3\n").unwrap();
        std::fs::set_permissions(&broken, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let error = starts(&broken, &BTreeMap::new()).unwrap_err();
        assert!(error.contains("cannot load libexample.so"), "{error}");
        assert!(error.contains("exit status: 3") || error.contains("3"), "{error}");
        let good = home.path().join("good");
        std::fs::write(&good, "#!/bin/sh\necho 'good 2.5'\n").unwrap();
        std::fs::set_permissions(&good, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert_eq!(starts(&good, &BTreeMap::new()).unwrap(), "good 2.5");
        assert!(starts(&home.path().join("absent"), &BTreeMap::new()).unwrap_err().contains("could not be started"));
    }
}
