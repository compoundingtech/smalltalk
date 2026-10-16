use super::*;
// The same print macros as main.rs: CLI output goes through st3's writer.
use st3::{cli_print as print, cli_println as println};

pub(super) async fn run_pty(
    client: &Client,
    endpoint: &Endpoint,
    configured_person: Option<&str>,
    pty_root: Option<&Path>,
    command: PtyCommand,
    json_output: bool,
) -> Result<()> {
    match command {
        PtyCommand::New(args) => {
            let person =
                configured_actor(args.person.as_deref(), configured_person, "terminals new")?;
            let generated = generated_client(endpoint, Some(&person))?;
            let capabilities = generated.capabilities().await?;
            let local = client.get::<Value>("/v1/health").await?["node"]
                .as_str()
                .context("daemon has no node")?
                .to_owned();
            let cwd = if let Some(cwd) = args.cwd {
                if args
                    .host
                    .as_deref()
                    .is_none_or(|host| host.trim_start_matches("host/") == local)
                {
                    Some(std::path::absolute(cwd)?.display().to_string())
                } else {
                    anyhow::ensure!(
                        cwd.is_absolute(),
                        "remote cwd must be an absolute path on its host"
                    );
                    Some(cwd.display().to_string())
                }
            } else if args
                .host
                .as_deref()
                .is_none_or(|host| host.trim_start_matches("host/") == local)
            {
                Some(std::env::current_dir()?.display().to_string())
            } else {
                None
            };
            let nonce = uuid::Uuid::now_v7().simple().to_string();
            let result = generated
                .terminal_create(
                    format!("action/{nonce}"),
                    format!("terminal-new:{nonce}"),
                    ClientFence {
                        snapshot_id: capabilities.snapshot.id,
                        ..ClientFence::default()
                    },
                    st3_client::TerminalCreateParameters {
                        name: args
                            .name
                            .unwrap_or_else(|| format!("shell-{}", &nonce[24..])),
                        host: args.host,
                        cwd,
                    },
                )
                .await?;
            if json_output {
                print_client_value(&result, true)
            } else {
                println!(
                    "{}",
                    result
                        .value
                        .affected_ids
                        .first()
                        .context("creation returned no terminal")?
                );
                Ok(())
            }
        }
        PtyCommand::End(args) => {
            let person =
                configured_actor(args.person.as_deref(), configured_person, "terminals end")?;
            let generated = generated_client(endpoint, Some(&person))?;
            let capabilities = generated.capabilities().await?;
            let nonce = uuid::Uuid::now_v7().simple().to_string();
            let result = generated
                .terminal_end(
                    format!("action/{nonce}"),
                    format!("terminal-end:{nonce}"),
                    ClientFence {
                        snapshot_id: capabilities.snapshot.id,
                        ..ClientFence::default()
                    },
                    ClientTargetParameters {
                        target_id: args.subject,
                        ..ClientTargetParameters::default()
                    },
                )
                .await?;
            print_client_value(&result, json_output)
        }
        PtyCommand::Ls {
            all,
            cursor,
            limit,
            owner,
            state,
        } => {
            anyhow::ensure!(
                limit > 0 && limit <= 200,
                "the terminal limit must be 1 through 200"
            );
            let response = generated_client(endpoint, None)?
                .terminals_list_filtered(
                    cursor.as_deref(),
                    Some(limit),
                    all,
                    owner.as_deref(),
                    state.as_deref(),
                )
                .await?;
            let history = if all { " --all" } else { "" };
            let mut command = format!("st terminals ls{history}");
            for (name, value) in [("owner", owner.as_deref()), ("state", state.as_deref())] {
                if let Some(value) = value {
                    let quoted = value.replace('\'', "'\"'\"'");
                    command.push_str(&format!(" --{name} '{quoted}'"));
                }
            }
            print_product_page("TERMINALS", &response, json_output, &command)
        }
        PtyCommand::Attach(args) => {
            let budget = (!args.subject.contains('/'))
                .then(|| tokio::time::Instant::now() + LOCAL_ATTACH_CONSULT);
            let resolution = resolve_member_subject(
                endpoint,
                &args.subject,
                "pty",
                Entity::Terminal,
                completion::Matching::Fuzzy,
            );
            let subject = if let Some(deadline) = budget {
                tokio::time::timeout_at(deadline, resolution)
                    .await
                    .context("st terminals attach: short-name resolution did not answer within 1 s; name the full subject to attach without the daemon")??
            } else {
                resolution.await?
            };
            let person = args.person.as_deref().or(configured_person);
            attach_terminal(
                client, endpoint, pty_root, person, &subject, args.force, budget,
            )
            .await
        }
        PtyCommand::ExposeFabric(_) | PtyCommand::ServeFabric(_) => {
            unreachable!("handled before any daemon client")
        }
        PtyCommand::Peek(args) => {
            let subject = resolve_member_subject(
                endpoint,
                &args.subject,
                "pty",
                Entity::Terminal,
                completion::Matching::Fuzzy,
            )
            .await?;
            let screen: SessionScreen = client
                .get(&format!(
                    "/v1/sessions/screen/{}",
                    urlencoding::encode(&subject)
                ))
                .await?;
            if json_output {
                print_value(&screen, true)
            } else {
                print!("{}", screen.screen);
                Ok(())
            }
        }
        PtyCommand::Screen(args) => {
            let person = configured_actor(
                args.person.as_deref(),
                configured_person,
                "terminals screen",
            )?;
            let response = generated_client(endpoint, Some(&person))?
                .terminal_screen(&args.subject)
                .await?;
            if json_output {
                print_value(&response, true)
            } else {
                print!("{}", render_terminal_screen(&response.value));
                Ok(())
            }
        }
        PtyCommand::AttachInfo(args) => {
            let person = configured_actor(
                args.person.as_deref(),
                configured_person,
                "terminals attach-info",
            )?;
            let generated = generated_client(endpoint, Some(&person))?;
            let screen = generated.terminal_screen(&args.subject).await?;
            let capabilities = generated.capabilities().await?;
            let nonce = uuid::Uuid::now_v7().simple().to_string();
            let response = generated
                .terminal_attach(
                    format!("action/{nonce}"),
                    format!("terminal-attach:{nonce}"),
                    ClientFence {
                        snapshot_id: capabilities.snapshot.id,
                        runtime_incarnation: Some(screen.value.runtime_incarnation),
                        terminal_sequence: Some(capabilities.snapshot.store_index),
                        ..ClientFence::default()
                    },
                    ClientTargetParameters {
                        target_id: screen.value.terminal_id,
                        ..ClientTargetParameters::default()
                    },
                )
                .await?;
            print_client_value(&response, json_output)
        }
        PtyCommand::Stream(args) => {
            let person = configured_actor(
                args.person.as_deref(),
                configured_person,
                "terminals stream",
            )?;
            let mut stream = generated_client(endpoint, Some(&person))?
                .terminal_stream(&args.subject, args.incarnation.as_deref(), &args.capability)
                .await?;
            let mut shown = 0_u64;
            while args.count.is_none_or(|count| shown < count) {
                let Some(screen) = stream.next().await? else {
                    break;
                };
                shown += 1;
                if json_output {
                    println!("{}", serde_json::to_string(&screen)?);
                } else {
                    if shown > 1 {
                        println!();
                    }
                    print!("{}", render_terminal_screen(&screen.value));
                }
            }
            stream.close().await;
            Ok(())
        }
        PtyCommand::InputClient(args) => {
            let person = configured_actor(
                args.person.as_deref(),
                configured_person,
                "terminals input-client",
            )?;
            let generated = generated_client(endpoint, Some(&person))?;
            let screen = generated.terminal_screen(&args.subject).await?;
            let capabilities = generated.capabilities().await?;
            let mode = if args.raw {
                ClientTerminalInputMode::Raw
            } else if args.key {
                ClientTerminalInputMode::Key
            } else {
                ClientTerminalInputMode::Line
            };
            let value = if args.raw {
                base64::engine::general_purpose::STANDARD.encode(args.value.as_bytes())
            } else {
                args.value
            };
            let nonce = uuid::Uuid::now_v7().simple().to_string();
            let response = generated
                .terminal_input(
                    format!("action/{nonce}"),
                    format!("terminal-input:{nonce}"),
                    ClientFence {
                        snapshot_id: capabilities.snapshot.id,
                        runtime_incarnation: Some(screen.value.runtime_incarnation),
                        terminal_sequence: Some(screen.value.next_sequence),
                        ..ClientFence::default()
                    },
                    ClientTerminalInputParameters {
                        terminal_id: screen.value.terminal_id,
                        mode,
                        value,
                    },
                )
                .await?;
            print_client_value(&response, json_output)
        }
        PtyCommand::DetachClient(args) => {
            let person = configured_actor(
                args.person.as_deref(),
                configured_person,
                "terminals detach-client",
            )?;
            let generated = generated_client(endpoint, Some(&person))?;
            let capabilities = generated.capabilities().await?;
            let nonce = uuid::Uuid::now_v7().simple().to_string();
            let response = generated
                .terminal_detach(
                    format!("action/{nonce}"),
                    format!("terminal-detach:{nonce}"),
                    ClientFence {
                        snapshot_id: capabilities.snapshot.id,
                        runtime_incarnation: Some(args.incarnation),
                        ..ClientFence::default()
                    },
                    ClientTargetParameters {
                        target_id: args.attachment,
                        ..ClientTargetParameters::default()
                    },
                )
                .await?;
            print_client_value(&response, json_output)
        }
        PtyCommand::Send(args) => {
            let subject = resolve_member_subject(
                endpoint,
                &args.subject,
                "pty",
                Entity::Terminal,
                completion::Matching::Exact,
            )
            .await?;
            let incarnation = session_incarnation(client, &subject).await?;
            let mode = if args.raw {
                SessionInputMode::Raw
            } else if args.key {
                SessionInputMode::Key
            } else {
                SessionInputMode::Line
            };
            let value = if args.raw {
                base64::engine::general_purpose::STANDARD.encode(args.value.as_bytes())
            } else {
                args.value
            };
            let response: SessionControlResponse = client
                .post(
                    &format!("/v1/sessions/input/{}", urlencoding::encode(&subject)),
                    &SessionInputRequest {
                        expected_incarnation: incarnation,
                        mode,
                        value,
                        idempotency_key: format!("pty-input:{}:{}", subject, now_ms()),
                    },
                )
                .await?;
            print_value(&response, json_output)
        }
        PtyCommand::Signal(args) => {
            let subject = resolve_member_subject(
                endpoint,
                &args.subject,
                "pty",
                Entity::Terminal,
                completion::Matching::Exact,
            )
            .await?;
            let response: SessionControlResponse = client
                .post(
                    &format!("/v1/sessions/{}/signal", urlencoding::encode(&subject)),
                    &SessionSignalRequest {
                        expected_incarnation: session_incarnation(client, &subject).await?,
                        signal: args.signal,
                        idempotency_key: format!("pty-signal:{}:{}", subject, now_ms()),
                    },
                )
                .await?;
            print_value(&response, json_output)
        }
    }
}

async fn session_incarnation(client: &Client, subject: &str) -> Result<String> {
    let status = status_for(client, subject).await?;
    status
        .subjects
        .first()
        .and_then(|item| item.actual.as_ref())
        .map(|actual| actual.get("fields").unwrap_or(actual))
        .and_then(|fields| fields.get("incarnation_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("subject `{subject}` has no live incarnation"))
}
