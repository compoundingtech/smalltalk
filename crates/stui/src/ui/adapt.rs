//! Live st3 data, turned into the view model the screens draw.
//!
//! A collection with no snapshot has not loaded yet, so it becomes `Load::Loading`, never an
//! empty list. Anything the graph does not say stays unsaid.

use super::view::*;
use crate::model::{Model, clean_message_text};
use serde_json::Value;
#[cfg(test)]
use st3_client::TimelineEntry;
use st3_client::{MissionStep, WorkLabel};
use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::BTreeSet;

/// What the live loop has fetched beside the model: conversations and launch previews.
#[derive(Default)]
pub struct Extras {
    pub conversations: BTreeMap<String, Load<Vec<Entry>>>,
    pub previews: BTreeMap<String, Load<MissionPreview>>,
    /// Loaded messages behind unread-message items: sender, title, text.
    pub bodies: BTreeMap<String, (String, Option<String>, String)>,
    pub live: bool,
    pub offline: Option<String>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn age(then: &str) -> String {
    let label = crate::age_label(then, &now());
    label.trim_end_matches(" ago").to_owned()
}

fn loaded<T>(snapshot: bool, items: Vec<T>) -> Load<Vec<T>> {
    if snapshot {
        Load::Ready(items)
    } else {
        Load::Loading
    }
}

/// The host this stui talks to, from whichever window has loaded.
fn gateway(model: &Model) -> Option<String> {
    [&model.missions, &model.agents, &model.now, &model.sessions]
        .into_iter()
        .find_map(|collection| collection.snapshot.as_ref())
        .map(|snapshot| snapshot.host_id.clone())
}

/// A step st sent with some mission, and that mission.
fn find_step<'a>(model: &'a Model, id: &str) -> Option<(&'a st3_client::Mission, &'a MissionStep)> {
    model.missions().find_map(|mission| {
        mission
            .run_details
            .iter()
            .flat_map(|run| run.steps.iter().flatten())
            .find(|step| step.id == id)
            .map(|step| (mission, step))
    })
}

/// "Mission › step" for a step an agent holds or has queued, as st named it.
fn label_text(model: &Model, label: &WorkLabel) -> String {
    let mission = model
        .missions()
        .find(|mission| mission.header.id == label.mission_id)
        .map(crate::mission_display_label)
        .unwrap_or_else(|| short(&label.mission_id));
    format!("{mission} › {}", label.path)
}

fn short(id: &str) -> String {
    id.trim_start_matches("mission/")
        .trim_start_matches("agent/")
        .to_owned()
}

fn missions(model: &Model) -> Vec<Mission> {
    st3_ui_model::missions::adapt(
        model.missions(),
        model.agents(),
        model.attention(),
        &now(),
        &st3_ui_model::missions::Display {
            mission_label: &crate::mission_display_label,
            agent_label: &crate::agent_label,
            age_label: &crate::age_label,
            clean_text: &clean_message_text,
        },
    )
}

pub fn world(model: &Model, person: &str, extras: &Extras) -> World {
    let link = if extras.offline.is_some() {
        Link::Offline(match &model.last_connected {
            Some(at) => format!("Last connected at {at} · reconnecting · r retry now"),
            None => "No member reachable · reconnecting · r retry now".into(),
        })
    } else if extras.live {
        Link::Live
    } else {
        Link::Connecting
    };
    let host = gateway(model)
        .map(|host| host.trim_start_matches("host/").to_owned())
        .unwrap_or_else(|| "this machine".into());
    let attention = attention(model, extras);
    let missions = missions(model);
    let quiet = missions
        .iter()
        .filter(|mission| {
            !matches!(
                mission.word,
                Word::Decision | Word::Done | Word::Failed | Word::Cancelled | Word::NotStarted
            ) && !mission.system
        })
        .count();
    let diverged = model
        .sync_notice()
        .into_iter()
        .flat_map(|sync| &sync.peers)
        .filter(|peer| peer.diverged_since.is_some())
        .map(|peer| peer.host_id.trim_start_matches("host/").to_owned())
        .collect();
    World {
        person: person.to_owned(),
        host,
        link,
        diverged,
        attention: loaded(model.now.snapshot.is_some(), attention),
        agents: loaded(model.agents.snapshot.is_some(), agents(model)),
        missions: loaded(model.missions.snapshot.is_some(), missions),
        machines: loaded(model.machines.snapshot.is_some(), machines(model)),
        worktrees: Load::Ready(super::demo::world().worktrees.items().to_vec()),
        devices: loaded(
            model.devices.snapshot.is_some(),
            model
                .devices()
                .map(|device| Device {
                    id: device.header.id.clone(),
                    name: device.name.clone().unwrap_or_else(|| {
                        device.header.id.trim_start_matches("device/").to_owned()
                    }),
                    state: device.state.clone(),
                    scopes: device.scopes.clone(),
                    expires: device.expires_at.clone(),
                })
                .collect(),
        ),
        conversations: extras.conversations.clone(),
        quiet_missions: quiet,
        usage: match &model.usage {
            Some(Ok(period)) => Load::Ready(period.rows.clone()),
            Some(Err(why)) => Load::Failed(why.clone()),
            None => Load::Loading,
        },
        usage_limits: match &model.usage {
            Some(Ok(period)) => period.limits.clone(),
            _ => Vec::new(),
        },
    }
}

// ------------------------------------------------------------------ attention

fn attention(model: &Model, extras: &Extras) -> Vec<Attention> {
    model
        .attention()
        .filter_map(|item| {
            let step = item
                .step_run_id
                .as_ref()
                .and_then(|id| find_step(model, id))
                .map(|(_, step)| step.path.clone());
            let mission = item.mission_id.clone();
            let (tier, kind) = match item.attention_kind.as_str() {
                "human-gate" => (
                    Tier::Stopped,
                    AttentionKind::Review {
                        question: item.detail.clone(),
                        because: "the step cannot finish until you answer".into(),
                        look_at: item
                            .target_states
                            .iter()
                            .map(|target| (target.id.clone(), target.state.clone()))
                            .chain(
                                item.targets
                                    .iter()
                                    .filter(|target| {
                                        !item.target_states.iter().any(|state| &state.id == *target)
                                    })
                                    .map(|target| ("review".to_owned(), target.clone())),
                            )
                            .collect(),
                        step: step.clone().unwrap_or_default(),
                    },
                ),
                "launch-approval" => (
                    Tier::Today,
                    AttentionKind::Launch {
                        planner: "The planner".into(),
                        name: mission
                            .as_deref()
                            .map(short)
                            .unwrap_or_else(|| item.title.clone()),
                        preview: extras
                            .previews
                            .get(&item.header.id)
                            .cloned()
                            .unwrap_or(Load::Loading),
                    },
                ),
                "revision-approval" => (
                    Tier::Today,
                    AttentionKind::Revision {
                        reason: item.detail.clone(),
                        changes: vec![],
                    },
                ),
                // An agent stopped on the person: a request to answer, not a fault to clear.
                "person-step" | "agent-request" => (
                    Tier::Stopped,
                    AttentionKind::Request {
                        from: item
                            .requester_id
                            .as_deref()
                            .map(|id| {
                                model
                                    .agents()
                                    .find(|agent| agent.header.id == id)
                                    .map(crate::agent_label)
                                    .unwrap_or_else(|| short(id))
                            })
                            .unwrap_or_else(|| "An agent".into()),
                        from_id: item.requester_id.clone().unwrap_or_default(),
                        question: clean_message_text(&item.detail),
                    },
                ),
                // Home holds only requests and reviews. Messages stay in conversations, and st
                // sends each fault to the agent that owns it.
                _ => return None,
            };
            let agent = item
                .step_run_id
                .as_ref()
                .and_then(|id| find_step(model, id))
                .and_then(|(_, step)| step.claimant.clone())
                .or_else(|| {
                    // An agent working in the mission, for a gate that has no claimant.
                    item.mission_id.as_ref().and_then(|mission| {
                        model
                            .agents()
                            .find(|agent| {
                                agent
                                    .current_work
                                    .iter()
                                    .any(|work| &work.mission_id == mission)
                            })
                            .map(|agent| agent.header.id.clone())
                    })
                })
                .or_else(|| {
                    item.source_id
                        .starts_with("agent/")
                        .then(|| item.source_id.clone())
                })
                .or_else(|| match &kind {
                    AttentionKind::Message { from, .. } if from.starts_with("agent/") => {
                        Some(from.clone())
                    }
                    AttentionKind::Request { from_id, .. } if from_id.starts_with("agent/") => {
                        Some(from_id.clone())
                    }
                    _ => None,
                });
            let related = item
                .targets
                .iter()
                .filter(|target| **target != item.header.id)
                .map(|target| {
                    let state = item
                        .target_states
                        .iter()
                        .find(|state| &state.id == target)
                        .map(|state| state.state.clone());
                    (target.clone(), state)
                })
                .collect();
            // Who raised it: the requester st names, else the agent the item is about. st's own
            // machinery reads as st.
            let raised_by = item
                .requester_id
                .clone()
                .or_else(|| {
                    item.extra
                        .get("actor")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .or_else(|| agent.clone())
                .map(|who| match who.as_str() {
                    "daemon/runtime" | "agent/st3/reconciler" => "st".to_owned(),
                    _ => model
                        .agents()
                        .find(|candidate| candidate.header.id == who)
                        .map(|candidate| format!("{} · {who}", crate::agent_label(candidate)))
                        .unwrap_or(who),
                })
                // A gate or fault nobody filed by hand comes from its mission.
                .or_else(|| {
                    item.mission_id.as_ref().map(|mission| {
                        format!("the {} mission", mission.trim_start_matches("mission/"))
                    })
                });
            Some(Attention {
                id: item.header.id.clone(),
                agent,
                related,
                raised_by,
                tier,
                title: extras
                    .bodies
                    .get(&item.header.id)
                    .and_then(|(_, title, content)| {
                        title.clone().filter(|title| !title.is_empty()).or_else(|| {
                            content
                                .lines()
                                .find(|line| !line.trim().is_empty())
                                .map(str::to_owned)
                        })
                    })
                    .map(|title| clean_message_text(&title))
                    .unwrap_or_else(|| clean_message_text(&item.title)),
                waiting: step.map(|step| format!("step {step}")),
                age: age(&item.requested_at),
                mission,
                kind,
                actions: item.actions.clone(),
            })
        })
        .collect()
}

/// Build a launch preview from a launch variant's normalized mission.
pub fn preview(name: &str, normalized: &Value) -> MissionPreview {
    let strings = |value: &Value| {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(clean_message_text))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let order = normalized["display_order"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let steps_value = normalized["steps"].as_object().cloned().unwrap_or_default();
    let mut keys = order
        .into_iter()
        .filter(|key| steps_value.contains_key(key))
        .collect::<Vec<_>>();
    for key in steps_value.keys() {
        if !keys.contains(key) {
            keys.push(key.clone());
        }
    }
    let mut agents = Vec::new();
    let steps = keys
        .iter()
        .filter_map(|key| steps_value.get(key).map(|step| (key, step)))
        .filter(|(_, step)| !step["finally"].as_bool().unwrap_or(false))
        .map(|(key, step)| {
            let selector = &step["work_selector"];
            let assignee = match selector["kind"].as_str() {
                Some("assigned") => selector["agent"].as_str().map(short).unwrap_or_default(),
                Some("available") => "any of several".into(),
                Some("agentless") => "st".into(),
                _ => "—".into(),
            };
            if let Some(agent) = selector["agent"].as_str()
                && !agents
                    .iter()
                    .any(|known: &PreviewAgent| known.name == short(agent))
            {
                agents.push(PreviewAgent {
                    name: short(agent),
                    harness: Harness::Unknown,
                    host: String::new(),
                });
            }
            let after = step["dependencies"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|dependency| dependency["step"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let asks_you = step["gates"]
                .as_array()
                .is_some_and(|gates| gates.iter().any(|gate| gate.get("reviewer").is_some()));
            PreviewStep {
                name: step["path"].as_str().unwrap_or(key).to_owned(),
                assignee,
                after,
                asks_you,
            }
        })
        .collect();
    MissionPreview {
        name: name.to_owned(),
        goals: strings(&normalized["goals"]),
        steps,
        agents,
        workspace: String::new(),
    }
}

// --------------------------------------------------------------------- agents

fn harness(driver: Option<&str>) -> Harness {
    match driver.unwrap_or("") {
        driver if driver.contains("claude") => Harness::Claude,
        driver if driver.contains("codex") => Harness::Codex,
        driver if driver.contains("omp") => Harness::Omp,
        "pi" => Harness::Pi,
        _ => Harness::Unknown,
    }
}

fn agents(model: &Model) -> Vec<Agent> {
    let mut agents = model
        .agents()
        .map(|agent| {
            let state = match (agent.state.as_str(), agent.harness_state.as_deref()) {
                _ if agent.fault.is_some() => AgentState::Fault,
                // A seat whose message path stopped polling or cannot hand off takes no
                // messages, however ready its harness looks. An `outdated` path still delivers.
                _ if agent
                    .delivery
                    .as_ref()
                    .is_some_and(|delivery| delivery.state == "stale") =>
                {
                    AgentState::Fault
                }
                ("failed", _) => AgentState::Fault,
                ("running", Some("working")) => AgentState::Working,
                ("running", _) => AgentState::Idle,
                ("waiting", Some("ready" | "working" | "idle"))
                    if agent.blocked_on.as_deref() == Some("human") =>
                {
                    AgentState::NeedsYou
                }
                ("waiting", Some("unauthenticated" | "blocked")) => AgentState::NeedsYou,
                ("waiting" | "starting" | "desired", _) => AgentState::Starting,
                ("stopped", _) => AgentState::Stopped,
                _ => AgentState::Unknown,
            };
            let host = agent
                .host_id
                .as_deref()
                .map(|host| host.trim_start_matches("host/").to_owned())
                .unwrap_or_else(|| "?".into());
            // st names the step each agent holds and the ones queued for it.
            let work = agent.current_work.first();
            Agent {
                id: agent.header.id.clone(),
                name: crate::agent_label(agent),
                harness: harness(agent.driver.as_deref()),
                state,
                host,
                worktree: None,
                mission: work.map(|work| work.mission_id.clone()),
                step: work.map(|work| work.path.clone()),
                activity: age(&agent.header.updated_at),
                unmanaged: false,
                // A running agent with a runtime is worth trying; opening it finds the terminal.
                terminal: !agent.runtime_ids.is_empty()
                    && !matches!(agent.state.as_str(), "stopped" | "failed"),
                parent: agent
                    .under
                    .first()
                    .map(|relation| relation.agent_id.clone()),
                details: AgentDetails {
                    goal: work.and_then(|work| work.goal.as_deref().map(clean_message_text)),
                    claimed: work.map(|work| format!("{} ago", age(&work.since))),
                    next: agent
                        .next_work
                        .as_ref()
                        .map(|label| label_text(model, label)),
                    queue: agent
                        .upcoming_work
                        .iter()
                        .map(|label| label_text(model, label))
                        .collect(),
                    queued: agent.queued_work_count,
                    harness_state: agent.harness_state.clone(),
                    runtime: None,
                    fault: agent.fault.clone(),
                    under: agent.under.first().map(|relation| {
                        model
                            .agents()
                            .find(|parent| parent.header.id == relation.agent_id)
                            .map(crate::agent_label)
                            .unwrap_or_else(|| short(&relation.agent_id))
                    }),
                },
                subagents: agent
                    .subagents
                    .iter()
                    .map(|subagent| Subagent {
                        id: subagent.id.clone(),
                        kind: subagent.subagent_type.clone(),
                        description: subagent.description.clone(),
                        age: subagent.started_at.as_deref().map(age).unwrap_or_default(),
                    })
                    .collect(),
            }
        })
        .collect::<Vec<_>>();
    let gateway = gateway(model)
        .map(|host| host.trim_start_matches("host/").to_owned())
        .unwrap_or_default();
    agents.extend(model.undeclared_sessions().map(|session| {
        let driver = session.extra.get("driver").and_then(Value::as_str);
        let workspace = session
            .extra
            .get("workspace")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Agent {
            id: session.header.id.clone(),
            name: format!(
                "{} in {}",
                driver.unwrap_or("harness"),
                workspace
                    .as_deref()
                    .map(|path| path.rsplit('/').next().unwrap_or(path))
                    .unwrap_or("?")
            ),
            harness: harness(driver),
            state: AgentState::Unknown,
            host: gateway.clone(),
            worktree: workspace,
            mission: None,
            step: None,
            activity: age(&session.header.updated_at),
            unmanaged: true,
            parent: None,
            details: AgentDetails::default(),
            terminal: false,
            subagents: Vec::new(),
        }
    }));
    agents
}

// ------------------------------------------------------------------- machines

/// A member heard from within this long is online, even without a direct link.
const HEARD_RECENTLY: chrono::Duration = chrono::Duration::minutes(5);

fn machines(model: &Model) -> Vec<Machine> {
    let gateway = gateway(model).unwrap_or_default();
    model
        .machines()
        .map(|machine| {
            // A member this machine does not replicate with directly is still heard through
            // the others: its agents' activity arrives with everything else.
            let heard = machine
                .transports
                .iter()
                .filter_map(|transport| transport.last_success_at.as_deref())
                .chain(
                    model
                        .agents()
                        .filter(|agent| agent.host_id.as_deref() == Some(machine.host_id.as_str()))
                        .filter_map(|agent| agent.last_activity_at.as_deref()),
                )
                .filter_map(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                .max();
            let recent = heard.is_some_and(|at| chrono::Utc::now() - at.to_utc() < HEARD_RECENTLY);
            let reach = match machine.state.as_str() {
                _ if machine.host_id == gateway => Reach::Here,
                "local" => Reach::Here,
                "reachable" => Reach::Direct,
                _ if recent => Reach::Indirect,
                "indeterminate" => Reach::Unknown,
                _ => Reach::Offline,
            };
            Machine {
                name: machine.name.clone(),
                reach,
                platform: String::new(),
                seen: heard
                    .map(|at| crate::age_label(&at.to_rfc3339(), &now()))
                    .unwrap_or_else(|| "never".into()),
                load: Some(match machine.occupancy.running_runtimes {
                    1 => "1 running runtime".to_owned(),
                    count => format!("{count} running runtimes"),
                }),
                links: machine
                    .transports
                    .iter()
                    // This machine's own socket is not a link to anywhere.
                    .filter(|transport| transport.status != "local")
                    .map(|transport| {
                        let up = matches!(
                            transport.status.as_str(),
                            "ok" | "up" | "connected" | "reachable" | "healthy"
                        );
                        let detail = match (up, transport.last_success_at.as_deref()) {
                            (true, _) => "up".to_owned(),
                            (false, Some(at)) => format!("down · last worked {}", age(at)),
                            (false, None) => "no direct link".to_owned(),
                        };
                        (transport.protocol.clone(), up, detail)
                    })
                    .collect(),
                you_are_here: machine.host_id == gateway,
            }
        })
        .collect()
}

// -------------------------------------------------------------- conversations

/// Display names for the ids that appear in message headers.
pub fn names(model: &Model, person: &str) -> BTreeMap<String, String> {
    let mut names = model
        .agents()
        .map(|agent| (agent.header.id.clone(), crate::agent_label(agent)))
        .collect::<BTreeMap<_, _>>();
    names.insert(person.to_owned(), "you".into());
    names
}

#[cfg(test)]
use st3_conversation_ui::adapt::from_harness;
pub use st3_conversation_ui::adapt::{conversation, unreadable_transcript};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HARNESS_TAGS: &[&str] = &[
        "task-notification",
        "task-id",
        "tool-use-id",
        "output-file",
        "<status>",
        "<summary>",
        "<result>",
        "<note>",
        "<channel",
        "system-reminder",
        "command-name",
        "command-message",
        "command-args",
        "local-command",
        "environment_context",
        "<permissions",
        "collaboration_mode",
        "multi_agent_mode",
        "apps_instructions",
        "plugins_instructions",
        "skills_instructions",
        "turn_aborted",
        "send_user_message_question_reply",
        "<cwd>",
        "<shell>",
        "<timezone>",
    ];

    #[test]
    fn small_talk_in_the_timeline_draws_as_mail_and_step_pings_as_events() {
        let timeline: Vec<TimelineEntry> = serde_json::from_value(serde_json::json!([
            {"id":"e1","sequence":4,"revision":1,"timestamp":"2026-09-29T10:00:00Z","role":"user","type":"message","final":true,
             "body":{"message_id":"message/one","from":"agent/example/harbor","to":"agent/example/cos","title":"A question"}},
            {"id":"e2","sequence":5,"revision":1,"timestamp":"2026-09-29T10:00:00Z","role":"user","type":"content","final":true,
             "body":{"media_type":"text/plain","text":"Can you look?"}},
            {"id":"e3","sequence":8,"revision":1,"timestamp":"2026-09-29T10:01:00Z","role":"user","type":"message","final":true,
             "body":{"message_id":"message/two","from":"daemon/runtime","to":"agent/example/cos","title":"Mission step ready: review"}},
            {"id":"e4","sequence":9,"revision":1,"timestamp":"2026-09-29T10:01:00Z","role":"user","type":"content","final":true,
             "body":{"media_type":"text/plain","text":"A mission step is ready."}},
            {"id":"e5a","sequence":11,"revision":1,"timestamp":"2026-09-29T10:02:00Z","role":"assistant","type":"message","final":true,
             "body":{"message_id":"msg_harness_turn"}},
            {"id":"e5","sequence":12,"revision":1,"timestamp":"2026-09-29T10:02:00Z","role":"assistant","type":"content","final":true,
             "body":{"media_type":"text/plain","text":"On it."}}
        ]))
        .unwrap();
        let names = BTreeMap::from([("agent/example/cos".to_owned(), "COS".to_owned())]);
        let entries = conversation(&timeline, &names);
        assert_eq!(entries.len(), 3, "{entries:#?}");
        match &entries[0].body {
            Body::Mail {
                from,
                to,
                subject,
                body,
                ..
            } => {
                assert_eq!(to, "COS");
                assert!(from.contains("harbor"), "{from}");
                assert_eq!(subject, "A question");
                assert_eq!(body, "Can you look?");
            }
            other => panic!("expected mail, got {other:?}"),
        }
        assert_eq!(entries[0].id, "message/one");
        assert!(
            matches!(&entries[1].body, Body::Event(title) if title == "Mission step ready: review")
        );
        // A harness turn's own message header is not Small Talk.
        assert!(matches!(&entries[2].body, Body::Assistant(text) if text == "On it."));
    }

    fn tool_pair(arguments: Value, content: Value) -> Vec<Entry> {
        let timeline: Vec<TimelineEntry> = serde_json::from_value(json!([
            {"id":"c","sequence":1,"revision":1,"timestamp":"2026-09-30T10:00:00Z","role":"assistant","type":"tool_call","final":true,
             "body":{"call_id":"call-1","name":"exec","arguments":arguments}},
            {"id":"r","sequence":2,"revision":1,"timestamp":"2026-09-30T10:00:01Z","role":"tool","type":"tool_result","final":true,
             "body":{"call_id":"call-1","status":"success","media_type":"application/json","content":content}}
        ]))
        .unwrap();
        conversation(&timeline, &BTreeMap::new())
    }

    fn tool(entries: &[Entry]) -> (String, ToolState, Vec<String>) {
        match entries {
            [
                Entry {
                    body:
                        Body::Tool {
                            title,
                            state,
                            output,
                        },
                    ..
                },
            ] => (title.clone(), *state, output.clone()),
            other => panic!("expected one tool, got {other:?}"),
        }
    }

    #[test]
    fn codex_code_mode_shows_each_commands_output_and_names_the_commands() {
        let script = "const r=await Promise.allSettled([\ntools.exec_command({cmd:\"git status --short\"}),\ntools.exec_command({cmd:'cargo test -p harbor',max_output_tokens:8000})]);\ntext(JSON.stringify(r));";
        let reports = [
            json!({"status":"fulfilled","value":{"chunk_id":"a1","wall_time_seconds":0.1,"exit_code":0,"original_token_count":3,"output":" M src/lib.rs\n"}}),
            json!({"i":1,"result":{"status":"fulfilled","value":{"chunk_id":"b2","exit_code":101,"output":"test harbor::keys ... FAILED\n"}}}),
        ];
        let entries = tool_pair(
            Value::String(script.into()),
            json!([
                {"type":"input_text","text":"Script completed\nWall time 0.1 seconds\nOutput:\n"},
                {"type":"input_text","text":format!("{}\n{}", reports[0], reports[1])}
            ]),
        );
        let (title, state, output) = tool(&entries);
        assert_eq!(title, "$ git status --short (+1 more)");
        assert_eq!(state, ToolState::Failed, "a command exited 101");
        assert_eq!(
            output,
            [
                " M src/lib.rs",
                "",
                "test harbor::keys ... FAILED",
                "exit 101"
            ]
        );
    }

    #[test]
    fn a_result_st_cut_short_reads_as_far_as_it_goes_and_says_so() {
        let whole = json!([
            {"type":"input_text","text":"Output:\n"},
            {"type":"input_text","text":json!({"chunk_id":"c3","exit_code":0,"output":"line one\nline two\nline three that goes on"}).to_string()}
        ])
        .to_string();
        let cut = format!(
            "{}\n[st truncated this native timeline value]",
            &whole[..whole.find("three").unwrap()]
        );
        let entries = tool_pair(json!({"cmd":"cat notes.txt"}), Value::String(cut));
        let (title, _, output) = tool(&entries);
        assert_eq!(title, "exec cat notes.txt");
        assert_eq!(
            output,
            [
                "line one",
                "line two",
                "line",
                "… st kept only the start of this output"
            ]
        );
    }

    #[test]
    fn redacted_and_image_results_are_words_not_json() {
        let redacted = json!({"redacted":true,"sha256":"00ff","bytes":120});
        let (title, _, output) = tool(&tool_pair(redacted.clone(), redacted));
        assert_eq!(title, "exec");
        assert_eq!(output, ["output not recorded"]);
        let (_, _, output) = tool(&tool_pair(
            json!({"path":"shot.png"}),
            json!([{"type":"image","source":{"type":"base64","data":"iVBORw0KGgo="}}]),
        ));
        assert_eq!(output, ["[image]"]);
        let (title, _, _) = tool(&tool_pair(
            json!({"duration_ms":50000}),
            json!("Sleep completed."),
        ));
        assert_eq!(title, "exec 50s");
    }

    #[test]
    fn st_deliveries_in_a_harness_prompt_are_mail_or_a_line_never_markup() {
        let envelope = "<smalltalk-message id=\"a1\" from=\"agent/example/harbor\" to=\"agent/example/quay\" subject=\"Keys &amp; locks\" sha256=\"00\" graph=\"message/a1\">\nRotate &lt;all&gt; keys.\n</smalltalk-message>";
        let bodies = from_harness(true, envelope, &BTreeSet::new());
        match &bodies[..] {
            [
                Body::Mail {
                    from,
                    to,
                    subject,
                    body,
                    ..
                },
            ] => {
                assert_eq!(
                    (from.as_str(), to.as_str()),
                    ("agent/example/harbor", "agent/example/quay")
                );
                assert_eq!(subject, "Keys & locks");
                assert_eq!(body, "Rotate <all> keys.");
            }
            other => panic!("expected mail, got {other:?}"),
        }
        // Mail the stream already shows is marked delivered on the mail, not announced again.
        let shown = BTreeSet::from(["message/a1".to_owned()]);
        let bodies = from_harness(true, envelope, &shown);
        assert!(bodies.is_empty(), "{bodies:?}");
        let bodies = from_harness(
            true,
            "[PING from st3] message/b2 from agent/example/quay: Tide tables\nplease look",
            &BTreeSet::new(),
        );
        assert!(
            matches!(&bodies[..], [Body::User(text), Body::Event(line)]
                if text == "please look" && line == "delivered to the agent: Tide tables · from example/quay"),
            "{bodies:?}"
        );
    }

    #[test]
    fn the_persons_mail_says_when_the_agent_has_it_and_its_delivery_is_not_a_line() {
        let timeline: Vec<TimelineEntry> = serde_json::from_value(json!([
            {"id":"m","sequence":1,"revision":1,"timestamp":"2026-10-01T10:00:00Z","role":"user","type":"message","final":true,
             "body":{"message_id":"message/one","from":"person/avery","to":"agent/example/harbor/keeper"}},
            {"id":"c","sequence":2,"revision":1,"timestamp":"2026-10-01T10:00:00Z","role":"user","type":"content","final":true,
             "body":{"media_type":"text/plain","text":"How is the audit going?"}},
            {"id":"m2","sequence":3,"revision":1,"timestamp":"2026-10-01T10:00:01Z","role":"user","type":"message","final":true,
             "body":{"message_id":"message/two","from":"person/avery","to":"agent/example/harbor/keeper"}},
            {"id":"c2","sequence":4,"revision":1,"timestamp":"2026-10-01T10:00:01Z","role":"user","type":"content","final":true,
             "body":{"media_type":"text/plain","text":"And the keys?"}},
            {"id":"h","sequence":5,"revision":1,"timestamp":"2026-10-01T10:00:02Z","role":"user","type":"content","final":true,
             "body":{"media_type":"text/plain","text":"<channel source=\"plugin:st3-channel:st3\" from=\"person/avery\">[st3-delivery:1.md]\n[PING from st3] message/one from person/avery: (no subject)\n</channel>"}},
        ]))
        .unwrap();
        let names = BTreeMap::from([("person/avery".to_owned(), "you".to_owned())]);
        let entries = conversation(&timeline, &names);
        let marks = entries
            .iter()
            .map(|entry| match &entry.body {
                Body::Mail { delivered, .. } => format!("mail {delivered}"),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            marks,
            ["mail true", "mail false"],
            "no delivery line, once or twice"
        );
        let doc = st3_conversation_ui::conversation::Cache::default().render(
            &entries,
            80,
            &Default::default(),
            "⠋",
            &super::super::theme::conversation(),
        );
        let text = doc
            .lines
            .iter()
            .map(st3_conversation_ui::text::plain)
            .collect::<Vec<_>>();
        assert!(
            text.iter().any(|line| line.contains("✓✓ delivered")),
            "{text:?}"
        );
        assert!(text.iter().any(|line| line.contains("✓ sent")), "{text:?}");
    }

    #[test]
    fn a_seat_that_has_said_nothing_since_it_started_shows_its_small_talk() {
        let timeline: Vec<TimelineEntry> = serde_json::from_value(json!([
            {"id":"m","sequence":1,"revision":1,"timestamp":"2026-10-01T10:00:00Z","role":"user","type":"message","final":true,
             "body":{"message_id":"message/one","from":"person/avery","to":"agent/example/harbor/keeper","title":"Status?"}},
            {"id":"c","sequence":2,"revision":1,"timestamp":"2026-10-01T10:00:00Z","role":"user","type":"content","final":true,
             "body":{"media_type":"text/plain","text":"How is the audit going?"}},
            {"id":"n","sequence":3,"revision":1,"timestamp":"2026-10-01T10:00:01Z","role":"system","type":"error","final":true,
             "body":{"code":"transcript-not-bound","message":"transcript not bound: Claude session 0190 has no transcript file yet","retryable":true,
                     "details":{"driver":"claude","not_yet":true}}},
        ]))
        .unwrap();
        assert_eq!(unreadable_transcript(&timeline), None, "nothing is wrong");
        let entries = conversation(&timeline, &BTreeMap::new());
        assert!(matches!(&entries[0].body, Body::Mail { subject, .. } if subject == "Status?"));
        assert!(
            matches!(&entries[1].body, Body::Event(line) if line == "nothing in the harness yet since this seat started"),
            "{entries:?}"
        );
    }

    #[test]
    fn half_a_conversation_is_not_shown_and_says_why() {
        let entries = |notice: Option<Value>| -> Vec<TimelineEntry> {
            let mut rows = vec![
                json!({"id":"m","sequence":1,"revision":1,"timestamp":"2026-10-01T10:00:00Z","role":"user","type":"message","final":true,
                    "body":{"message_id":"message/one","from":"person/avery","to":"agent/example/harbor/keeper","title":"Status?"}}),
                json!({"id":"c","sequence":2,"revision":1,"timestamp":"2026-10-01T10:00:00Z","role":"user","type":"content","final":true,
                    "body":{"media_type":"text/plain","text":"How is the audit going?"}}),
            ];
            rows.extend(notice);
            serde_json::from_value(Value::Array(rows)).unwrap()
        };
        assert_eq!(
            unreadable_transcript(&entries(None)),
            None,
            "a whole conversation shows"
        );
        let notice = |details: Value| {
            json!({"id":"n","sequence":3,"revision":1,"timestamp":"2026-10-01T10:00:01Z","role":"system","type":"error","final":true,
                "body":{"code":"transcript-not-bound","message":"transcript not bound: the transcript could not be read: line 12: expected value","retryable":true,"details":details}})
        };
        assert_eq!(
            unreadable_transcript(&entries(Some(notice(
                json!({"driver":"omp","transcript":"/srv/example/omp/sessions/harbor/0190.jsonl"})
            ))))
            .as_deref(),
            Some(
                "This conversation could not be loaded: the transcript could not be read: line 12: expected value (transcript /srv/example/omp/sessions/harbor/0190.jsonl)"
            )
        );
        assert_eq!(
            unreadable_transcript(&entries(Some(notice(json!({"driver":"omp"}))))).as_deref(),
            Some(
                "This conversation could not be loaded: the transcript could not be read: line 12: expected value"
            )
        );
    }

    #[test]
    fn delivery_pauses_that_recovered_fold_into_one_quiet_line() {
        let diagnostic = |id: &str, at: &str, code: &str, severity: &str, message: &str| {
            json!({"id":id,"sequence":1,"revision":1,"timestamp":at,"role":"system","type":"error","final":true,
                "body":{"code":code,"message":message,"retryable":true,"details":{"severity":severity}}})
        };
        let paused = "Native conversation delivery over claude-channel paused while the st daemon was unreachable; the driver stayed online and retried every second.";
        let resumed = "Native conversation delivery over claude-channel recovered and resumed replay from durable graph state.";
        let mut rows = Vec::new();
        for minute in 0..3 {
            rows.push(diagnostic(
                &format!("p{minute}"),
                &format!("2026-09-30T10:0{minute}:00Z"),
                "native-delivery-degraded",
                "warning",
                paused,
            ));
            rows.push(diagnostic(
                &format!("r{minute}"),
                &format!("2026-09-30T10:0{minute}:30Z"),
                "native-delivery-recovered",
                "warning",
                resumed,
            ));
        }
        rows.push(json!({"id":"a1","sequence":2,"revision":1,"timestamp":"2026-09-30T10:05:00Z","role":"assistant","type":"content","final":true,
            "body":{"media_type":"text/plain","text":"Still here."}}));
        rows.push(diagnostic(
            "p9",
            "2026-09-30T10:06:00Z",
            "native-delivery-degraded",
            "warning",
            paused,
        ));
        rows.push(diagnostic(
            "e1",
            "2026-09-30T10:07:00Z",
            "harness-crashed",
            "error",
            "the harness exited",
        ));
        let timeline: Vec<TimelineEntry> = serde_json::from_value(json!(rows)).unwrap();
        let events: Vec<String> = conversation(&timeline, &BTreeMap::new())
            .into_iter()
            .map(|entry| match entry.body {
                Body::Event(text) => text,
                Body::Assistant(text) => format!("assistant: {text}"),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            events,
            [
                "delivery paused, then resumed ×3",
                "assistant: Still here.",
                "delivery paused · st unreachable",
                "error: the harness exited",
            ]
        );
    }

    fn rendered(fixture: &str) -> String {
        let timeline: Vec<TimelineEntry> = serde_json::from_str(fixture).unwrap();
        let entries = conversation(&timeline, &BTreeMap::new());
        let doc = super::super::conversation::Cache::default().render(
            &entries,
            100,
            &Default::default(),
            "⠋",
            st3_conversation_ui::Density::Full,
        );
        doc.lines
            .iter()
            .map(super::super::text::plain)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_clean(text: &str) {
        for tag in HARNESS_TAGS {
            let needle = if tag.starts_with('<') {
                tag.to_string()
            } else {
                format!("<{tag}")
            };
            assert!(!text.contains(&needle), "{needle} leaked into:\n{text}");
            assert!(
                !text.contains(&format!("</{}", tag.trim_start_matches('<'))),
                "closing {tag} leaked into:\n{text}"
            );
        }
    }

    #[test]
    fn a_real_claude_transcript_shows_no_harness_markup_and_keeps_the_conversation() {
        let text = rendered(include_str!(
            "../../../../fixtures/clients/transcripts/claude.json"
        ));
        assert_clean(&text);
        for kept in [
            "Please check why the nightly build failed and fix it.",
            "Two helpers stopped at the session limit",
            "Thanks. Keep going.",
            "background task failed: Agent \"Lane C: client operations\" failed",
            "delivered to the agent: re: Change widget license to MIT",
            "delivered to the agent: Plan step ready",
            "/usage",
            "Login successful",
            "The nightly build is fixed",
        ] {
            assert!(text.contains(kept), "lost {kept:?} from:\n{text}");
        }
        assert!(
            !text.contains("gentle reminder"),
            "system reminders are for the model:\n{text}"
        );
        assert!(
            !text.contains("DO NOT respond"),
            "the local command caveat is for the model:\n{text}"
        );
        // The agent may still talk about a tag by name.
        assert!(text.contains("<smalltalk-message>"), "{text}");
    }

    #[test]
    fn a_real_codex_transcript_shows_no_harness_markup_and_keeps_the_conversation() {
        let text = rendered(include_str!(
            "../../../../fixtures/clients/transcripts/codex.json"
        ));
        assert_clean(&text);
        for kept in [
            "Rotate the signing keys in harbor",
            "Starting with an inventory of every key reader.",
            "the turn was interrupted",
            "You run it as me",
            "Thanks, I'll wait for the revision output.",
        ] {
            assert!(text.contains(kept), "lost {kept:?} from:\n{text}");
        }
        for context in [
            "sandbox_mode",
            "Collaboration Mode",
            "Plugins",
            "Skills",
            "/tmp/st3-osh",
        ] {
            assert!(
                !text.contains(context),
                "{context} is harness context:\n{text}"
            );
        }
    }

    #[test]
    fn an_unclosed_wrapper_cannot_leak() {
        let bodies = from_harness(
            true,
            "hello\n<system-reminder>\nnever closed",
            &BTreeSet::new(),
        );
        assert!(
            matches!(&bodies[..], [Body::User(text)] if text == "hello"),
            "{bodies:?}"
        );
    }

    #[test]
    fn a_launch_preview_reads_steps_assignees_dependencies_and_person_gates() {
        let normalized = json!({
            "goals": ["Audit dependencies"],
            "display_order": ["scan", "merge"],
            "steps": {
                "scan": {"path": "scan", "work_selector": {"kind": "assigned", "agent": "agent/example/auditor"}, "dependencies": [], "gates": []},
                "merge": {"path": "merge", "work_selector": {"kind": "agentless"}, "dependencies": [{"dependency": "step", "step": "scan", "state": "completed"}], "gates": [{"reviewer": "person/robin"}]}
            }
        });
        let preview = preview("harbor/audit", &normalized);
        assert_eq!(preview.goals, vec!["Audit dependencies"]);
        assert_eq!(preview.steps[0].assignee, "example/auditor");
        assert_eq!(preview.steps[1].after, vec!["scan"]);
        assert!(preview.steps[1].asks_you);
        assert_eq!(preview.agents.len(), 1);
    }

    #[test]
    fn tool_calls_take_their_results_and_messages_merge_by_time() {
        let timeline: Vec<TimelineEntry> = serde_json::from_value(json!([
            {"id": "1", "sequence": 1, "revision": 1, "timestamp": "2026-09-28T09:00:00Z", "role": "assistant", "final": true,
             "type": "tool_call", "body": {"call_id": "c", "name": "Bash", "arguments": {"command": "cargo test"}}},
            {"id": "2", "sequence": 2, "revision": 1, "timestamp": "2026-09-28T09:00:05Z", "role": "tool", "final": true,
             "type": "tool_result", "body": {"call_id": "c", "status": "error", "media_type": "text/plain", "content": "1 failed"}}
        ]))
        .unwrap();
        let entries = conversation(&timeline, &BTreeMap::new());
        assert_eq!(entries.len(), 1);
        match &entries[0].body {
            Body::Tool {
                title,
                state,
                output,
            } => {
                assert_eq!(title, "$ cargo test");
                assert_eq!(*state, ToolState::Failed);
                assert_eq!(output, &vec!["1 failed".to_owned()]);
            }
            other => panic!("{other:?}"),
        }
    }

    fn window(items: Vec<Value>) -> crate::model::Collection {
        crate::model::Collection {
            items: items
                .into_iter()
                .map(|item| serde_json::from_value(item).unwrap())
                .collect(),
            snapshot: Some(st3_client::Snapshot {
                id: "snapshot/1".into(),
                host_id: "host/harbor".into(),
                store_index: 1,
                projection_version: "1".into(),
                created_at: "2026-09-29T10:00:00Z".into(),
            }),
            truncated: false,
            sync: None,
        }
    }

    #[test]
    fn machines_say_how_they_are_reached_and_never_call_a_heard_member_offline() {
        let now = chrono::Utc::now();
        let ago = |minutes: i64| (now - chrono::Duration::minutes(minutes)).to_rfc3339();
        let machine = |name: &str, state: &str, transport: Value| {
            json!({
                "id": format!("machine/{name}"), "kind": "machine", "revision": "r",
                "updated_at": "2026-09-29T09:00:00Z", "host_id": format!("host/{name}"),
                "name": name, "state": state, "capacity": {"state": "unknown", "reason": "no capacity observation"},
                "occupancy": {"running_runtimes": 1}, "transports": [transport],
            })
        };
        let mut model = Model::default();
        model.machines = window(vec![
            machine(
                "harbor",
                "local",
                json!({"protocol": "unix", "status": "local", "last_success_at": null}),
            ),
            machine(
                "quay",
                "reachable",
                json!({"protocol": "replication", "status": "up", "last_success_at": ago(0)}),
            ),
            // No direct link from here, but its agent spoke a minute ago through another member.
            machine(
                "wren",
                "last-seen",
                json!({"protocol": "replication", "status": "last-seen", "last_success_at": null}),
            ),
            machine(
                "gull",
                "last-seen",
                json!({"protocol": "replication", "status": "last-seen", "last_success_at": ago(90)}),
            ),
        ]);
        model.agents = window(vec![json!({
            "id": "agent/example/wren/probe", "kind": "agent", "revision": "r",
            "updated_at": "2026-09-29T09:00:00Z", "name": "fleet/wren/probe", "state": "running",
            "reachability": "remote", "harness_state": "idle", "next_work_id": null, "next_work": null,
            "host_id": "host/wren", "last_activity_at": ago(1), "runtime_ids": [],
            "current_work_ids": [], "upcoming_work_ids": [], "queued_work_count": 0,
            "current_work": [], "upcoming_work": [], "under": [],
        })]);
        let world = world(&model, "person/avery", &Extras::default());
        let Load::Ready(machines) = &world.machines else {
            panic!("machines load")
        };
        let reach = machines
            .iter()
            .map(|machine| (machine.name.as_str(), machine.reach, machine.seen.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(reach[0], ("harbor", Reach::Here, "never"));
        assert_eq!(reach[1].1, Reach::Direct);
        assert_eq!(reach[2].1, Reach::Indirect, "wren's agent was heard 1m ago");
        assert_eq!((reach[3].1, reach[3].2), (Reach::Offline, "1h ago"));
        assert!(
            machines[0].links.is_empty(),
            "the local socket is not a link"
        );
        assert_eq!(machines[2].links[0].2, "no direct link");
        for machine in machines {
            assert!(machine.platform.is_empty(), "st reports no platform");
            assert!(!machine.seen.contains("not seen"));
        }
    }

    #[test]
    fn a_waiting_human_ask_needs_you_even_while_the_harness_activity_is_working() {
        let mut model = Model::default();
        let resource = |state: &str, activity: &str, blocked_on: Option<&str>| {
            serde_json::json!({
                "id": "agent/human-omp", "kind": "agent", "revision": "r1",
                "updated_at": "2026-09-30T12:00:00Z", "name": "human-omp",
                "state": state, "reachability": "local", "harness_state": activity,
                "blocked_on": blocked_on, "runtime_ids": [], "under": [],
            })
        };
        model.agents = window(vec![resource("waiting", "working", Some("human"))]);
        assert_eq!(agents(&model)[0].state, AgentState::NeedsYou);
        model.agents = window(vec![resource("running", "working", None)]);
        assert_eq!(agents(&model)[0].state, AgentState::Working);
        model.agents = window(vec![resource("failed", "working", Some("human"))]);
        assert_eq!(agents(&model)[0].state, AgentState::Fault);
        model.agents = window(vec![resource("waiting", "indeterminate", Some("human"))]);
        assert_eq!(agents(&model)[0].state, AgentState::Starting);
    }

    #[test]
    fn an_agents_subagents_come_from_st_as_part_of_it() {
        let mut model = Model::default();
        model.agents = window(vec![serde_json::json!({
            "id": "agent/example/harbor/keeper", "kind": "agent", "revision": "r2",
            "updated_at": "2026-09-29T09:59:00Z", "name": "fleet/harbor/keeper",
            "state": "running", "reachability": "local", "harness_state": "working",
            "runtime_ids": [], "under": [],
            "subagents": [
                {"id": "a1", "subagent_type": "Explore", "description": "map the code",
                 "driver": "claude", "started_at": "2026-09-29T09:58:00Z",
                 "lease_expires_at": "2026-09-29T10:08:00Z"},
                {"id": "019a-thread", "driver": "codex",
                 "lease_expires_at": "2026-09-29T10:08:00Z"}
            ],
        })]);
        let world = world(&model, "person/avery", &Extras::default());
        let Load::Ready(agents) = &world.agents else {
            panic!("agents load from the agents window")
        };
        let subagents = &agents[0].subagents;
        assert_eq!(
            subagents.iter().map(Subagent::label).collect::<Vec<_>>(),
            ["map the code · Explore", "019a-thread"]
        );
        assert!(!subagents[0].age.is_empty());
        assert!(
            subagents[1].age.is_empty(),
            "st did not say when it started"
        );
    }

    #[test]
    fn agents_read_joined_work_without_work_or_runtime_lists() {
        let step = |id: &str, path: &str, state: &str, claimant: Option<&str>| {
            serde_json::json!({
                "id": id, "path": path, "state": state, "attempt": 1,
                "assignee": "agent/example/harbor/keeper", "claimant": claimant,
                "since": "2026-09-29T09:58:00Z", "goals": [format!("Do {path}.")],
                "constraints": [], "blockers": [],
            })
        };
        let label = |id: &str, path: &str, state: &str| {
            serde_json::json!({
                "id": id, "mission_id": "mission/fleet/harbor/audit",
                "mission_run_id": "mission-run/audit-1", "path": path,
                "title": null, "goal": format!("Do {path}."), "state": state,
                "since": "2026-09-29T09:58:00Z",
            })
        };
        let run = |id: &str, status: &str, steps: Vec<Value>| {
            serde_json::json!({
                "id": id, "requester": "person/avery", "status": status, "phase": "normal",
                "progress": {"done": 0, "total": steps.len()}, "current_steps": [],
                "must_act": "agent", "state_since": "2026-09-29T09:58:00Z", "steps": steps,
            })
        };
        let mut model = Model::default();
        model.missions = window(vec![serde_json::json!({
            "id": "mission/fleet/harbor/audit", "kind": "mission", "revision": "r1",
            "updated_at": "2026-09-29T09:58:00Z", "title": "fleet/harbor/audit",
            "state": "running", "mission_revision": "r1",
            "runs": ["mission-run/audit-0", "mission-run/audit-1"],
            "run_details": [
                // A finished earlier run whose steps must not mix with the open one.
                run("mission-run/audit-0", "completed", vec![step("step-run/old/scan", "scan", "completed", None)]),
                run("mission-run/audit-1", "running", vec![
                    step("step-run/audit-1/scan", "scan", "claimed", Some("agent/example/harbor/keeper")),
                    step("step-run/audit-1/report", "report", "ready", None),
                ]),
            ],
        })]);
        model.agents = window(vec![serde_json::json!({
            "id": "agent/example/harbor/keeper", "kind": "agent", "revision": "r2",
            "updated_at": "2026-09-29T09:59:00Z", "name": "fleet/harbor/keeper",
            "state": "running", "reachability": "local", "harness_state": "working",
            "host_id": "host/lighthouse", "runtime_ids": ["runtime/keeper"],
            "current_work_ids": ["step-run/audit-1/scan"],
            "next_work_id": "step-run/audit-1/report",
            "upcoming_work_ids": ["step-run/audit-1/report"], "queued_work_count": 1,
            "current_work": [label("step-run/audit-1/scan", "scan", "claimed")],
            "next_work": label("step-run/audit-1/report", "report", "ready"),
            "upcoming_work": [label("step-run/audit-1/report", "report", "ready")],
            "under": [],
        })]);
        assert!(model.work.items.is_empty() && model.runtimes.items.is_empty());
        let world = world(&model, "person/avery", &Extras::default());

        let Load::Ready(agents) = &world.agents else {
            panic!("agents load from the agents window alone")
        };
        let agent = &agents[0];
        assert_eq!(agent.host, "lighthouse");
        assert!(agent.terminal);
        assert_eq!(agent.mission.as_deref(), Some("mission/fleet/harbor/audit"));
        assert_eq!(agent.step.as_deref(), Some("scan"));
        assert_eq!(agent.details.goal.as_deref(), Some("Do scan."));
        assert_eq!(
            agent.details.next.as_deref(),
            Some("fleet/harbor · Audit › report")
        );
        assert_eq!(agent.details.queue, ["fleet/harbor · Audit › report"]);
        assert_eq!(world.host, "harbor");
    }
}
