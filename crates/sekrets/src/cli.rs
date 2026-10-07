//! `sekrets`: run any command through the gateway, and manage profiles, grants and locks.

use std::io::{IsTerminal as _, Read as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use super::client::{Connection, socket_path};
use super::policy::{PRESETS, Policy};
use super::protocol::{Attestation, CallerView, Request, RunRequest};

#[derive(Args, Debug)]
#[command(
    args_conflicts_with_subcommands = true,
    after_help = "Run a command:  sekrets [--profile P] -- gh pr create --draft\n\
                  A command runs as the sekrets user with the profile's home and environment, \
                  under its allow and deny policy; the caller never reads the credential."
)]
pub struct SekretsArgs {
    #[command(subcommand)]
    command: Option<SekretsCommand>,
    /// The profile to run as. Without it: a person's default profile, or the one profile granted
    /// to this seat that allows the command.
    #[arg(long)]
    profile: Option<String>,
    /// The command and its arguments, after `--`.
    #[arg(last = true, value_name = "COMMAND")]
    argv: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum SekretsCommand {
    /// Run a tool's own login as the sekrets user, so the credential lands in a profile.
    Login(LoginArgs),
    /// Put a value read from standard input into a profile, as an environment variable its
    /// commands get. Nothing reads it back out.
    Put(PutArgs),
    /// Remove a value from a profile.
    Unset(PutArgs),
    /// Stop all use of sekrets, or one person's.
    Lock(LockArgs),
    /// Allow use again.
    Unlock(UnlockArgs),
    /// Show who the gateway takes this caller for.
    Whoami,
    /// List profiles this caller may use.
    Profiles,
    /// Create, show, change or remove a profile.
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Give an agent or another person the use of a profile, narrowed by a policy.
    Grant(GrantArgs),
    /// Remove a grant.
    Revoke { grant: String },
    /// List grants of this person's profiles, and grants to this caller.
    Grants,
    /// Show the calls and changes this person may read.
    Log(LogArgs),
    /// List policy presets.
    Presets,
    /// Register this host's daemon key with the gateway, so seats can be told from their person.
    /// Run from a login session.
    Enable,
    /// Print the root commands that create the sekrets user, its store and the gateway service.
    Setup(SetupArgs),
    /// Move a tool onto sekrets for your agents, one step at a time. Today: gh, step 1 (every
    /// seat's gh runs through sekrets; your shell and st's daemon keep gh as it is).
    Adopt(AdoptArgs),
    /// Undo `adopt`: remove the shim, so seats run the tool directly again.
    Unadopt(AdoptArgs),
    /// Serve the gateway. Run as the sekrets user, by its service.
    #[command(hide = true)]
    Serve {
        #[arg(long, default_value = super::gateway::DEFAULT_CONFIG)]
        config: PathBuf,
    },
}

#[derive(Args, Debug)]
struct LoginArgs {
    #[arg(long)]
    profile: String,
    /// The tool, such as gh. Arguments after `--` replace its default login arguments.
    tool: String,
    #[arg(last = true)]
    args: Vec<String>,
}

#[derive(Args, Debug)]
struct PutArgs {
    /// The environment variable name, such as GH_TOKEN.
    name: String,
    #[arg(long)]
    profile: String,
}

#[derive(Args, Debug)]
struct LockArgs {
    /// Lock only this person's use, such as person/ada. Without it, everyone's.
    person: Option<String>,
    #[arg(long)]
    reason: Option<String>,
}

#[derive(Args, Debug)]
struct UnlockArgs {
    person: Option<String>,
}

#[derive(Args, Debug, Clone)]
struct PolicyArgs {
    /// A named policy; repeat to combine. See `sekrets presets`.
    #[arg(long = "preset")]
    presets: Vec<String>,
    /// Allow commands that start with these whole arguments, such as "gh pr view".
    #[arg(long)]
    allow: Vec<String>,
    /// Deny commands that start with these arguments, or with options after them, such as
    /// "gh pr create --body-file,-F".
    #[arg(long)]
    deny: Vec<String>,
}

impl PolicyArgs {
    fn policy(&self) -> Result<Policy> {
        Policy::build(&self.presets, &self.allow, &self.deny).map_err(anyhow::Error::msg)
    }
}

#[derive(Subcommand, Debug)]
enum ProfileCommand {
    /// Create a profile you own, with its command policy.
    Create {
        /// OWNER/NAME, such as ada/agent-gh.
        profile: String,
        #[command(flatten)]
        policy: PolicyArgs,
        #[arg(long)]
        description: Option<String>,
        /// Use this profile when its owner names none.
        #[arg(long)]
        default: bool,
    },
    /// Show a profile you own or were granted, with its policy and the names of its values.
    Show { profile: String },
    /// Replace a profile's policy.
    Policy {
        profile: String,
        #[command(flatten)]
        policy: PolicyArgs,
    },
    /// Remove a profile, its home and its values, and end its grants.
    Rm { profile: String },
}

#[derive(Args, Debug)]
struct GrantArgs {
    profile: String,
    /// The agent or person, or a pattern such as agent/web/**.
    #[arg(long)]
    to: String,
    #[command(flatten)]
    policy: PolicyArgs,
    /// When the grant ends: a date (2026-11-01) or an RFC 3339 time.
    #[arg(long)]
    until: Option<String>,
}

#[derive(Args, Debug)]
struct LogArgs {
    #[arg(long, default_value_t = 0)]
    after: i64,
    #[arg(long, default_value_t = 50)]
    limit: i64,
    /// Keep printing new entries.
    #[arg(long)]
    follow: bool,
}

#[derive(Args, Debug)]
struct AdoptArgs {
    /// The tool: gh.
    tool: String,
    /// The profile your agents use; PERSON/agent-gh when omitted.
    #[arg(long)]
    agent_profile: Option<String>,
    /// Where the shim goes; ~/.local/bin when omitted.
    #[arg(long)]
    bin_dir: Option<PathBuf>,
    /// Create a missing profile or grant without asking.
    #[arg(long)]
    yes: bool,
}

impl AdoptArgs {
    fn adopt(&self) -> Result<super::adopt::Adopt> {
        if self.tool != "gh" {
            bail!("sekrets adopts gh only, for now");
        }
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(super::adopt::Adopt {
            socket: socket_path(),
            bin_dir: self
                .bin_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from(home).join(".local/bin")),
            agent_profile: self.agent_profile.clone(),
            yes: self.yes,
            sekrets: std::fs::canonicalize(std::env::current_exe()?)?,
            path: std::env::var_os("PATH").unwrap_or_default(),
        })
    }
}

#[derive(Args, Debug)]
struct SetupArgs {
    /// The person this host's Unix user is, such as person/ada.
    #[arg(long)]
    person: String,
    /// That person's Unix user ID; this user's when omitted.
    #[arg(long)]
    uid: Option<u32>,
    /// More people on a shared host: UID=person/NAME.
    #[arg(long = "also")]
    also: Vec<String>,
    /// The sekrets binary root copies for the gateway; this one when omitted.
    #[arg(long)]
    st: Option<PathBuf>,
    /// Directories callers' checkouts live under.
    #[arg(long = "checkout-root", default_value = "/home")]
    checkout_roots: Vec<PathBuf>,
}

/// Exit code for the caller: the command's own, or 1 for st's errors.
pub fn run(args: SekretsArgs, json_output: bool) -> Result<i32> {
    let Some(command) = args.command else {
        if args.argv.is_empty() {
            bail!(
                "name a command after `--`, such as `sekrets -- gh pr list`, or see `sekrets --help`"
            );
        }
        let connection = identified_connection()?;
        return connection.run(RunRequest {
            profile: args.profile,
            argv: args.argv,
            ..RunRequest::default()
        });
    };
    match command {
        SekretsCommand::Serve { config } => {
            super::gateway::serve(&config)?;
            Ok(0)
        }
        SekretsCommand::Setup(setup) => {
            print!("{}", setup_script(&setup)?);
            Ok(0)
        }
        SekretsCommand::Adopt(args) => {
            let lines = args.adopt()?.gh(&mut super::adopt::ask_terminal)?;
            for line in &lines {
                println!("{line}");
            }
            Ok(i32::from(
                lines
                    .iter()
                    .any(|line| line.state == super::adopt::State::Needs),
            ))
        }
        SekretsCommand::Unadopt(args) => {
            println!("{}", args.adopt()?.unadopt_gh()?);
            Ok(0)
        }
        SekretsCommand::Presets => {
            for (name, description) in PRESETS {
                println!("{name:<24} {description}");
            }
            Ok(0)
        }
        SekretsCommand::Login(login) => {
            let mut argv = vec![login.tool.clone()];
            if login.args.is_empty() {
                argv.extend(default_login(&login.tool)?.iter().map(|a| (*a).to_owned()));
            } else {
                argv.extend(login.args);
            }
            let connection = identified_connection()?;
            connection.run(RunRequest {
                profile: Some(login.profile),
                argv,
                login: true,
                ..RunRequest::default()
            })
        }
        SekretsCommand::Whoami => {
            let connection = identified_connection()?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "gateway": connection.gateway,
                        "caller": connection.caller,
                    }))?
                );
            } else {
                println!("{}", describe(&connection.caller));
            }
            Ok(0)
        }
        SekretsCommand::Enable => {
            let connection = identified_connection()?;
            let node = st_json(&["sekrets-node"]).context("ask st for this node's key")?;
            let (Some(node_name), Some(key)) = (node["node"].as_str(), node["key"].as_str()) else {
                bail!("the daemon did not say its node key");
            };
            let value = connection.manage(&Request::Register {
                node: node_name.into(),
                key: key.into(),
            })?;
            print_value(&value, json_output, |value| {
                format!(
                    "Seats of {} on {} can now use the profiles granted to them.",
                    value["person"].as_str().unwrap_or("?"),
                    value["node"].as_str().unwrap_or("?")
                )
            })
        }
        other => {
            let connection = identified_connection()?;
            let request = management_request(other)?;
            let value = connection.manage(&request)?;
            print_value(&value, json_output, |value| render(&request, value))
        }
    }
}

fn management_request(command: SekretsCommand) -> Result<Request> {
    Ok(match command {
        SekretsCommand::Put(put) => {
            let mut value = String::new();
            if std::io::stdin().is_terminal() {
                eprint!("Value for {} (input is not shown): ", put.name);
                value = read_hidden_line()?;
                eprintln!();
            } else {
                std::io::stdin().read_to_string(&mut value)?;
            }
            let value = value.trim_end_matches(['\n', '\r']).to_owned();
            Request::Put {
                profile: put.profile,
                name: put.name,
                value,
            }
        }
        SekretsCommand::Unset(put) => Request::Unset {
            profile: put.profile,
            name: put.name,
        },
        SekretsCommand::Lock(lock) => Request::Lock {
            person: lock.person,
            reason: lock.reason,
        },
        SekretsCommand::Unlock(unlock) => Request::Unlock {
            person: unlock.person,
        },
        SekretsCommand::Profiles => Request::ProfileList,
        SekretsCommand::Profile { command } => match command {
            ProfileCommand::Create {
                profile,
                policy,
                description,
                default,
            } => Request::ProfileCreate {
                profile,
                description,
                policy: policy.policy()?,
                default,
            },
            ProfileCommand::Show { profile } => Request::ProfileShow { profile },
            ProfileCommand::Policy { profile, policy } => Request::PolicySet {
                profile,
                policy: policy.policy()?,
            },
            ProfileCommand::Rm { profile } => Request::ProfileRemove { profile },
        },
        SekretsCommand::Grant(grant) => Request::GrantAdd {
            profile: grant.profile,
            to: grant.to,
            policy: grant.policy.policy()?,
            until_unix_ms: grant.until.as_deref().map(parse_until).transpose()?,
        },
        SekretsCommand::Revoke { grant } => Request::GrantRemove { grant },
        SekretsCommand::Grants => Request::GrantList,
        SekretsCommand::Log(log) => {
            return Ok(Request::Log {
                after: log.after,
                limit: log.limit,
                wait_ms: if log.follow { 60_000 } else { 0 },
            });
        }
        SekretsCommand::Whoami
        | SekretsCommand::Enable
        | SekretsCommand::Login(_)
        | SekretsCommand::Presets
        | SekretsCommand::Setup(_)
        | SekretsCommand::Adopt(_)
        | SekretsCommand::Unadopt(_)
        | SekretsCommand::Serve { .. } => unreachable!("handled before"),
    })
}

/// Connect, and when this process is a seat, bring its daemon's attestation.
fn identified_connection() -> Result<Connection> {
    let mut connection = Connection::open(&socket_path())?;
    if let CallerView::Unidentified { .. } = connection.caller
        && std::env::var("ST_AGENT").is_ok_and(|agent| agent.starts_with("agent/"))
    {
        let attestation: Attestation = serde_json::from_value(
            st_json(&["sekrets-attest", "--nonce", &connection.nonce])
                .context("ask this seat's daemon to vouch for it")?,
        )?;
        connection.hello(Some(attestation))?;
    }
    Ok(connection)
}

/// Run st, which knows its own daemon, and read the JSON it prints. A seat's st is
/// `ST3_BIN`; otherwise `st` on the path. st asks its daemon about this process: its parent.
fn st_json(args: &[&str]) -> Result<Value> {
    let st = std::env::var_os("ST3_BIN").unwrap_or_else(|| "st".into());
    let output = std::process::Command::new(&st)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("run {}", std::path::Path::new(&st).display()))?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn describe(caller: &CallerView) -> String {
    match caller {
        CallerView::Person { person } => format!("{person}, from a login session"),
        CallerView::Agent { agent, person } => format!("{agent}, working for {person}"),
        CallerView::Host { node, person } => format!("{node}'s st, working for {person}"),
        CallerView::Unidentified { person, reason } => {
            format!("unidentified process of {person}: {reason}")
        }
    }
}

/// The login arguments for tools sekrets knows; any other tool names its own after `--`.
fn default_login(tool: &str) -> Result<&'static [&'static str]> {
    Ok(match tool {
        "gh" => &[
            "auth",
            "login",
            "--hostname",
            "github.com",
            "--git-protocol",
            "https",
        ],
        _ => {
            bail!("sekrets has no default login for `{tool}`; give its login arguments after `--`")
        }
    })
}

fn parse_until(text: &str) -> Result<i64> {
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(text) {
        return Ok(time.timestamp_millis());
    }
    let date = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .with_context(|| format!("`{text}` is neither a date (2026-11-01) nor an RFC 3339 time"))?;
    Ok(date
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
        .timestamp_millis())
}

fn read_hidden_line() -> Result<String> {
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    let hidden = unsafe { libc::tcgetattr(0, &mut saved) } == 0;
    if hidden {
        let mut quiet = saved;
        quiet.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &quiet) };
    }
    let mut line = String::new();
    let result = std::io::stdin().read_line(&mut line);
    if hidden {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
    }
    result?;
    Ok(line)
}

fn print_value(value: &Value, json_output: bool, human: impl Fn(&Value) -> String) -> Result<i32> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{}", human(value));
    }
    Ok(0)
}

fn render_policy(policy: &Value) -> String {
    let rules = |key: &str| {
        policy[key]
            .as_array()
            .map(|rules| {
                rules
                    .iter()
                    .map(|rule| {
                        let mut text = rule["prefix"]
                            .as_array()
                            .map(|words| {
                                words
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                            .unwrap_or_default();
                        if let Some(options) = rule["options"].as_array() {
                            let options =
                                options.iter().filter_map(Value::as_str).collect::<Vec<_>>();
                            if !options.is_empty() {
                                text.push(' ');
                                text.push_str(&options.join(","));
                            }
                        }
                        text
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let mut lines = Vec::new();
    if let Some(presets) = policy["presets"].as_array()
        && !presets.is_empty()
    {
        lines.push(format!(
            "  presets  {}",
            presets
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for rule in rules("allow") {
        lines.push(format!("  allow    {rule}"));
    }
    for rule in rules("deny") {
        lines.push(format!("  deny     {rule}"));
    }
    lines.join("\n")
}

fn render(request: &Request, value: &Value) -> String {
    match request {
        Request::ProfileList | Request::ProfileShow { .. } => {
            let Some(items) = value.as_array() else {
                return value.to_string();
            };
            if items.is_empty() {
                return "No profiles.".into();
            }
            items
                .iter()
                .map(|item| {
                    let profile = &item["profile"];
                    let mut text = format!(
                        "{}  owner {}{}",
                        profile["id"].as_str().unwrap_or("?"),
                        profile["owner"].as_str().unwrap_or("?"),
                        if profile["default"].as_bool() == Some(true) {
                            "  default"
                        } else {
                            ""
                        }
                    );
                    if let Some(description) = profile["description"].as_str() {
                        text.push_str(&format!("\n  {description}"));
                    }
                    if let Some(env) = profile["env"].as_array()
                        && !env.is_empty()
                    {
                        text.push_str(&format!(
                            "\n  values   {}",
                            env.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    text.push('\n');
                    text.push_str(&render_policy(&profile["policy"]));
                    if item["grant"].is_object() {
                        text.push_str(&format!(
                            "\n  granted by {} as {}\n{}",
                            item["grant"]["granted_by"].as_str().unwrap_or("?"),
                            item["grant"]["id"].as_str().unwrap_or("?"),
                            render_policy(&item["grant"]["policy"])
                        ));
                    }
                    text
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        }
        Request::GrantList => {
            let Some(items) = value.as_array() else {
                return value.to_string();
            };
            if items.is_empty() {
                return "No grants.".into();
            }
            items
                .iter()
                .map(|grant| {
                    format!(
                        "{}  {} → {}{}\n{}",
                        grant["id"].as_str().unwrap_or("?"),
                        grant["profile"].as_str().unwrap_or("?"),
                        grant["grantee"].as_str().unwrap_or("?"),
                        grant["until_unix_ms"]
                            .as_i64()
                            .and_then(chrono::DateTime::from_timestamp_millis)
                            .map(|until| format!("  until {}", until.format("%Y-%m-%d %H:%M UTC")))
                            .unwrap_or_default(),
                        render_policy(&grant["policy"])
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        }
        Request::GrantAdd { .. } => format!(
            "Granted {} to {} as {}.",
            value["profile"].as_str().unwrap_or("?"),
            value["grantee"].as_str().unwrap_or("?"),
            value["id"].as_str().unwrap_or("?")
        ),
        Request::Log { .. } => {
            let Some(entries) = value.as_array() else {
                return value.to_string();
            };
            entries
                .iter()
                .map(|entry| {
                    let at = entry["at_unix_ms"]
                        .as_i64()
                        .and_then(chrono::DateTime::from_timestamp_millis)
                        .map(|at| at.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_default();
                    let detail = &entry["detail"];
                    let argv = detail["argv"]
                        .as_array()
                        .map(|argv| {
                            argv.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or_default();
                    let outcome = if let Some(reason) = detail["reason"].as_str() {
                        format!("refused: {reason}")
                    } else if let Some(code) = detail["code"].as_i64() {
                        format!("call {} exit {code}", detail["call"])
                    } else if let Some(error) = detail["error"].as_str() {
                        format!("call {} failed: {error}", detail["call"])
                    } else {
                        String::new()
                    };
                    format!(
                        "{:>5} {at} {:<18} {:<28} {:<14} {argv} {outcome}",
                        entry["seq"],
                        entry["event"].as_str().unwrap_or("?"),
                        entry["actor"].as_str().unwrap_or("-"),
                        entry["profile"].as_str().unwrap_or("-"),
                    )
                    .trim_end()
                    .to_owned()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => serde_json::to_string_pretty(value).unwrap_or_default(),
    }
}

fn setup_script(setup: &SetupArgs) -> Result<String> {
    let person = &setup.person;
    if super::store::person_name(person).is_none() {
        bail!("--person must be a person such as person/ada");
    }
    let uid = setup.uid.unwrap_or_else(|| unsafe { libc::getuid() });
    let mut people = vec![(uid, person.clone())];
    for also in &setup.also {
        let (uid, person) = also
            .split_once('=')
            .context("--also takes UID=person/NAME")?;
        let uid = uid.parse::<u32>().context("--also takes UID=person/NAME")?;
        if super::store::person_name(person).is_none() {
            bail!("--also: `{person}` is not a person such as person/ada");
        }
        people.push((uid, person.to_owned()));
    }
    let st = match &setup.st {
        Some(path) => path.clone(),
        None => std::env::current_exe()?,
    };
    let st = std::fs::canonicalize(&st).with_context(|| format!("resolve {}", st.display()))?;
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    let roots = setup
        .checkout_roots
        .iter()
        .map(|root| format!("{:?}", root.display().to_string()))
        .collect::<Vec<_>>()
        .join(", ");
    let people_toml = people
        .iter()
        .map(|(uid, person)| format!("\"{uid}\" = {person:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!(
        r#"#!/bin/sh
# sekrets: the gateway, its user and its store. Review, then run as root, for example
#   sekrets setup --person {person} > sekrets-setup.sh && sudo sh sekrets-setup.sh
# Run it again after updating st to give the gateway the new binary.
set -eu
command -v bwrap >/dev/null || {{ echo "install bubblewrap first (apt install bubblewrap)" >&2; exit 1; }}
command -v setfacl >/dev/null || {{ echo "install acl first (apt install acl)" >&2; exit 1; }}
id -u sekrets >/dev/null 2>&1 || useradd --system --user-group --home-dir {store} --shell /usr/sbin/nologin sekrets
install -d -o sekrets -g sekrets -m 0700 {store}
install -d -o root -g root -m 0755 /etc/st-sekrets /usr/local/libexec
# The gateway runs a copy root owns, so no person or seat can change what runs as sekrets.
install -o root -g root -m 0755 {st} /usr/local/libexec/sekrets
rm -f /usr/local/libexec/st-sekrets
cat > /etc/st-sekrets/gateway.toml <<'EOF'
socket = "{socket}"
store = "{store}"
bwrap = "/usr/bin/bwrap"
path = ["/usr/local/bin", "/usr/bin", "/bin"]
checkout_roots = [{roots}]

[people]
{people_toml}
EOF
chmod 0644 /etc/st-sekrets/gateway.toml
# The gateway binds the checkout a caller passes by its path, so it must be able to pass through
# each person's home: search only, never read or list.
for uid in {uids}; do
  home=$(getent passwd "$uid" | cut -d: -f6)
  [ -n "$home" ] && setfacl -m u:sekrets:x "$home"
done
cat > /etc/systemd/system/st-sekrets.service <<'EOF'
[Unit]
Description=sekrets gateway
After=network-online.target

[Service]
User=sekrets
Group=sekrets
ExecStart=/usr/local/libexec/sekrets serve --config /etc/st-sekrets/gateway.toml
RuntimeDirectory=st-sekrets
RuntimeDirectoryMode=0755
UMask=0077
Restart=on-failure
NoNewPrivileges=yes

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable st-sekrets.service
systemctl restart st-sekrets.service
echo "sekrets gateway running; next, from a login session: sekrets enable"
"#,
        uids = people
            .iter()
            .map(|(uid, _)| uid.to_string())
            .collect::<Vec<_>>()
            .join(" "),
        store = super::gateway::DEFAULT_STORE,
        socket = super::gateway::DEFAULT_SOCKET,
        st = quote(&st),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn until_reads_dates_and_times() {
        assert_eq!(parse_until("2026-11-01").unwrap(), 1_793_491_200_000);
        assert_eq!(
            parse_until("2026-11-01T00:00:00Z").unwrap(),
            1_793_491_200_000
        );
        assert!(parse_until("next week").is_err());
    }

    #[test]
    fn the_setup_script_names_each_person_and_a_root_owned_binary() {
        let script = setup_script(&SetupArgs {
            person: "person/ada".into(),
            uid: Some(1000),
            also: vec!["1001=person/robin".into()],
            st: Some("/bin/sh".into()),
            checkout_roots: vec!["/home".into(), "/srv/work".into()],
        })
        .unwrap();
        assert!(script.contains("\"1000\" = \"person/ada\""));
        assert!(script.contains("\"1001\" = \"person/robin\""));
        assert!(script.contains("checkout_roots = [\"/home\", \"/srv/work\"]"));
        assert!(script.contains("install -o root -g root -m 0755"));
        assert!(script.contains("User=sekrets"));
        assert!(script.contains("for uid in 1000 1001; do"));
        assert!(script.contains("setfacl -m u:sekrets:x \"$home\""));
    }
}
