//! Live st3 data, turned into the view model the screens draw.
//!
//! A collection with no snapshot has not loaded yet, so it becomes `Load::Loading`, never an
//! empty list. Anything the graph does not say stays unsaid.

use super::view::*;
use crate::model::{Model, clean_message_text};
use serde_json::Value;
use st3_client::{MissionStep, WorkLabel};
use st3_client::{TimelineBody, TimelineEntry, TimelineRole, TimelineToolStatus};
use std::collections::BTreeMap;

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

/// The steps st sent with a mission: its open runs' steps, or its latest run's when none is open.
fn mission_steps(mission: &st3_client::Mission) -> Vec<&MissionStep> {
    let finished = |status: &str| matches!(status, "completed" | "failed" | "cancelled");
    let open = mission
        .run_details
        .iter()
        .filter(|run| !finished(&run.status))
        .collect::<Vec<_>>();
    let runs = if open.is_empty() {
        mission.run_details.last().into_iter().collect()
    } else {
        open
    };
    runs.into_iter()
        .flat_map(|run| run.steps.iter().flatten())
        .collect()
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

pub fn world(model: &Model, person: &str, extras: &Extras) -> World {
    let link = if let Some(error) = &extras.offline {
        Link::Offline(error.clone())
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
        .filter(|mission| !matches!(mission.word, Word::Decision | Word::Done) && !mission.system)
        .count();
    World {
        person: person.to_owned(),
        host,
        link,
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
    }
}

// ------------------------------------------------------------------ attention

fn attention(model: &Model, extras: &Extras) -> Vec<Attention> {
    model
        .attention()
        .map(|item| {
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
                "unread-message" => (
                    Tier::Later,
                    match extras.bodies.get(&item.header.id) {
                        Some((from, title, content)) => AttentionKind::Message {
                            from: from.clone(),
                            body: match title {
                                Some(title) if !title.is_empty() => {
                                    format!(
                                        "**{}**\n\n{}",
                                        clean_message_text(title),
                                        clean_message_text(content)
                                    )
                                }
                                _ => clean_message_text(content),
                            },
                        },
                        None => AttentionKind::Message {
                            from: item
                                .detail
                                .strip_prefix("Unread message from ")
                                .map(|from| from.trim_end_matches('.').to_owned())
                                .unwrap_or_else(|| item.source_id.clone()),
                            body: "Loading the message…".into(),
                        },
                    },
                ),
                _ => (
                    if matches!(item.priority.as_str(), "critical" | "high") {
                        Tier::Alert
                    } else {
                        Tier::Today
                    },
                    AttentionKind::Fault {
                        what: item.detail.clone(),
                        because: match item.priority.as_str() {
                            _ if item.attention_kind == "agent-request" => {
                                "an agent is asking you for help".into()
                            }
                            "critical" => "marked critical".into(),
                            "high" => "marked high priority".into(),
                            _ => "raised for you".into(),
                        },
                        fix: None,
                        source: if item.source_id == item.header.id {
                            String::new()
                        } else {
                            item.source_id.clone()
                        },
                    },
                ),
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
            let raised_by = item
                .extra
                .get("requester_id")
                .or_else(|| item.extra.get("actor"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            Attention {
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
            }
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
                ("failed", _) => AgentState::Fault,
                ("running", Some("working")) => AgentState::Working,
                ("running", _) => AgentState::Idle,
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
        }
    }));
    agents
}

/// Whether an agentless step only holds its run open (so the run's observers keep watching).
/// st does not say whether an agentless step waits on a flag or runs a command, so this goes
/// by the names the fleet uses for keep-open steps; anything else counts as work.
fn keeps_open(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name,
        "keep-watch" | "retire" | "steward-intake" | "standing" | "keep-open"
    ) || name.ends_with("-retirement")
}

/// The agent whose queue holds this work, if st says.
fn queued_for<'a>(model: &'a Model, work: &str) -> Option<&'a st3_client::Agent> {
    model.agents().find(|agent| {
        agent.next_work_id.as_deref() == Some(work)
            || agent.upcoming_work_ids.iter().any(|id| id == work)
    })
}

// ------------------------------------------------------------------- missions

/// Who holds or will take a step: its claimant, else its assignee, named.
fn step_owner(model: &Model, step: &MissionStep) -> Option<String> {
    if step.agentless {
        return Some("st".into());
    }
    let id = step.claimant.as_deref().or(step.assignee.as_deref())?;
    Some(
        model
            .agents()
            .find(|agent| agent.header.id == id)
            .map(|agent| format!("{} · {id}", crate::agent_label(agent)))
            .unwrap_or_else(|| id.to_owned()),
    )
}

fn missions(model: &Model) -> Vec<Mission> {
    model
        .missions()
        .map(|mission| {
            let work = mission_steps(mission);
            let decision = model
                .attention()
                .find(|item| {
                    item.attention_kind == "human-gate"
                        && item.mission_id.as_deref() == Some(mission.header.id.as_str())
                })
                .map(|item| item.header.id.clone());
            let states = work
                .iter()
                .map(|step| step.state.as_str())
                .collect::<Vec<_>>();
            let word = if decision.is_some() {
                Word::Decision
            } else if mission.state == "blocked" || states.contains(&"blocked") {
                Word::Stalled
            } else if states.iter().any(|state| matches!(*state, "failed")) {
                Word::Failed
            } else if !work.is_empty()
                && work.iter().all(|step| {
                    step.state == "completed"
                        || (matches!(step.state.as_str(), "claimed" | "running")
                            && step.agentless
                            && keeps_open(&step.path))
                })
                && work.iter().any(|step| step.state != "completed")
            {
                // Only st's own keep-open steps are running: an intake that watches.
                Word::Watching
            } else if states
                .iter()
                .any(|state| matches!(*state, "claimed" | "running"))
            {
                Word::Working
            } else if let Some(ready) = work
                .iter()
                .find(|step| step.state == "ready" && step.claimant.is_none())
            {
                // Who has this step queued decides whether a person is needed.
                match queued_for(model, &ready.id) {
                    Some(agent)
                        if matches!(agent.state.as_str(), "failed" | "stopped")
                            || agent.fault.is_some() =>
                    {
                        Word::Unstaffed
                    }
                    Some(_) => Word::Queued,
                    None => Word::Unclaimed,
                }
            } else if states.contains(&"waiting") {
                Word::Held
            } else if matches!(
                mission.state.as_str(),
                "standing" | "running" | "ready" | "draft"
            ) {
                Word::Idle
            } else {
                Word::Done
            };
            let steps = work
                .iter()
                .map(|step| {
                    let state = match step.state.as_str() {
                        "completed" => StepState::Done,
                        "claimed" | "running" => StepState::Working,
                        "ready" => StepState::Ready,
                        "waiting" => StepState::Waiting,
                        "blocked" => StepState::Waiting,
                        "failed" => StepState::Failed,
                        _ => StepState::Pending,
                    };
                    let note = if step.state == "ready" && step.claimant.is_none() {
                        queued_for(model, &step.id).map(|agent| {
                            let label = crate::agent_label(agent);
                            match agent.current_work.first() {
                                Some(current)
                                    if !matches!(agent.state.as_str(), "failed" | "stopped") =>
                                {
                                    format!(
                                        "queued for {label}, which is busy with {}",
                                        label_text(model, current)
                                    )
                                }
                                _ => format!("queued for {label}, which is {}", agent.state),
                            }
                        })
                    } else {
                        None
                    };
                    Step {
                        name: step.path.clone(),
                        state,
                        owner: step_owner(model, step),
                        note: note.or_else(|| step.blocked_reason.clone()),
                        after: vec![],
                        age: age(&step.since),
                        goals: step
                            .goals
                            .iter()
                            .map(|goal| clean_message_text(goal))
                            .collect(),
                        constraints: step
                            .constraints
                            .iter()
                            .map(|constraint| clean_message_text(constraint))
                            .collect(),
                        gates: vec![],
                        attempt: step.attempt,
                        blockers: step.blockers.clone(),
                    }
                })
                .collect::<Vec<_>>();
            let agents = model
                .agents()
                .filter(|agent| {
                    agent
                        .current_work_ids
                        .iter()
                        .any(|id| work.iter().any(|step| &step.id == id))
                })
                .map(|agent| agent.header.id.clone())
                .collect();
            Mission {
                id: mission.header.id.clone(),
                title: crate::mission_display_label(mission),
                word,
                age: age(&mission.header.updated_at),
                host: String::new(),
                goals: vec![],
                steps,
                agents,
                kdl: None,
                decision,
                worktree: None,
                parent: None,
                system: mission.header.id.starts_with("mission/__st3/"),
            }
        })
        .collect()
}

// ------------------------------------------------------------------- machines

fn machines(model: &Model) -> Vec<Machine> {
    let gateway = gateway(model).unwrap_or_default();
    model
        .machines()
        .map(|machine| Machine {
            name: machine.name.clone(),
            online: matches!(
                machine.state.as_str(),
                "online" | "reachable" | "local" | "active"
            ),
            platform: machine.state.clone(),
            seen: age(&machine.header.updated_at),
            load: Some(format!(
                "{} running runtimes",
                machine.occupancy.running_runtimes
            )),
            links: machine
                .transports
                .iter()
                .map(|transport| {
                    (
                        transport.protocol.clone(),
                        matches!(
                            transport.status.as_str(),
                            "ok" | "connected" | "reachable" | "healthy"
                        ),
                        transport.status.clone(),
                    )
                })
                .collect(),
            you_are_here: machine.host_id == gateway,
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

/// One conversation: the harness transcript and Small Talk messages, in time order.
/// Draw one conversation as st joined it: the harness's turns and the agent's Small Talk, in
/// the order st sent them.
pub fn conversation(timeline: &[TimelineEntry], names: &BTreeMap<String, String>) -> Vec<Entry> {
    let name = |id: &str| -> String {
        names.get(id).cloned().unwrap_or_else(|| match id {
            "daemon/runtime" => "st".into(),
            id if id.starts_with("person/") => id.trim_start_matches("person/").to_owned(),
            id => short(id),
        })
    };
    let mut stamped: Vec<(String, Entry)> = Vec::new();
    let mut tools: BTreeMap<String, usize> = BTreeMap::new();
    // A Small Talk message is two entries: who wrote to whom, then what they wrote. A
    // harness transcript heads its own turns with message entries too; only a graph message,
    // `message/…`, is Small Talk.
    let mut mail: Option<&st3_client::TimelineMessageBody> = None;
    for entry in timeline {
        let at = clock(&entry.timestamp);
        if let TimelineBody::Message(message) = &entry.body {
            mail = message
                .message_id
                .starts_with("message/")
                .then_some(message);
            continue;
        }
        if let (Some(message), TimelineBody::Content(content)) = (mail.take(), &entry.body) {
            let body = clean_message_text(content.text.as_deref().unwrap_or(""));
            let from = message.from.as_deref().unwrap_or_default();
            let body = if from == "daemon/runtime" {
                // Step-ready pings are graph events, not conversation.
                Body::Event(
                    message
                        .title
                        .clone()
                        .unwrap_or_else(|| body.lines().next().unwrap_or("").to_owned()),
                )
            } else {
                // An older st sends neither side; say what it is rather than draw a blank.
                let (from, to) = match (from, message.to.as_deref()) {
                    ("", None) => ("Small Talk".to_owned(), String::new()),
                    (from, to) => (name(from), name(to.unwrap_or_default())),
                };
                Body::Mail {
                    from,
                    to,
                    subject: message.title.clone().unwrap_or_default(),
                    body: if body.is_empty() {
                        "(notification)".into()
                    } else {
                        body
                    },
                }
            };
            stamped.push((
                entry.timestamp.clone(),
                Entry {
                    id: message.message_id.clone(),
                    at,
                    body,
                },
            ));
            continue;
        }
        let body = match (&entry.role, &entry.body) {
            (TimelineRole::User | TimelineRole::System, TimelineBody::Content(content)) => {
                // Harness markup becomes what it means; context blocks disappear.
                let bodies = from_harness(
                    entry.role == TimelineRole::User,
                    content.text.as_deref().unwrap_or(""),
                );
                for (index, body) in bodies.into_iter().enumerate() {
                    stamped.push((
                        entry.timestamp.clone(),
                        Entry {
                            id: format!("{}#{index}", entry.id),
                            at: at.clone(),
                            body,
                        },
                    ));
                }
                continue;
            }
            (TimelineRole::Assistant, TimelineBody::Content(content)) => {
                let text = clean_message_text(content.text.as_deref().unwrap_or(""));
                if text.is_empty() {
                    continue;
                }
                Body::Assistant(text)
            }
            (TimelineRole::Tool, TimelineBody::Content(content)) => {
                let text = clean_message_text(content.text.as_deref().unwrap_or(""));
                if text.is_empty() {
                    continue;
                }
                let mut lines = text.lines().map(str::to_owned);
                Body::Tool {
                    title: lines.next().unwrap_or_default(),
                    state: ToolState::Ok,
                    output: lines.collect(),
                }
            }
            (_, TimelineBody::ToolCall(call)) => {
                tools.insert(call.call_id.clone(), stamped.len());
                Body::Tool {
                    title: tool_title(&call.name, &call.arguments),
                    state: ToolState::Running,
                    output: vec![],
                }
            }
            (_, TimelineBody::ToolResult(result)) => {
                let output = tool_output(&result.content);
                let state = match result.status {
                    TimelineToolStatus::Error => ToolState::Failed,
                    _ => ToolState::Ok,
                };
                if let Some(index) = tools.get(&result.call_id).copied()
                    && let Some((
                        _,
                        Entry {
                            body:
                                Body::Tool {
                                    state: slot,
                                    output: out,
                                    ..
                                },
                            ..
                        },
                    )) = stamped.get_mut(index)
                {
                    *slot = state;
                    *out = output;
                    continue;
                }
                Body::Tool {
                    title: "tool result".into(),
                    state,
                    output,
                }
            }
            (_, TimelineBody::Error(error)) => Body::Event(format!("error: {}", error.message)),
            _ => continue,
        };
        stamped.push((
            entry.timestamp.clone(),
            Entry {
                id: entry.id.clone(),
                at,
                body,
            },
        ));
    }
    stamped.sort_by(|a, b| a.0.cmp(&b.0));
    stamped.into_iter().map(|(_, entry)| entry).collect()
}

/// Blocks harnesses add to a transcript for the model's benefit. None of it is conversation.
const CONTEXT_BLOCKS: &[&str] = &[
    "system-reminder",
    "local-command-caveat",
    "environment_context",
    "permissions",
    "collaboration_mode",
    "multi_agent_mode",
    "apps_instructions",
    "plugins_instructions",
    "skills_instructions",
    "user_instructions",
    "developer_instructions",
    "command-message",
    "command-args",
];

/// Take every `<tag …>…</tag>` block out of `text`, returning the inner texts. A block that
/// never closes runs to the end, so a truncated wrapper cannot leak either.
fn take_blocks(text: &mut String, tag: &str) -> Vec<String> {
    let mut found = Vec::new();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut from = 0;
    while let Some(offset) = text[from..].find(&open) {
        let start = from + offset;
        let after = &text[start + open.len()..];
        // `<permissions` must not match `<permissionsfoo`.
        if !after.starts_with(['>', ' ', '/', '\n']) {
            from = start + open.len();
            continue;
        }
        let Some(head_end) = after.find('>').map(|index| start + open.len() + index + 1) else {
            text.truncate(start);
            break;
        };
        let (inner, end) = match text[head_end..].find(&close) {
            Some(index) => (
                text[head_end..head_end + index].to_owned(),
                head_end + index + close.len(),
            ),
            None => (text[head_end..].to_owned(), text.len()),
        };
        found.push(inner);
        text.replace_range(start..end, "");
        from = start;
    }
    found
}

fn field(block: &str, tag: &str) -> Option<String> {
    let mut copy = block.to_owned();
    take_blocks(&mut copy, tag)
        .into_iter()
        .next()
        .map(|value| value.trim().to_owned())
}

fn shorten(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        line.to_owned()
    } else {
        format!("{}…", line.chars().take(max).collect::<String>())
    }
}

/// A user or system entry from a harness transcript, turned into what a person should see.
pub fn from_harness(is_user: bool, raw: &str) -> Vec<Body> {
    let mut text = raw.replace("\r\n", "\n");
    let mut bodies = Vec::new();
    for block in take_blocks(&mut text, "task-notification") {
        let status = field(&block, "status").unwrap_or_else(|| "update".into());
        let summary = field(&block, "summary").unwrap_or_default();
        bodies.push(Body::Event(format!(
            "background task {status}: {}",
            shorten(&summary, 90)
        )));
    }
    // st and st2 deliveries: the message itself is in the stream as mail.
    let mut deliveries = Vec::new();
    let mut from = 0;
    while let Some(offset) = text[from..].find("<channel") {
        let start = from + offset;
        let head_end = text[start..]
            .find('>')
            .map(|index| start + index + 1)
            .unwrap_or(text.len());
        let head = text[start..head_end].to_owned();
        let sender = head
            .split("from=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or("someone")
            .to_owned();
        deliveries.push(sender);
        from = start + 1;
    }
    for (block, sender) in take_blocks(&mut text, "channel")
        .into_iter()
        .zip(deliveries)
    {
        let subject = block
            .lines()
            .find_map(|line| line.trim().strip_prefix("Subject:").map(str::trim))
            .map(str::to_owned)
            .unwrap_or_else(|| shorten(&clean_message_text(&block), 70));
        bodies.push(Body::Event(format!(
            "delivered to the agent: {} · from {sender}",
            shorten(&subject, 80)
        )));
    }
    for command in take_blocks(&mut text, "command-name") {
        let args = field(raw, "command-args").unwrap_or_default();
        bodies.push(Body::User(
            format!("{} {}", command.trim(), args).trim().to_owned(),
        ));
    }
    for (tag, state) in [
        ("local-command-stdout", ToolState::Ok),
        ("local-command-stderr", ToolState::Failed),
    ] {
        for output in take_blocks(&mut text, tag) {
            bodies.push(Body::Tool {
                title: "command output".into(),
                state,
                output: output.trim().lines().map(str::to_owned).collect(),
            });
        }
    }
    for _ in take_blocks(&mut text, "turn_aborted") {
        bodies.push(Body::Event("the turn was interrupted".into()));
    }
    for reply in take_blocks(&mut text, "send_user_message_question_reply") {
        let answers = serde_json::from_str::<Value>(reply.trim())
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|item| {
                item.get("answer")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        if !answers.is_empty() {
            bodies.push(Body::User(answers.join("\n")));
        }
    }
    for tag in CONTEXT_BLOCKS {
        take_blocks(&mut text, tag);
    }
    let rest = clean_message_text(&text);
    if is_user && !rest.is_empty() {
        bodies.insert(0, Body::User(rest));
    }
    bodies
}

fn clock(timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn tool_title(name: &str, arguments: &Value) -> String {
    let pick = [
        "command",
        "cmd",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "description",
    ]
    .iter()
    .find_map(|key| arguments.get(key).and_then(Value::as_str));
    match (name, pick) {
        ("Bash" | "bash" | "shell" | "exec_command", Some(command)) => {
            format!("$ {}", command.lines().next().unwrap_or(command))
        }
        (_, Some(detail)) => format!("{name} {}", detail.lines().next().unwrap_or(detail)),
        _ => name.to_owned(),
    }
}

fn tool_output(content: &Value) -> Vec<String> {
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str).or(item.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| other.to_string()),
    };
    text.lines().take(400).map(str::to_owned).collect()
}

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
             "body":{"message_id":"message/one","from":"agent/fleet/harbor","to":"agent/fleet/cos","title":"A question"}},
            {"id":"e2","sequence":5,"revision":1,"timestamp":"2026-09-29T10:00:00Z","role":"user","type":"content","final":true,
             "body":{"media_type":"text/plain","text":"Can you look?"}},
            {"id":"e3","sequence":8,"revision":1,"timestamp":"2026-09-29T10:01:00Z","role":"user","type":"message","final":true,
             "body":{"message_id":"message/two","from":"daemon/runtime","to":"agent/fleet/cos","title":"Mission step ready: review"}},
            {"id":"e4","sequence":9,"revision":1,"timestamp":"2026-09-29T10:01:00Z","role":"user","type":"content","final":true,
             "body":{"media_type":"text/plain","text":"A mission step is ready."}},
            {"id":"e5a","sequence":11,"revision":1,"timestamp":"2026-09-29T10:02:00Z","role":"assistant","type":"message","final":true,
             "body":{"message_id":"msg_harness_turn"}},
            {"id":"e5","sequence":12,"revision":1,"timestamp":"2026-09-29T10:02:00Z","role":"assistant","type":"content","final":true,
             "body":{"media_type":"text/plain","text":"On it."}}
        ]))
        .unwrap();
        let names = BTreeMap::from([("agent/fleet/cos".to_owned(), "COS".to_owned())]);
        let entries = conversation(&timeline, &names);
        assert_eq!(entries.len(), 3, "{entries:#?}");
        match &entries[0].body {
            Body::Mail {
                from,
                to,
                subject,
                body,
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

    fn rendered(fixture: &str) -> String {
        let timeline: Vec<TimelineEntry> = serde_json::from_str(fixture).unwrap();
        let entries = conversation(&timeline, &BTreeMap::new());
        let doc = super::super::conversation::Cache::default().render(
            &entries,
            100,
            &Default::default(),
            "⠋",
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
        let bodies = from_harness(true, "hello\n<system-reminder>\nnever closed");
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
                "scan": {"path": "scan", "work_selector": {"kind": "assigned", "agent": "agent/fleet/auditor"}, "dependencies": [], "gates": []},
                "merge": {"path": "merge", "work_selector": {"kind": "agentless"}, "dependencies": [{"dependency": "step", "step": "scan", "state": "completed"}], "gates": [{"reviewer": "person/robin"}]}
            }
        });
        let preview = preview("harbor/audit", &normalized);
        assert_eq!(preview.goals, vec!["Audit dependencies"]);
        assert_eq!(preview.steps[0].assignee, "fleet/auditor");
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
    fn missions_and_agents_read_what_st_joined_without_work_or_runtime_lists() {
        let step = |id: &str, path: &str, state: &str, claimant: Option<&str>| {
            serde_json::json!({
                "id": id, "path": path, "state": state, "attempt": 1,
                "assignee": "agent/fleet/harbor/keeper", "claimant": claimant,
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
                    step("step-run/audit-1/scan", "scan", "claimed", Some("agent/fleet/harbor/keeper")),
                    step("step-run/audit-1/report", "report", "ready", None),
                ]),
            ],
        })]);
        model.agents = window(vec![serde_json::json!({
            "id": "agent/fleet/harbor/keeper", "kind": "agent", "revision": "r2",
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

        let Load::Ready(missions) = &world.missions else {
            panic!("missions load from the missions window alone")
        };
        let mission = &missions[0];
        assert_eq!(mission.word, Word::Working);
        assert_eq!(
            mission
                .steps
                .iter()
                .map(|step| (step.name.as_str(), step.state))
                .collect::<Vec<_>>(),
            [("scan", StepState::Working), ("report", StepState::Ready)]
        );
        assert_eq!(mission.steps[0].goals, ["Do scan."]);
        assert_eq!(mission.agents, ["agent/fleet/harbor/keeper"]);
        assert_eq!(
            mission.steps[1].note.as_deref(),
            Some("queued for Keeper, which is busy with fleet/harbor · Audit › scan")
        );

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
