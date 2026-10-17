use super::*;
// The same print macros as main.rs: CLI output goes through st3's writer.
use st3::{cli_print as print, cli_println as println};

fn parse_fleet_duration(text: &str) -> Result<u64> {
    let text = text.trim();
    let (number, unit) = text
        .find(|character: char| !character.is_ascii_digit())
        .map_or((text, "s"), |split| text.split_at(split));
    let number: u64 = number
        .parse()
        .context("a duration is a number and a unit, like 15m")?;
    Ok(number
        * match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            _ => anyhow::bail!("a duration unit is s, m, or h"),
        })
}

fn read_code_without_echo() -> Result<String> {
    use std::io::{BufRead as _, IsTerminal as _};
    let stdin = std::io::stdin();
    let terminal = stdin.is_terminal();
    let mut saved = None;
    if terminal {
        eprint!("Paste the join code: ");
        // SAFETY: plain termios calls on standard input; the saved settings are restored below.
        unsafe {
            let mut settings: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut settings) == 0 {
                saved = Some(settings);
                settings.c_lflag &= !libc::ECHO;
                libc::tcsetattr(0, libc::TCSANOW, &settings);
            }
        }
    }
    let mut line = String::new();
    let result = stdin.lock().read_line(&mut line);
    if let Some(settings) = saved {
        // SAFETY: restores the settings read above.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &settings);
        }
        eprintln!();
    }
    result?;
    Ok(line.trim().to_owned())
}

/// The one `st-fleet-join-*.code` file in this machine's Fabric inbox.
fn fabric_inbox_code() -> Result<PathBuf> {
    let home = std::env::var_os("FABRIC_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/fabric"))
        })
        .context("HOME is not set")?;
    let inbox = home.join("inbox");
    let mut found = Vec::new();
    for sender in fs::read_dir(&inbox)
        .with_context(|| format!("read the Fabric inbox {}", inbox.display()))?
    {
        let sender = sender?.path();
        if !sender.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&sender)? {
            let path = entry?.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("st-fleet-join-") && name.ends_with(".code"))
            {
                found.push(path);
            }
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => anyhow::bail!("no join code in the Fabric inbox {}", inbox.display()),
        _ => anyhow::bail!(
            "{} join codes in the Fabric inbox {}; remove the old ones",
            found.len(),
            inbox.display()
        ),
    }
}

async fn lock_fleet_admission(
    client: &Client,
    config: &Config,
    services: bool,
    verb: &str,
) -> Result<st3::node_identity::StateLock> {
    if client.get::<Value>("/v1/health").await.is_ok() {
        anyhow::ensure!(
            services,
            "stop the running st3 daemon first: nothing may write while this machine {verb}"
        );
        st3::service::stop()?;
        st3::node_identity::lock_after_stop(&config.state_dir).await
    } else {
        st3::node_identity::lock(&config.state_dir)
    }
}

pub(super) async fn run_fleet(endpoint: &Endpoint, command: FleetCommand, json_output: bool) -> Result<()> {
    let manages_services = match &command {
        FleetCommand::Create(args) => !args.no_service,
        FleetCommand::Join(args) => !args.no_service,
        FleetCommand::Leave(args) => !args.no_service,
        FleetCommand::Mode(args) => !args.no_service,
        FleetCommand::Migrate(args) => !args.no_service,
        FleetCommand::Transports { command } => match command {
            FleetTransportsCommand::Add { no_service, .. }
            | FleetTransportsCommand::Remove { no_service, .. } => !*no_service,
        },
        _ => false,
    };
    st3::service::preflight(manages_services)?;
    let mut config = Config::load_unvalidated(None)?;
    // Explicit admissions can repair a pin after their prior attempt saved membership.
    // Other controls must use the daemon's resolved identity, including leave's runtime check.
    if !matches!(&command, FleetCommand::Join(_) | FleetCommand::Create(_)) {
        config.apply_fleet_file()?;
        st3::node_identity::resolve(&mut config)?;
    }
    let client = Client::new(endpoint.clone());
    match command {
        FleetCommand::Create(args) => {
            anyhow::ensure!(
                st3::config::FleetFile::load(&config.state_dir)?.is_none(),
                "this machine is already in a fleet"
            );
            anyhow::ensure!(
                config.fleet_id.is_none(),
                "config.toml already configures a fleet with config peers; move it to membership with st fleet migrate"
            );
            st3::node_identity::resolve(&mut config)?;
            let node = args.name.unwrap_or_else(|| config.node.clone());
            anyhow::ensure!(
                st3::store::valid_fleet_node_name(&node),
                "`{node}` cannot name a fleet member; use --name"
            );
            let services = !args.no_service && services_installed()?;
            let state_identity =
                lock_fleet_admission(&client, &config, services, "founds a fleet").await?;
            let founded =
                st3::fleet::join::found(&config.state_dir, &node, &args.member.settings())?;
            state_identity.record_fleet_found(&founded)?;
            drop(state_identity);
            if json_output {
                return print_value(&founded, true);
            }
            println!(
                "Created fleet {} with {} as its first member.",
                founded.fleet_id, founded.node
            );
            if services {
                st3::service::install(Config::load_with_fleet(None)?)?;
            }
            print!("{}", cli_help::fleet_created_next_steps(services));
            Ok(())
        }
        FleetCommand::Invite(args) => {
            let person = fleet_person(args.actor.clone(), &config)?;
            if args.send_fabric {
                anyhow::ensure!(
                    args.name.is_some(),
                    "--send-fabric needs the NAME of the machine to send it to"
                );
            }
            let created: st3::api::FleetInviteCreated = client
                .post(
                    "/v1/internal/fleet/invites",
                    &st3::api::FleetInviteRequest {
                        name: args.name.clone(),
                        expires_seconds: parse_fleet_duration(&args.expires)?,
                        via: Some(args.via.clone()),
                        migrate: args.migrate,
                        person,
                    },
                )
                .await?;
            let command = if args.migrate { "migrate" } else { "join" };
            if let Some(path) = &args.code_file {
                st3::fleet::join::write_private(path, created.code.as_bytes())?;
                println!("{}\t{}", created.invite, path.display());
            } else if args.send_fabric {
                let name = args.name.as_deref().unwrap_or_default();
                let fabric = st3::config::FleetFile::load(&config.state_dir)?
                    .and_then(|file| file.fabric)
                    .or_else(|| st3::fleet::transport::resolve_tool(None, "fabric"))
                    .context("--send-fabric needs the fabric command")?;
                let temporary = config
                    .state_dir
                    .join("fleet")
                    .join(format!(".send-{}.code", std::process::id()));
                st3::fleet::join::write_private(&temporary, created.code.as_bytes())?;
                let invite_id = created.invite.trim_start_matches("fleet-invite/");
                let sent = std::process::Command::new(&fabric)
                    .args(["send-file", name])
                    .arg(&temporary)
                    .args(["--as", &format!("st-fleet-join-{invite_id}.code")])
                    .status();
                let _ = fs::remove_file(&temporary);
                anyhow::ensure!(
                    sent?.success(),
                    "fabric send-file to {name} failed; the invite stays open until it expires"
                );
                println!(
                    "Sent invite {} to {name}'s Fabric inbox. On {name}, run:\n  st fleet {command} --fabric-inbox",
                    created.invite
                );
            } else if args.code_only {
                println!("{}", created.code);
            } else if json_output {
                return print_value(&created, true);
            } else {
                let expires_in =
                    (u128::from(created.expires_at_unix_ms).saturating_sub(unix_ms()) / 60_000) + 1;
                let target = args.name.as_deref().unwrap_or("the new machine");
                println!(
                    "Invite {} for {target} expires in about {expires_in} minutes.",
                    created.invite
                );
                println!(
                    "On {target}, run this and paste the code when asked:\n  st fleet {command}"
                );
                println!("Code:\n  {}", created.code);
                if let Some(name) = &args.name {
                    println!(
                        "Or, if {name} is a Fabric peer of this machine, send the code instead of showing it:\n  st fleet invite {name} --send-fabric\n  fabric exec {name} -- st fleet {command} --fabric-inbox"
                    );
                }
            }
            Ok(())
        }
        FleetCommand::Invites { all, command } => match command {
            None => {
                let invites: Vec<st3::store::FleetInviteView> = client
                    .get(&format!("/v1/internal/fleet/invites?all={all}"))
                    .await?;
                if json_output {
                    return print_value(&invites, true);
                }
                println!("INVITES  {}", invites.len());
                for invite in invites {
                    let detail = match invite.state.as_str() {
                        "redeemed" => format!(
                            "by {} (key {}…)",
                            invite.redeemed_name.as_deref().unwrap_or("?"),
                            invite
                                .redeemed_key
                                .as_deref()
                                .map(|key| &key[..key.len().min(8)])
                                .unwrap_or("?")
                        ),
                        "revoked" => invite.revoked_reason.clone().unwrap_or_default(),
                        _ => format!("for {}", invite.name.as_deref().unwrap_or("any name")),
                    };
                    println!(
                        "{}  {}  sponsor {}  {}",
                        invite.invite, invite.state, invite.sponsor, detail
                    );
                }
                Ok(())
            }
            Some(FleetInvitesCommand::Revoke {
                invite,
                reason,
                actor,
            }) => {
                let person = fleet_person(actor, &config)?;
                let _: Value = client
                    .post(
                        "/v1/internal/fleet/invites/revoke",
                        &st3::api::FleetInviteRevokeRequest {
                            invite: invite.clone(),
                            reason,
                            person,
                        },
                    )
                    .await?;
                println!("revoked\t{invite}");
                Ok(())
            }
        },
        FleetCommand::Join(args) => {
            let (code, code_path) = if args.fabric_inbox {
                let path = fabric_inbox_code()?;
                (fs::read_to_string(&path)?, Some(path))
            } else if let Some(path) = &args.code_file {
                (fs::read_to_string(path)?, Some(path.clone()))
            } else {
                match args.code.as_deref() {
                    Some("-") => {
                        let mut text = String::new();
                        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
                        (text, None)
                    }
                    Some(code) => (code.to_owned(), None),
                    None => (read_code_without_echo()?, None),
                }
            };
            let use_services = !args.no_service && services_installed()?;
            let state_identity =
                lock_fleet_admission(&client, &config, use_services, "joins").await?;
            let joined = st3::fleet::join::join(&st3::fleet::join::JoinOptions {
                state_dir: config.state_dir.clone(),
                configured_node: config.node.clone(),
                code: code.trim().to_owned(),
                name: args.name.clone(),
                via: args.via.clone(),
                settings: args.member.settings(),
                legacy_secret_file: None,
                fabric_protocol: None,
                runtime: st3::store::runtime(),
                version: env!("CARGO_PKG_VERSION").into(),
            })
            .await?;
            state_identity.record_fleet_join(&joined)?;
            drop(state_identity);
            if let Some(path) = code_path {
                let _ = fs::remove_file(path);
            }
            if json_output {
                print_value(&joined, true)?;
            } else {
                println!(
                    "{} joined fleet {} through {}{}.",
                    joined.name,
                    joined.fleet_id,
                    joined.sponsor,
                    if joined.resumed { " (resumed)" } else { "" }
                );
            }
            let mut sync_state = None;
            if use_services {
                st3::service::install(Config::load_with_fleet(None)?)?;
                if !json_output {
                    println!("The st services now run as a fleet member.");
                }
                if !args.no_wait {
                    match wait_for_first_sync(&client, Duration::from_secs(30 * 60), !json_output)
                        .await?
                    {
                        // The join already printed its JSON; a failed first sync still fails.
                        Some(first) if json_output => anyhow::ensure!(
                            first.state == "verified",
                            "{}",
                            render_first_sync(&first, now_ms())
                        ),
                        Some(first) => {
                            sync_state = Some(first.state.clone());
                            if first.state != "verified" {
                                print!(
                                    "{}",
                                    cli_help::fleet_next_steps(use_services, sync_state.as_deref())
                                );
                            }
                            report_first_sync(&first, false)?;
                        }
                        None if json_output => {}
                        None => println!(
                            "This machine is a member and still syncing; st fleet wait waits for \
                             the first sync to end and checks it."
                        ),
                    }
                }
            } else if !json_output {
                println!(
                    "Start st3 up and st3 replication-worker to begin syncing; st fleet wait \
                     waits for the first sync to end and checks it."
                );
            }
            if !json_output {
                print!(
                    "{}",
                    cli_help::fleet_next_steps(use_services, sync_state.as_deref())
                );
            }
            Ok(())
        }
        FleetCommand::Wait { timeout } => {
            let timeout = Duration::from_secs(parse_fleet_duration(&timeout)?);
            let started = std::time::Instant::now();
            let since = now_ms();
            let first = wait_for_first_sync(&client, timeout, !json_output)
                .await?
                .with_context(|| {
                    format!(
                        "the first sync has not ended after {} s; st replication status shows \
                         how far it got",
                        timeout.as_secs()
                    )
                })?;
            if first.state != "verified" {
                return report_first_sync(&first, json_output);
            }
            // A first sync is verified once, long ago after a restart. A wait is a gate for now:
            // it also needs an exchange since it began at which this node held everything.
            let caught_up = wait_for_caught_up(
                &client,
                since,
                timeout.saturating_sub(started.elapsed()),
                !json_output,
            )
            .await?;
            let caught_up = match caught_up {
                Ok(caught_up) => caught_up,
                Err(waiting) => anyhow::bail!(
                    "{}\nbut this node has not caught up since this wait began {} s ago: {}; st \
                     replication status shows how far it got",
                    render_first_sync(&first, now_ms()),
                    started.elapsed().as_secs(),
                    waiting
                ),
            };
            // Holding the fleet's envelopes is not enough when this node admitted some the fleet
            // fenced: its claims then differ from every other member's.
            let residue: Vec<smallclaims::store::FleetAdmissionResidue> =
                client.get("/v1/internal/fleet/admission-residue").await?;
            if let Some(fenced) = render_fenced_admissions(&residue) {
                anyhow::bail!("{}\nbut {fenced}", render_first_sync(&first, now_ms()));
            }
            if json_output {
                let mut value = serde_json::to_value(&first)?;
                value["caught_up"] = serde_json::to_value(&caught_up)?;
                return print_value(&value, true);
            }
            println!("{}", render_first_sync(&first, now_ms()));
            println!("{}", render_caught_up(&caught_up, now_ms()));
            Ok(())
        }
        FleetCommand::Remove(args) => run_fleet_remove(&client, &config, args).await,
        FleetCommand::Migrate(args) => run_fleet_migrate(&client, &config, args).await,
        FleetCommand::Mode(args) => {
            let lock = lock_fleet_settings(&config.state_dir).await?;
            let mut file = st3::config::FleetFile::load(&config.state_dir)?
                .context("this machine is not a fleet member")?;
            file.mode = match args.mode.as_str() {
                "listening" => st3::config::FleetMode::Listening,
                "dial-out" => st3::config::FleetMode::DialOut,
                _ => anyhow::bail!("the mode is listening or dial-out"),
            };
            file.port = match file.mode {
                st3::config::FleetMode::DialOut => None,
                st3::config::FleetMode::Listening => Some(match args.port.or(file.port) {
                    Some(port) => port,
                    None => st3::fleet::join::free_port(st3::fleet::join::DEFAULT_PORT)?,
                }),
            };
            file.save(&config.state_dir)?;
            drop(lock);
            println!(
                "This member is now {}; it announces the change when its replication worker starts.",
                file.mode.as_str()
            );
            refresh_fleet_worker(args.no_service)?;
            Ok(())
        }
        FleetCommand::Transports { command } => {
            run_fleet_transports(&client, &config, command).await
        }
        FleetCommand::Leave(args) => {
            let person = fleet_person(args.actor.clone(), &config)?;
            if args.cancel {
                let _: Value = client
                    .post("/v1/internal/fleet/leave/cancel", &json!({}))
                    .await?;
                println!("leave cancelled");
                return Ok(());
            }
            let use_services = !args.no_service && services_installed()?;
            let confirmed = fleet_leave(
                &client,
                &config,
                &person,
                args.offline,
                args.force,
                parse_fleet_duration(&args.wait)?,
            )
            .await?;
            match confirmed {
                Some(member) => println!("{member} holds everything this machine wrote."),
                None => println!(
                    "Left without reaching a member. On another member run: st fleet remove {} --reason \"left offline\"",
                    config.node
                ),
            }
            if use_services {
                st3::service::install(Config::load_with_fleet(None)?)?;
                println!("st3 now runs local-only on this machine.");
            } else {
                println!(
                    "Stop st3 replication-worker; st3 up now runs local-only after a restart."
                );
            }
            Ok(())
        }
        FleetCommand::Status => {
            let status: st3::api::FleetStatus = client.get("/v1/internal/fleet/status").await?;
            if json_output {
                return print_value(&status, true);
            }
            println!(
                "FLEET  {}   this node: {}",
                status.fleet_id.as_deref().unwrap_or("none"),
                status.node
            );
            println!("TRANSPORTS  {}", status.transports.join(","));
            if let Some(protocol) = &status.fabric_protocol {
                println!("FABRIC-PROTOCOL  {protocol}");
            }
            if let Some(removed) = &status.removed {
                println!("REMOVED  {}", removed.describe(status.fleet_id.as_deref()));
            }
            println!("MEMBER  MODE  STATE  ROUTE-ENDPOINTS");
            for member in &status.view.members {
                let transports = member
                    .endpoints
                    .iter()
                    .filter_map(|endpoint| endpoint["transport"].as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "{}  {}  {}{}  {}",
                    member.name,
                    member.mode,
                    member.state,
                    member
                        .ended
                        .as_deref()
                        .map(|ended| format!(" ({ended})"))
                        .unwrap_or_default(),
                    if transports.is_empty() {
                        "-".into()
                    } else {
                        transports
                    }
                );
            }
            for peer in &status.peers {
                println!(
                    "PEER  {}  {}{}",
                    peer.peer,
                    peer.status,
                    peer.refusal_reason
                        .as_deref()
                        .or(peer.last_error.as_deref())
                        .map(|error| format!("  {error}"))
                        .unwrap_or_default()
                );
            }
            for invite in &status.invites {
                println!("INVITE  {}  {}", invite.invite, invite.state);
            }
            Ok(())
        }
    }
}

async fn run_fleet_remove(client: &Client, config: &Config, args: FleetRemoveArgs) -> Result<()> {
    let person = fleet_person(args.actor, config)?;
    let removal: st3::store::FleetRemoval = client
        .post(
            "/v1/internal/fleet/remove",
            &st3::api::FleetRemoveRequest {
                name: args.name.clone(),
                reason: args.reason,
                person,
            },
        )
        .await?;
    println!(
        "Removed {} (high water {}). Each member refuses it once this removal reaches it.",
        removal.name, removal.high_water
    );
    for invite in &removal.revoked_invites {
        println!("revoked\t{invite}");
    }
    println!(
        "Writes {} made after this node's last exchange with it are not accepted. On {}, if it still runs: st uninstall",
        removal.name, removal.name
    );
    if st3::config::FleetFile::load(&config.state_dir)?.is_some_and(|file| file.legacy_peers) {
        println!(
            "This member still accepts legacy exchanges from config peers, so a machine that keeps the fleet secret can pose as one of them until st fleet migrate --finish."
        );
    }
    Ok(())
}

async fn lock_fleet_settings(state_dir: &std::path::Path) -> Result<std::fs::File> {
    let state_dir = state_dir.to_path_buf();
    tokio::task::spawn_blocking(move || st3::config::FleetFile::lock_settings(&state_dir)).await?
}

async fn member_endpoints(client: &Client, node: &str) -> Result<Vec<Value>> {
    let view: st3::fleet::FleetView = client.get("/v1/internal/fleet/membership").await?;
    let member = view
        .members
        .iter()
        .find(|member| member.name == node)
        .context("this node has no fleet membership yet; wait for its first sync")?;
    anyhow::ensure!(
        member.state == "current",
        "this node is not a current fleet member"
    );
    Ok(member.endpoints.clone())
}

/// Transport changes use mode's atomic settings write and service refresh. They also
/// publish immediately, so removal is announced before unexposure. The caller holds the
/// settings lock until publication and unexposure end.
async fn save_and_publish_fleet_settings(
    client: &Client,
    config: &Config,
    file: &st3::config::FleetFile,
    mut endpoints: Vec<Value>,
) -> Result<()> {
    if file.mode == st3::config::FleetMode::DialOut {
        endpoints.clear();
    } else {
        endpoints.retain(|endpoint| match endpoint["transport"].as_str() {
            Some("fabric") => file.transports.iter().any(|name| name == "fabric"),
            Some(name @ ("loopback" | "tailscale")) => {
                (file.transports.iter().any(|transport| transport == name)
                    || (name == "loopback" && file.advertise_loopback))
                    && endpoint["address"]
                        .as_str()
                        .and_then(|address| address.parse::<std::net::SocketAddr>().ok())
                        .is_some_and(|address| Some(address.port()) == file.port)
            }
            _ => false,
        });
    }
    file.save(&config.state_dir)?;
    let _: Value = client
        .post(
            "/v1/internal/fleet/endpoints",
            &st3::api::FleetEndpointsRequest {
                mode: file.mode.as_str().into(),
                endpoints,
            },
        )
        .await
        .context("fleet.toml saved; endpoint publication did not complete; restart the replication worker to announce the saved settings")?;
    Ok(())
}

fn refresh_fleet_worker(no_service: bool) -> Result<()> {
    if !no_service && services_installed()? {
        st3::service::install(Config::load_with_fleet(None)?)?;
    } else {
        println!("Restart st3 replication-worker for the change to take effect.");
        println!(
            "The old worker no longer publishes endpoints after its settings change; restart it to resume publication."
        );
    }
    Ok(())
}

async fn run_fleet_transports(
    client: &Client,
    config: &Config,
    command: FleetTransportsCommand,
) -> Result<()> {
    use st3::fleet::transport::{Fabric, default_fabric_protocol, resolve_tool};
    let lock = lock_fleet_settings(&config.state_dir).await?;
    let mut file = st3::config::FleetFile::load(&config.state_dir)?
        .context("this machine is not a fleet member")?;
    anyhow::ensure!(
        file.removed.is_none(),
        "this member was removed from its fleet"
    );
    let transport = match &command {
        FleetTransportsCommand::Add { transport, .. }
        | FleetTransportsCommand::Remove { transport, .. } => transport,
    };
    let enabled = file
        .transports
        .iter()
        .any(|configured| configured == transport);
    // A repeated add never reconfigures an existing exposure, even with a different flag.
    if matches!(&command, FleetTransportsCommand::Add { .. }) && enabled {
        println!("Fabric is already enabled; remove it before changing its protocol.");
        return Ok(());
    }
    if matches!(&command, FleetTransportsCommand::Remove { .. }) {
        anyhow::ensure!(enabled, "Fabric is not configured on this member");
        anyhow::ensure!(
            file.transports
                .iter()
                .any(|configured| configured != transport),
            "cannot remove this member's last transport"
        );
    }
    let mut endpoints = member_endpoints(client, &config.node).await?;
    let protocol = file
        .fabric_protocol
        .clone()
        .unwrap_or_else(|| default_fabric_protocol(&file.fleet_id));
    let no_service = match command {
        FleetTransportsCommand::Add {
            fabric_protocol,
            no_service,
            ..
        } => {
            anyhow::ensure!(
                file.mode == st3::config::FleetMode::Listening,
                "adding Fabric requires a listening member; run st fleet mode listening first"
            );
            let port = file
                .port
                .context("adding Fabric requires this member's configured replication port")?;
            anyhow::ensure!(
                port != 0,
                "adding Fabric requires a fixed nonzero replication port; run st fleet mode listening --port PORT first"
            );
            let protocol = fabric_protocol.unwrap_or(protocol);
            anyhow::ensure!(
                !protocol.trim().is_empty(),
                "Fabric protocol must not be empty"
            );
            let fabric = Fabric::new(
                resolve_tool(file.fabric.as_deref(), "fabric")
                    .context("this machine cannot use Fabric: fabric is not installed")?,
            );
            let node = fabric
                .id()
                .await
                .context("this machine cannot use Fabric")?;
            fabric
                .expose(&protocol, &format!("127.0.0.1:{port}"))
                .await
                .context("Fabric exposure failed; fleet settings and endpoints are unchanged")?;
            file.transports.push("fabric".into());
            file.fabric_protocol = Some(protocol.clone());
            endpoints.retain(|endpoint| endpoint["transport"] != "fabric");
            endpoints.push(json!({"transport":"fabric", "node":node, "protocol":protocol}));
            // A lost publication response can follow a committed claim. Keep the exposure
            // reachable and report the incomplete operation rather than undoing that route.
            save_and_publish_fleet_settings(client, config, &file, endpoints)
                .await
                .with_context(|| format!("Fabric remains exposed as {protocol}"))?;
            println!("Fabric enabled with protocol {protocol}.");
            no_service
        }
        FleetTransportsCommand::Remove { no_service, .. } => {
            // A configured but unavailable carrier cannot provide the promised rollback.
            let local = st3::fleet::transport::local_addresses();
            anyhow::ensure!(
                (file.mode == st3::config::FleetMode::DialOut
                    && file
                        .transports
                        .iter()
                        .any(|transport| match transport.as_str() {
                            "loopback" => local.iter().any(std::net::IpAddr::is_loopback),
                            "tailscale" =>
                                local.iter().any(st3::fleet::transport::is_tailnet_address)
                                    && resolve_tool(file.tailscale.as_deref(), "tailscale")
                                        .is_some(),
                            _ => false,
                        }))
                    || endpoints.iter().any(|endpoint| {
                        let Some(transport) = endpoint["transport"].as_str() else {
                            return false;
                        };
                        transport != "fabric"
                            && file.transports.iter().any(|name| name == transport)
                            && endpoint["address"]
                                .as_str()
                                .and_then(|address| address.parse::<std::net::SocketAddr>().ok())
                                .is_some_and(|address| {
                                    local.contains(&address.ip())
                                        && Some(address.port()) == file.port
                                })
                    }),
                "cannot remove Fabric: no remaining configured transport is usable on this machine"
            );
            file.transports.retain(|transport| transport != "fabric");
            // Retain the existing protocol setting so a later add can restore it.
            save_and_publish_fleet_settings(client, config, &file, endpoints).await?;
            let result = match resolve_tool(file.fabric.as_deref(), "fabric") {
                Some(program) => Fabric::new(program).unexpose(&protocol).await,
                None => Err(anyhow::anyhow!("fabric is not installed")),
            };
            drop(lock);
            // Refresh even when cleanup fails: no worker may keep using the removed carrier.
            refresh_fleet_worker(no_service)?;
            if let Err(error) = result {
                println!(
                    "Fabric endpoint removed; unexposure still needs cleanup: {error:#}. When Fabric is available, run fabric unexpose {protocol}"
                );
            }
            println!(
                "Fabric removed; remaining transports: {}.",
                file.transports.join(",")
            );
            return Ok(());
        }
    };
    drop(lock);
    refresh_fleet_worker(no_service)
}

/// Settings for a migrating node: its existing replication port unless one is given.
fn migration_settings(args: &FleetMemberArgs, config: &Config) -> st3::fleet::join::MemberSettings {
    let mut settings = args.settings();
    if settings.port.is_none() {
        settings.port = config
            .peer_listen
            .as_deref()
            .and_then(|address| address.parse::<std::net::SocketAddr>().ok())
            .map(|address| address.port());
    }
    settings
}

async fn run_fleet_migrate(client: &Client, config: &Config, args: FleetMigrateArgs) -> Result<()> {
    if args.finish || args.unfinish {
        let mut file = st3::config::FleetFile::load(&config.state_dir)?
            .context("this machine has not migrated yet")?;
        if args.finish {
            let status: st3::api::FleetStatus = client.get("/v1/internal/fleet/status").await?;
            let waiting = config
                .peers
                .iter()
                .filter(|peer| {
                    let known = status
                        .view
                        .members
                        .iter()
                        .any(|member| member.name == peer.name)
                        || status.view.legacy_removed.contains(&peer.name);
                    !known
                })
                .map(|peer| peer.name.clone())
                .collect::<Vec<_>>();
            anyhow::ensure!(
                waiting.is_empty(),
                "these config peers are neither members nor removed yet: {}",
                waiting.join(", ")
            );
        }
        file.legacy_peers = args.unfinish;
        file.save(&config.state_dir)?;
        if args.finish {
            println!(
                "This machine no longer accepts legacy exchanges. Delete these lines from {}:",
                Config::default_path().display()
            );
            println!("  fleet_id, shared_secret_file, peer_listen, and every [[peers]] entry");
        } else {
            println!("This machine accepts legacy exchanges from config peers again.");
        }
        if !args.no_service && services_installed()? {
            st3::service::install(Config::load_with_fleet(None)?)?;
        } else {
            println!("Restart st3 replication-worker for this to take effect.");
        }
        return Ok(());
    }
    anyhow::ensure!(
        st3::config::FleetFile::load(&config.state_dir)?.is_none(),
        "this machine already has fleet membership settings"
    );
    anyhow::ensure!(
        st3::store::valid_fleet_node_name(&config.node),
        "`{}` cannot name a fleet member",
        config.node
    );
    let fleet_id = config
        .fleet_id
        .clone()
        .context("this machine has no config-peer fleet to migrate; use st fleet join")?;
    // Read only while the daemon may still be running; the locked migration rechecks
    // its store before writing. Invalid admission must not stop healthy services.
    {
        use rusqlite::OptionalExtension as _;
        let database = config.state_dir.join("claims.sqlite3");
        anyhow::ensure!(
            database.exists(),
            "this store is not bound to a fleet yet; start st3 once first"
        );
        let connection = rusqlite::Connection::open_with_flags(
            database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let bound: Option<String> = connection
            .query_row("SELECT value FROM meta WHERE key='fleet_id'", [], |row| {
                row.get(0)
            })
            .optional()?;
        match bound {
            Some(bound) => anyhow::ensure!(
                bound == fleet_id,
                "this store belongs to fleet {bound}, not the configured {fleet_id}"
            ),
            None => anyhow::bail!("this store is not bound to a fleet yet; start st3 once first"),
        }
    }
    // fleet.toml resolves a relative path under STATE/fleet, and --finish removes the
    // config.toml override, so record the secret file's absolute path now.
    let configured_secret = config
        .shared_secret_file
        .clone()
        .context("config.toml names no shared_secret_file")?;
    let secret_file = fs::canonicalize(&configured_secret).with_context(|| {
        format!(
            "the shared secret file {} is not readable from here; name it with an absolute path in config.toml",
            configured_secret.display()
        )
    })?;
    let use_services = !args.no_service && services_installed()?;
    let state_identity = lock_fleet_admission(client, config, use_services, "migrates").await?;
    let settings = migration_settings(&args.member, config);
    if args.anchor {
        let founded = st3::fleet::join::migrate_anchor(
            &config.state_dir,
            &config.node,
            &fleet_id,
            &secret_file,
            &settings,
            args.fabric_protocol.clone(),
            st3::store::runtime(),
        )?;
        state_identity.record_fleet_found(&founded)?;
        println!(
            "{} is the anchor of fleet {}. It admits itself and signs its history when st3 starts.",
            founded.node, founded.fleet_id
        );
    } else {
        let (code, code_path) = if args.fabric_inbox {
            let path = fabric_inbox_code()?;
            (fs::read_to_string(&path)?, Some(path))
        } else if let Some(path) = &args.code_file {
            (fs::read_to_string(path)?, Some(path.clone()))
        } else {
            match args.code.as_deref() {
                Some("-") => {
                    let mut text = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
                    (text, None)
                }
                Some(code) => (code.to_owned(), None),
                None => (read_code_without_echo()?, None),
            }
        };
        let joined = st3::fleet::join::join(&st3::fleet::join::JoinOptions {
            state_dir: config.state_dir.clone(),
            configured_node: config.node.clone(),
            code: code.trim().to_owned(),
            name: Some(config.node.clone()),
            via: args.via.clone(),
            settings,
            legacy_secret_file: Some(secret_file),
            fabric_protocol: args.fabric_protocol.clone(),
            runtime: st3::store::runtime(),
            version: env!("CARGO_PKG_VERSION").into(),
        })
        .await?;
        anyhow::ensure!(
            joined.migrate,
            "that code is a join code; use st fleet join"
        );
        state_identity.record_fleet_join(&joined)?;
        if let Some(path) = code_path {
            let _ = fs::remove_file(path);
        }
        println!(
            "{} migrated to membership in fleet {} through {}.",
            joined.name, joined.fleet_id, joined.sponsor
        );
    }
    drop(state_identity);
    if use_services {
        st3::service::install(Config::load_with_fleet(None)?)?;
        println!(
            "The st3 services now run as a fleet member, with legacy exchanges still accepted."
        );
    } else {
        println!(
            "Start st3 up and st3 replication-worker; legacy exchanges stay accepted until st fleet migrate --finish."
        );
    }
    Ok(())
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// Wait for this node's first sync to end, printing progress when `progress` is set. It ends at
/// the first exchange at which this node holds the same envelopes as a peer and has admitted
/// them all. Returns None on timeout.
async fn wait_for_first_sync(
    client: &Client,
    timeout: Duration,
    progress: bool,
) -> Result<Option<st3::model::ReplicationFirstSync>> {
    let started = std::time::Instant::now();
    let mut reported = None::<std::time::Instant>;
    loop {
        if let Ok(status) = client
            .get::<ReplicationStatus>("/v1/replication/status")
            .await
        {
            // A removed node never finishes syncing; say why instead of waiting out the timeout.
            if let Some(removed) = &status.removed {
                anyhow::bail!("{}", removed.describe(status.fleet_id.as_deref()));
            }
            let first = status.first_sync.clone().context(
                "this node has no first sync to wait for: it did not join with st fleet join",
            )?;
            match first.state.as_str() {
                "verified" | "failed" => return Ok(Some(first)),
                _ => {
                    if progress && reported.is_none_or(|at| at.elapsed() >= Duration::from_secs(10))
                    {
                        reported = Some(std::time::Instant::now());
                        let behind = status
                            .peers
                            .iter()
                            .filter_map(|peer| {
                                let sync = peer.sync.as_ref()?;
                                Some(format!(
                                    "{} has {} this node lacks",
                                    peer.peer,
                                    envelope_count(sync.peer_only_envelopes)
                                ))
                            })
                            .collect::<Vec<_>>();
                        println!(
                            "first sync: {} envelopes so far{}",
                            status.received_envelopes,
                            if behind.is_empty() {
                                String::new()
                            } else {
                                format!("; {}", behind.join("; "))
                            }
                        );
                    }
                }
            }
        }
        if started.elapsed() >= timeout {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Wait until [`caught_up_since`] holds, printing progress when `progress` is set. Returns what
/// is still missing at the deadline.
async fn wait_for_caught_up(
    client: &Client,
    since: u128,
    timeout: Duration,
    progress: bool,
) -> Result<Result<CaughtUp, String>> {
    let started = std::time::Instant::now();
    let mut reported = None::<std::time::Instant>;
    let mut missing = "replication status did not answer".to_owned();
    loop {
        if let Ok(status) = client
            .get::<ReplicationStatus>("/v1/replication/status")
            .await
        {
            if let Some(removed) = &status.removed {
                anyhow::bail!("{}", removed.describe(status.fleet_id.as_deref()));
            }
            match caught_up_since(&status, since) {
                Ok(caught_up) => return Ok(Ok(caught_up)),
                Err(waiting) => missing = waiting,
            }
            if progress && reported.is_none_or(|at| at.elapsed() >= Duration::from_secs(10)) {
                reported = Some(std::time::Instant::now());
                println!("catching up: {missing}");
            }
        }
        if started.elapsed() >= timeout {
            return Ok(Err(missing));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Print how a first sync ended, and fail when an earlier build recorded it as failed.
fn report_first_sync(first: &st3::model::ReplicationFirstSync, json_output: bool) -> Result<()> {
    if json_output {
        print_value(first, true)?;
        anyhow::ensure!(first.state == "verified", "the first sync failed");
        return Ok(());
    }
    anyhow::ensure!(
        first.state == "verified",
        "{}\nst replication status shows this node's sync with each peer; report it with st \
         diagnostic.",
        render_first_sync(first, now_ms())
    );
    println!("{}", render_first_sync(first, now_ms()));
    Ok(())
}
