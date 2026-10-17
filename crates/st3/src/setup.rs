//! Establish a person's local configuration before any daemon or seat starts.

use std::fs::{self, File};
use std::io::{IsTerminal as _, Write as _};
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Args;

mod screen;

use st3::client::{Client, Endpoint};
use st3::config::Config;

pub(crate) use crate::onboarding::{capture, said};

macro_rules! say {
    ($($arg:tt)*) => { said(format!($($arg)*)) };
}

#[derive(Args, Clone, Debug, Default)]
pub struct SetupArgs {
    /// Your name: lowercase letters, digits, hyphens and underscores.
    #[arg(long)]
    pub person: Option<String>,
    /// A persistent machine name; `local` is reserved.
    #[arg(long)]
    pub node: Option<String>,
    /// Skip the setup screen: accept defaults for questions not answered by flags.
    #[arg(long)]
    pub yes: bool,
    /// Keep st running as a user service (default: true).
    #[arg(long, action = clap::ArgAction::Set)]
    pub service: Option<bool>,
    /// Install st3, the st link and pty in ~/.local/bin (default: true).
    #[arg(long, action = clap::ArgAction::Set)]
    pub install: Option<bool>,
    /// Start the daemon if stopped (default: true).
    #[arg(long, action = clap::ArgAction::Set)]
    pub start: Option<bool>,
    /// Merge this config file instead of the default config.toml.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Choose an installed harness, or none to skip it.
    #[arg(long, value_parser = ["claude", "codex", "opencode", "pi", "omp", "none"])]
    pub harness: Option<String>,
    /// Install the user-owned Claude channel (default: true; never installs policy).
    #[arg(long, action = clap::ArgAction::Set)]
    pub claude_channel: Option<bool>,
    /// Start onboarding again, including a stopped built-in Smalltalk Assistant.
    #[arg(long)]
    pub onboarding: bool,
    /// Start the built-in Smalltalk Assistant after setup (default: ask, with --yes: yes).
    #[arg(long, action = clap::ArgAction::Set)]
    pub assistant: Option<bool>,
}

pub struct PreparedSetup {
    // Only the binary calls setup now, and it reads `initial_subject` alone; these were public library fields.
    #[allow(dead_code)]
    pub config: Config,
    #[allow(dead_code)]
    pub harness: Option<String>,
    pub initial_subject: Option<String>,
}

fn interactive() -> bool {
    std::env::var_os("ST_AGENT").is_none()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
}

fn require_person_process() -> Result<()> {
    anyhow::ensure!(
        std::env::var_os("ST_AGENT").is_none(),
        "setup is a person command and is unavailable inside an agent seat"
    );
    Ok(())
}

/// Called before entering the TUI runtime. Remote paired-client dispatch skips this.
pub fn prepare_plain_ui() -> Result<Option<String>> {
    require_person_process()?;
    anyhow::ensure!(interactive(), "interactive setup needs a terminal");
    if st3_client::device::Profile::load(&st3_client::device::profile_path()?)?.is_some() {
        return Ok(None);
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let mut config = Config::load_with_fleet(None)?;
            st3::node_identity::resolve(&mut config)?;
            if config.person.is_none() {
                return Ok(run(SetupArgs::default()).await?.initial_subject);
            } else if !daemon_ready(&config).await
                && ask_bool(
                    "The st daemon is not running. Start it?",
                    "--start",
                    None,
                    false,
                    true,
                )?
            {
                start_daemon(
                    &config,
                    &std::env::current_exe()?,
                    &Config::default_path(),
                    true,
                    true,
                )
                .await?;
            }
            // An existing configuration never starts onboarding on its own, whatever its
            // graph holds; `st setup --onboarding` is the explicit way.
            Ok(None)
        })
}

pub async fn run(args: SetupArgs) -> Result<PreparedSetup> {
    require_person_process()?;
    let path = args.config.clone().unwrap_or_else(Config::default_path);
    // A new install records auto; a config that already exists keeps whatever mode it has,
    // including none, which reads as bypass.
    let mut config = if path.try_exists()? {
        Config::load_unvalidated(Some(&path))?
    } else {
        Config {
            access_mode: Some(st_drivers::access::AccessMode::Auto),
            seat_priority: None,
            ..Config::default()
        }
    };
    config.apply_fleet_file()?;
    st3::node_identity::resolve(&mut config)?;
    let first_run = config.person.is_none();
    let previous_node = config.node.clone();
    let existing_store = config.state_dir.join("node-identity.json").exists()
        || config.state_dir.join("claims.sqlite3").exists()
        || config.fleet.is_some();

    let default_person = config
        .person
        .as_deref()
        .and_then(|p| p.strip_prefix("person/"))
        .map(str::to_owned)
        .unwrap_or_else(|| slug(&std::env::var("USER").unwrap_or_else(|_| "ada".into())));
    let default_node = if path.exists() || existing_store {
        config.node.clone()
    } else {
        machine_default(&config.node)
    };
    if interactive() && !args.yes {
        return screen::run(screen::Start {
            args,
            path,
            config,
            previous_node,
            existing_store,
            default_person,
            default_node,
        })
        .await;
    }
    println!("Welcome to Smalltalk.");
    let person = ask_name(
        "Your name",
        "--person",
        args.person.as_deref(),
        &default_person,
        args.yes,
        false,
    )?;
    let node = ask_name(
        "This machine's name",
        "--node",
        args.node.as_deref(),
        &default_node,
        args.yes,
        true,
    )?;
    anyhow::ensure!(
        !existing_store || node == previous_node,
        "this store belongs to `{previous_node}`; setup cannot rename its machine"
    );
    config.person = Some(format!("person/{person}"));
    config.node = node;
    config.validate()?;
    let service = ask_bool(
        "Keep st running in the background (no admin rights needed)?",
        "--service",
        args.service,
        args.yes,
        true,
    )?;
    let install = args.install.unwrap_or(true);
    let start = if daemon_ready(&config).await {
        false
    } else if first_run {
        args.start.unwrap_or(true)
    } else {
        ask_bool(
            "Start the st daemon?",
            "--start",
            args.start,
            args.yes,
            true,
        )?
    };
    st3::service::preflight(service && start)?;
    // Validate every answer before writing, including the persisted machine identity.
    merge_config(&path, &config)?;
    say!("Saved {}", path.display());
    let work_root = config.work_root()?;
    std::fs::create_dir_all(&work_root)
        .with_context(|| format!("create the work folder {}", work_root.display()))?;
    say!(
        "Work folder: {}. Agents work in folders inside it, so none of them has to look through your home folder.",
        work_root.display()
    );
    if let Some(kib) = st3::read_cache::override_kib() {
        say!("Read cache: {kib} KiB per reader.");
    }
    let current_exe = std::env::current_exe()?;
    let executable = if install {
        install_binaries(&current_exe)?
    } else {
        current_exe
    };
    check_login_path(&executable);
    if start {
        start_daemon(&config, &executable, &path, service, true).await?;
    } else if !daemon_ready(&config).await {
        say!("Configuration saved; the daemon remains stopped.");
    }
    let harness = prepare_harness(&config, &executable, &args).await?;
    let initial_subject = if let Some(harness) = &harness {
        if !first_run && !args.onboarding {
            println!(
                "This setup already existed, so the built-in Smalltalk Assistant is not started on its own. Run st setup --onboarding to begin."
            );
            None
        } else if daemon_ready(&config).await {
            if confirm_assistant(&config, harness, &args)? {
                let started = crate::onboarding::start(
                    &config,
                    harness,
                    std::slice::from_ref(harness),
                    args.onboarding,
                )
                .await?;
                if started.is_some() {
                    print_assistant_summary(&config, harness)?;
                }
                started
            } else {
                None
            }
        } else {
            anyhow::ensure!(
                !args.onboarding,
                "rerunning onboarding needs a running daemon; pass --start true"
            );
            say!("Start the st daemon and run st setup to begin onboarding.");
            None
        }
    } else {
        anyhow::ensure!(
            !args.onboarding,
            "rerunning onboarding needs an installed harness"
        );
        None
    };
    print_access_mode(&config);
    Ok(PreparedSetup {
        config,
        harness,
        initial_subject,
    })
}

async fn prepare_harness(
    config: &Config,
    executable: &Path,
    args: &SetupArgs,
) -> Result<Option<String>> {
    if args.harness.as_deref() == Some("none") {
        say!("Harness setup skipped; no onboarding seat was created.");
        return Ok(None);
    }
    let found = if daemon_ready(config).await {
        match Client::new(Endpoint::Unix(config.client_socket()))
            .get::<Vec<String>>("/v1/harnesses")
            .await
        {
            Ok(found) => found,
            // A running daemon from before this route cannot answer; read the login PATH here.
            Err(error)
                if st3::client::is_missing_route(&error) || st3::client::is_not_found(&error) =>
            {
                println!("The running daemon is older than this st; checking the login PATH from here.");
                st3::environment::available_harnesses()?
            }
            Err(error) => return Err(error),
        }
    } else {
        st3::environment::available_harnesses()?
    };
    if let Some(requested) = &args.harness {
        anyhow::ensure!(
            found.contains(requested),
            "{requested} is not installed on the daemon's login PATH; install it and open a new login shell, then run st setup"
        );
    }
    if found.is_empty() {
        say!(
            "No supported harness is installed on the daemon's login PATH. Install Claude Code, Codex, OpenCode, Pi or Omp, then run st setup. No onboarding seat was created."
        );
        return Ok(None);
    }
    let chosen = if let Some(requested) = &args.harness {
        requested.clone()
    } else if found.len() == 1 || args.yes {
        found[0].clone()
    } else {
        anyhow::ensure!(
            interactive(),
            "several harnesses are installed; pass --harness NAME or --yes"
        );
        loop {
            let answer = read_answer(
                &format!(
                    "Which harness ({})",
                    found
                        .iter()
                        .enumerate()
                        .map(|(i, h)| format!("{}: {h}", i + 1))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                &found[0],
            )?;
            if let Some(name) = answer
                .parse::<usize>()
                .ok()
                .and_then(|i| i.checked_sub(1))
                .and_then(|i| found.get(i))
            {
                break name.clone();
            }
            if found.contains(&answer) {
                break answer;
            }
            say!("Choose one of {}.", found.join(", "));
        }
    };
    if chosen == "claude" {
        let install = ask_bool(
            "Install the st Claude channel (no admin rights needed)?",
            "--claude-channel",
            args.claude_channel,
            args.yes,
            true,
        )?;
        if !install {
            say!(
                "Claude user plugin installation skipped; seats use the inline server:st3 development channel. Provider or organization channel restrictions still apply."
            );
            say!("Selected harness: {chosen}");
            return Ok(Some(chosen));
        }
        // CLI plugin commands must see the same account login environment as the daemon.
        if let Err(error) = ensure_claude_channel(executable) {
            say!("Claude channel installation failed: {error}");
            return Ok(fallback_harness(&found));
        }
        if st_drivers::claude_channel::st3_policy_available() {
            say!(
                "Claude channel policy is present; seats use --channels plugin:st-channel@st."
            );
        } else {
            say!(
                "Claude channel uses --dangerously-load-development-channels plugin:st-channel@st. st accepts its local-development dialog when starting a seat. Provider or organization channel restrictions still apply.\nOptional administrator approval policy: sudo st claude-channel install-policy"
            );
        }
    }
    say!("Selected harness: {chosen}");
    Ok(Some(chosen))
}

/// Make sure the st Claude channel is installed for this account; the error is the install
/// command's own stderr, whole.
pub(crate) fn ensure_claude_channel(executable: &Path) -> std::result::Result<(), String> {
    let environment = st3::environment::snapshot().map_err(|error| format!("{error:#}"))?;
    let status = Command::new(executable)
        .args(["claude-channel", "status"])
        .envs(&environment)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    if status.status.success() {
        return Ok(());
    }
    let output = Command::new(executable)
        .args(["claude-channel", "install", "--no-policy"])
        .envs(&environment)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

fn fallback_harness(found: &[String]) -> Option<String> {
    let next = found.iter().find(|h| h.as_str() != "claude").cloned();
    if let Some(next) = &next {
        say!("Selected harness: {next}");
    } else {
        say!("No usable harness remains; no onboarding seat was created.");
    }
    next
}

/// Ask before the built-in Smalltalk Assistant starts: it is an agent running on the person's own
/// provider account. An explicit `--onboarding` already names the request.
fn confirm_assistant(config: &Config, harness: &str, args: &SetupArgs) -> Result<bool> {
    if args.onboarding {
        return Ok(true);
    }
    println!(
        "The built-in Smalltalk Assistant is an agent that runs on your own {harness} account, so it uses your {harness} usage and quota. It runs in {} access mode.",
        config.active_access_mode().as_str()
    );
    let start = ask_bool(
        "Start the built-in Smalltalk Assistant now?",
        "--assistant",
        args.assistant,
        args.yes,
        true,
    )?;
    if !start {
        println!("Not started. Run st setup --onboarding when you want it.");
    }
    Ok(start)
}

/// Name what starting the Assistant created and how to stop it.
fn print_assistant_summary(config: &Config, harness: &str) -> Result<()> {
    let person = config
        .person
        .as_deref()
        .context("setup needs a person")?;
    println!(
        "Created: the agent agent/st/assistant ({harness}, working in {}), the guide document doc/st/guide, and a run of mission/st/onboarding.\nStop it with: st agents stop agent/st/assistant --as {person}",
        crate::onboarding::workspace(config)?.display()
    );
    Ok(())
}

/// Say what the install's access mode does, as it will apply to agents the daemon starts.
fn print_access_mode(config: &Config) {
    use st_drivers::access::AccessMode;
    match config.active_access_mode() {
        AccessMode::Auto => println!(
            "Access mode: auto. Agents use their provider's managed permission checks. Auto reduces accidents; it is not a security boundary."
        ),
        AccessMode::Bypass => println!(
            "Access mode: bypass. Agents run without permission prompts. Bypass is not a security boundary."
        ),
    }
}

fn slug(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(63)
        .collect()
}

fn validate_name(value: &str, machine: bool) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= 63
            && value
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_'),
        "use 1–63 lowercase letters, digits, hyphens or underscores"
    );
    anyhow::ensure!(
        !machine || value != "local",
        "`local` is reserved; choose a machine name such as studio"
    );
    Ok(())
}

fn machine_default(hostname: &str) -> String {
    #[cfg(target_os = "macos")]
    let hostname = Command::new("scutil")
        .args(["--get", "LocalHostName"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| hostname.into());
    #[cfg(target_os = "macos")]
    let hostname = hostname.as_str();
    let name = slug(hostname);
    if name.is_empty() || name == "local" || name == "localhost" {
        "studio".into()
    } else {
        name
    }
}

fn read_answer(question: &str, default: &str) -> Result<String> {
    print!("{question} [{default}]: ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    anyhow::ensure!(
        std::io::stdin().read_line(&mut answer)? != 0,
        "setup cancelled: input closed"
    );
    let answer = answer.trim();
    Ok(if answer.is_empty() {
        default.into()
    } else {
        answer.into()
    })
}

fn ask_name(
    question: &str,
    flag: &str,
    answer: Option<&str>,
    default: &str,
    yes: bool,
    machine: bool,
) -> Result<String> {
    if let Some(answer) = answer.or(yes.then_some(default)) {
        validate_name(answer, machine).with_context(|| format!("invalid {flag}"))?;
        return Ok(answer.into());
    }
    anyhow::ensure!(
        interactive(),
        "pass {flag} or --yes; setup never prompts without a terminal"
    );
    loop {
        let answer = read_answer(question, default)?;
        match validate_name(&answer, machine) {
            Ok(()) => return Ok(answer),
            Err(error) => say!("{error}"),
        }
    }
}

fn ask_bool(
    question: &str,
    flag: &str,
    answer: Option<bool>,
    yes: bool,
    default: bool,
) -> Result<bool> {
    if let Some(answer) = answer.or(yes.then_some(default)) {
        return Ok(answer);
    }
    anyhow::ensure!(
        interactive(),
        "pass {flag} true|false or --yes; setup never prompts without a terminal"
    );
    loop {
        match read_answer(question, if default { "Y/n" } else { "y/N" })?
            .to_ascii_lowercase()
            .as_str()
        {
            "y/n" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => say!("Answer yes or no."),
        }
    }
}

fn merge_config(path: &Path, config: &Config) -> Result<()> {
    let mut created = false;
    let mut table = match fs::read_to_string(path) {
        Ok(text) => toml::from_str::<toml::Table>(&text).context("parse existing setup config")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            created = true;
            toml::Table::new()
        }
        Err(error) => return Err(error).context("read existing setup config"),
    };
    // Only a config setup creates gets the install access mode; an existing file is never given
    // a key it did not have.
    if created && let Some(mode) = config.access_mode {
        table.insert("access_mode".into(), mode.as_str().into());
    }
    table.insert(
        "person".into(),
        config
            .person
            .clone()
            .context("setup needs a person")?
            .into(),
    );
    table.insert("node".into(), config.node.clone().into());
    if let Some(on) = config.codex_work_without_asking {
        table.insert("codex_work_without_asking".into(), on.into());
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut pending = tempfile::NamedTempFile::new_in(parent)?;
    pending.write_all(toml::to_string_pretty(&table)?.as_bytes())?;
    pending.as_file().sync_all()?;
    pending
        .persist(path)
        .with_context(|| format!("save config {}", path.display()))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn same_file(left: &Path, right: &Path) -> bool {
    fs::canonicalize(left)
        .ok()
        .zip(fs::canonicalize(right).ok())
        .is_some_and(|(a, b)| a == b)
}

fn copy_executable(source: &Path, target: &Path) -> Result<()> {
    if same_file(source, target) {
        return Ok(());
    }
    let mut pending =
        tempfile::NamedTempFile::new_in(target.parent().context("executable directory")?)?;
    std::io::copy(&mut File::open(source)?, &mut pending)?;
    pending
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o755))?;
    pending.as_file().sync_all()?;
    pending
        .persist(target)
        .with_context(|| format!("install {}", target.display()))?;
    Ok(())
}

fn find_pty(executable: &Path) -> Result<PathBuf> {
    let sibling = executable
        .parent()
        .context("st executable directory")?
        .join("pty");
    if sibling.is_file() {
        return Ok(sibling);
    }
    let environment = st3::environment::snapshot()?;
    st_runtime::resolve_executable("pty", &environment)
        .context("pty is missing; unpack the complete st archive (st3, st and pty) before setup")
}

fn install_binaries(executable: &Path) -> Result<PathBuf> {
    // An instance always gets its own copies, below its own directory, so removing the
    // directory removes them and the default install is never the one that runs.
    let instance = st3::instance::current();
    // Keep managed installations (including the signed macOS app bundle) in place.
    if instance.is_none()
        && let Ok(environment) = st3::environment::snapshot()
        && st_runtime::resolve_executable("st", &environment)
            .ok()
            .is_some_and(|st| same_file(&st, executable))
    {
        return Ok(executable.into());
    }
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let bin = match &instance {
        Some(instance) => instance.bin_dir(),
        None => home.join(".local/bin"),
    };
    // Locate pty before changing any installed file.
    let pty = find_pty(executable)?;
    fs::create_dir_all(&bin)?;
    let target = bin.join("st3");
    let replacing = [
        ("st3", fs::symlink_metadata(&target).is_ok() && !same_file(executable, &target)),
        ("pty", fs::symlink_metadata(bin.join("pty")).is_ok() && !same_file(&pty, &bin.join("pty"))),
        (
            "st",
            fs::symlink_metadata(bin.join("st")).is_ok()
                && fs::read_link(bin.join("st")).ok().as_deref() != Some(Path::new("st3")),
        ),
    ]
    .into_iter()
    .filter_map(|(name, exists)| exists.then_some(name))
    .collect::<Vec<_>>();
    if replacing.is_empty() {
        println!("Installing st3, st and pty in {}", bin.display());
    } else {
        println!(
            "Installing st3, st and pty in {}, replacing the existing {}",
            bin.display(),
            replacing.join(", ")
        );
    }
    copy_executable(executable, &target)?;
    copy_executable(&pty, &bin.join("pty"))?;
    let link = bin.join("st");
    if fs::read_link(&link).ok().as_deref() != Some(Path::new("st3")) {
        let pending = tempfile::tempdir_in(&bin)?;
        symlink("st3", pending.path().join("st"))?;
        fs::rename(pending.path().join("st"), &link)?;
    }
    File::open(&bin)?.sync_all()?;
    record_installed_binaries(&bin)?;
    say!("Installed st3, st and pty in {}", bin.display());
    Ok(target)
}

fn record_installed_binaries(bin: &Path) -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
        .join("st3");
    fs::create_dir_all(&data)?;
    let bin = fs::canonicalize(bin)?;
    let manifest = serde_json::json!({
        "version": 1,
        "source": "setup",
        "files": crate::fractal::installed_binaries(vec![
            bin.join("st3"), bin.join("st"), bin.join("pty"),
        ]),
    });
    let mut pending = tempfile::NamedTempFile::new_in(&data)?;
    pending.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    pending.as_file().sync_all()?;
    pending
        .persist(data.join("install.json"))
        .context("record setup-installed executables for st uninstall")?;
    File::open(&data)?.sync_all()?;
    Ok(())
}

fn check_login_path(executable: &Path) {
    // An instance is run by name from its own directory; nothing belongs on the login PATH.
    if let Some(instance) = st3::instance::current() {
        println!(
            "This instance's commands are in {}. Run them with: {} --instance {}",
            instance.bin_dir().display(),
            instance.bin_dir().join("st").display(),
            instance.name()
        );
        return;
    }
    match st3::environment::snapshot() {
        Ok(environment) => {
            if st_runtime::resolve_executable("st", &environment)
                .ok()
                .is_some_and(|st| same_file(&st, executable))
            {
                return;
            }
            say!(
                "Add this line to your login shell profile, then open a new terminal:\n  export PATH=\"$HOME/.local/bin:$PATH\""
            );
        }
        Err(error) => say!(
            "Could not check the login shell PATH: {error:#}\nAdd this line to your login shell profile:\n  export PATH=\"$HOME/.local/bin:$PATH\""
        ),
    }
}

async fn daemon_ready(config: &Config) -> bool {
    let client = Client::new(Endpoint::Unix(config.client_socket()));
    tokio::time::timeout(
        Duration::from_millis(750),
        client.get::<serde_json::Value>("/v1/health"),
    )
    .await
    .is_ok_and(|r| r.is_ok())
}

#[cfg(target_os = "linux")]
fn try_linger() {
    let user = std::env::var("USER").ok().filter(|user| !user.is_empty());
    // Never allow a polkit/password conversation to open in setup.
    // Lingering belongs to the real user session, never to an instance's environment.
    let mut command = st3::service::host_command("loginctl");
    command.args(["--no-ask-password", "enable-linger"]);
    if let Some(user) = &user {
        command.arg(user);
    }
    let enabled = command
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success());
    if enabled {
        say!("Enabled lingering; st can keep running after you log out.");
    } else {
        let quoted = user
            .map(|user| format!(" '{}'", user.replace('\'', "'\\''")))
            .unwrap_or_default();
        say!(
            "Lingering was not enabled. To keep st running after logout, run:\n  loginctl enable-linger{quoted}"
        );
    }
}

async fn start_daemon(
    config: &Config,
    executable: &Path,
    path: &Path,
    service: bool,
    linger: bool,
) -> Result<()> {
    st3::service::preflight(service)?;
    if daemon_ready(config).await {
        return Ok(());
    }
    if service {
        let result = Command::new(executable)
            .args(["service", "install", "--config"])
            .arg(path)
            .stdin(Stdio::null())
            .output();
        match result {
            Ok(output) if output.status.success() => {
                say!("Installed the st user service; it starts when you log in.");
                #[cfg(target_os = "linux")]
                if linger {
                    try_linger();
                }
                #[cfg(not(target_os = "linux"))]
                let _ = linger;
                anyhow::ensure!(
                    daemon_ready(config).await,
                    "the installed service did not answer at {}",
                    config.client_socket().display()
                );
                return Ok(());
            }
            Ok(output) => {
                // A failed install may already have started the daemon. Do not race it.
                if daemon_ready(config).await {
                    return Ok(());
                }
                say!(
                    "The user service is unavailable: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Err(error) => say!("The user service is unavailable: {error}"),
        }
    }
    let pty = find_pty(executable)?;
    let logs = config.state_dir.join("logs");
    fs::create_dir_all(&logs)?;
    let log_path = logs.join("setup-daemon.log");
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let mut command = Command::new(executable);
    command
        .args(["up", "--config"])
        .arg(path)
        .arg("--pty-binary")
        .arg(pty)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    if let Some(kib) = st3::read_cache::override_kib() {
        command.env("SMALLCLAIMS_READ_CACHE_KIB", kib.to_string());
    }
    // SAFETY: setsid is async-signal-safe; no allocation or environment changes after fork.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn().context("start the detached st daemon")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if daemon_ready(config).await {
            say!(
                "Started st without a service. It stops when you reboot.\nRun `st service install` to keep it running."
            );
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            anyhow::bail!(
                "st daemon exited with {status}; read {}",
                log_path.display()
            );
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "st daemon did not become ready; read {}",
                log_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_defaults_are_safe_and_local_is_reserved() {
        for name in ["ada", "studio-2", "ada_lovelace"] {
            validate_name(name, true).unwrap();
        }
        for name in [
            "",
            "local",
            "Ada",
            "ada lovelace",
            "studio's",
            "a/b",
            "$(echo secret)",
        ] {
            assert!(validate_name(name, true).is_err(), "{name}");
        }
        assert!(validate_name(&"a".repeat(64), true).is_err());
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(machine_default("local"), "studio");
            assert_eq!(machine_default("localhost"), "studio");
        }
        assert_eq!(slug("Ada Lovelace's Studio"), "ada-lovelace-s-studio");
    }

    #[test]
    fn merge_preserves_paths_and_nested_settings() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("config.toml");
        fs::write(
            &path,
            "state_dir = '/srv/st/state'\nsocket = '/srv/st/run/st.sock'\n[planner]\nprovider = 'codex'\nmodel = 'example-model'\n[observations]\nretention = '2d'\n",
        )?;
        let config = Config {
            person: Some("person/ada".into()),
            node: "studio".into(),
            ..Config::default()
        };
        merge_config(&path, &config)?;
        let merged: toml::Table = toml::from_str(&fs::read_to_string(&path)?)?;
        assert_eq!(merged["person"].as_str(), Some("person/ada"));
        assert_eq!(merged["node"].as_str(), Some("studio"));
        assert_eq!(merged["state_dir"].as_str(), Some("/srv/st/state"));
        assert_eq!(merged["planner"]["model"].as_str(), Some("example-model"));
        assert_eq!(merged["observations"]["retention"].as_str(), Some("2d"));
        let bytes = fs::read(&path)?;
        merge_config(&path, &config)?;
        assert_eq!(fs::read(&path)?, bytes);
        Ok(())
    }

    #[test]
    fn replacing_an_installed_binary_is_atomic_and_repeating_is_safe() -> Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let target = root.path().join("st3");
        fs::write(&source, b"candidate")?;
        fs::write(&target, b"old")?;
        let open_old = File::open(&target)?;
        copy_executable(&source, &target)?;
        assert_eq!(fs::read(&target)?, b"candidate");
        assert_eq!(open_old.metadata()?.len(), 3);
        copy_executable(&target, &target)?;
        assert_eq!(fs::metadata(&target)?.permissions().mode() & 0o777, 0o755);
        Ok(())
    }
}
