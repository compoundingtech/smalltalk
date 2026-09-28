//! Live st3 data, turned into the view model the screens draw.
//!
//! A collection with no snapshot has not loaded yet, so it becomes `Load::Loading`, never an
//! empty list. Anything the graph does not say stays unsaid.

use super::view::*;
use crate::model::{Model, clean_message_text};
use serde_json::Value;
use st3_client::{TimelineBody, TimelineEntry, TimelineRole, TimelineToolStatus};
use std::collections::BTreeMap;

/// What the live loop has fetched beside the model: conversations and launch previews.
#[derive(Default)]
pub struct Extras {
    pub conversations: BTreeMap<String, Load<Vec<Entry>>>,
    pub previews: BTreeMap<String, Load<MissionPreview>>,
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
    let host = model
        .sessions
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.host_id.trim_start_matches("host/").to_owned())
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
        missions: loaded(
            model.missions.snapshot.is_some() && model.work.snapshot.is_some(),
            missions,
        ),
        machines: loaded(model.machines.snapshot.is_some(), machines(model)),
        worktrees: Load::Ready(super::demo::world().worktrees.items().to_vec()),
        conversations: extras.conversations.clone(),
        quiet_missions: quiet,
    }
}

// ------------------------------------------------------------------ attention

fn attention(model: &Model, extras: &Extras) -> Vec<Attention> {
    model
        .attention()
        .map(|item| {
            let step = item.step_run_id.as_ref().and_then(|id| {
                model
                    .work()
                    .find(|work| &work.header.id == id)
                    .map(|work| work.path.clone())
            });
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
                    AttentionKind::Message {
                        from: item
                            .detail
                            .strip_prefix("Unread message from ")
                            .map(|from| from.trim_end_matches('.').to_owned())
                            .unwrap_or_else(|| item.source_id.clone()),
                        body: item.title.clone(),
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
                .and_then(|id| model.work().find(|work| &work.header.id == id))
                .and_then(|work| work.claimant.clone())
                .or_else(|| {
                    // The agents working in the mission, for a gate that has no claimant.
                    item.mission_id
                        .as_ref()
                        .and_then(|mission| {
                            model
                                .missions()
                                .find(|candidate| &candidate.header.id == mission)
                                .and_then(|mission| {
                                    model.agents().find(|agent| {
                                        agent.current_work_ids.iter().any(|id| {
                                            model.work().any(|work| {
                                                &work.header.id == id
                                                    && crate::mission_work_matches(mission, work)
                                            })
                                        })
                                    })
                                })
                        })
                        .map(|agent| agent.header.id.clone())
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
                title: clean_message_text(&item.title),
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
            let host = model
                .runtimes()
                .find(|runtime| runtime.owner_id == agent.header.id)
                .map(|runtime| runtime.owner_host_id.trim_start_matches("host/").to_owned())
                .unwrap_or_else(|| "?".into());
            let work = agent
                .current_work_ids
                .first()
                .and_then(|id| model.work().find(|work| &work.header.id == id));
            let mission = work.and_then(|work| {
                model
                    .missions()
                    .find(|mission| mission.runs.contains(&work.mission_run_id))
                    .map(|mission| mission.header.id.clone())
            });
            Agent {
                id: agent.header.id.clone(),
                name: crate::agent_label(agent),
                harness: harness(agent.driver.as_deref()),
                state,
                host,
                worktree: None,
                mission,
                step: work.map(|work| work.path.clone()),
                activity: age(&agent.header.updated_at),
                unmanaged: false,
                parent: agent
                    .under
                    .first()
                    .map(|relation| relation.agent_id.clone()),
                details: AgentDetails {
                    goal: work
                        .and_then(|work| work.goals.first().map(|goal| clean_message_text(goal))),
                    claimed: work.map(|work| format!("{} ago", age(&work.header.updated_at))),
                    next: agent.next_work_id.as_ref().map(|id| step_label(model, id)),
                    queue: agent
                        .upcoming_work_ids
                        .iter()
                        .map(|id| step_label(model, id))
                        .collect(),
                    queued: agent.queued_work_count,
                    harness_state: agent.harness_state.clone(),
                    runtime: model
                        .runtimes()
                        .find(|runtime| runtime.owner_id == agent.header.id)
                        .map(|runtime| runtime.state.clone()),
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
    let gateway = model
        .sessions
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.host_id.trim_start_matches("host/").to_owned())
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

/// "Mission › step" for a work id, or the id itself when st has not sent that work.
fn step_label(model: &Model, id: &str) -> String {
    model
        .work()
        .find(|work| work.header.id == id)
        .map(|work| {
            let mission = model
                .missions()
                .find(|mission| mission.runs.contains(&work.mission_run_id))
                .map(crate::mission_display_label)
                .unwrap_or_else(|| {
                    work.mission_run_id
                        .trim_start_matches("mission-run/")
                        .to_owned()
                });
            format!("{mission} › {}", work.path)
        })
        .unwrap_or_else(|| id.trim_start_matches("step-run/").to_owned())
}

// ------------------------------------------------------------------- missions

fn missions(model: &Model) -> Vec<Mission> {
    model
        .missions()
        .map(|mission| {
            let work = model
                .work()
                .filter(|work| crate::mission_work_matches(mission, work))
                .collect::<Vec<_>>();
            let decision = model
                .attention()
                .find(|item| {
                    item.attention_kind == "human-gate"
                        && item.mission_id.as_deref() == Some(mission.header.id.as_str())
                })
                .map(|item| item.header.id.clone());
            let states = work
                .iter()
                .map(|work| work.state.as_str())
                .collect::<Vec<_>>();
            let word = if decision.is_some() {
                Word::Decision
            } else if mission.state == "blocked" || states.contains(&"blocked") {
                Word::Stalled
            } else if states.iter().any(|state| matches!(*state, "failed")) {
                Word::Failed
            } else if !work.is_empty()
                && work.iter().all(|work| {
                    work.state == "completed"
                        || (matches!(work.state.as_str(), "claimed" | "running")
                            && crate::work_owner(model, work) == "Agentless step"
                            && keeps_open(&work.path))
                })
                && work.iter().any(|work| work.state != "completed")
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
                .find(|work| work.state == "ready" && work.claimant.is_none())
            {
                // Who has this step queued decides whether a person is needed.
                match queued_for(model, &ready.header.id) {
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
                .map(|work| {
                    let state = match work.state.as_str() {
                        "completed" => StepState::Done,
                        "claimed" | "running" => StepState::Working,
                        "ready" => StepState::Ready,
                        "waiting" => StepState::Waiting,
                        "blocked" => StepState::Waiting,
                        "failed" => StepState::Failed,
                        _ => StepState::Pending,
                    };
                    let owner = crate::work_owner(model, work);
                    let note = if work.state == "ready" && work.claimant.is_none() {
                        queued_for(model, &work.header.id).map(|agent| {
                            let label = crate::agent_label(agent);
                            match agent.current_work_ids.first() {
                                Some(current)
                                    if !matches!(agent.state.as_str(), "failed" | "stopped") =>
                                {
                                    format!(
                                        "queued for {label}, which is busy with {}",
                                        step_label(model, current)
                                    )
                                }
                                _ => format!("queued for {label}, which is {}", agent.state),
                            }
                        })
                    } else {
                        None
                    };
                    Step {
                        name: work.path.clone(),
                        state,
                        owner: match owner.as_str() {
                            "" | "unassigned" => None,
                            "Agentless step" => Some("st".into()),
                            _ => Some(owner),
                        },
                        note: note.or_else(|| work.blocked_reason.clone()),
                        after: vec![],
                        age: age(&work.header.updated_at),
                        goals: work
                            .goals
                            .iter()
                            .map(|goal| clean_message_text(goal))
                            .collect(),
                        constraints: work
                            .constraints
                            .iter()
                            .map(|constraint| clean_message_text(constraint))
                            .collect(),
                        gates: vec![],
                        attempt: work.attempt,
                        blockers: work.blockers.clone(),
                    }
                })
                .collect::<Vec<_>>();
            let agents = model
                .agents()
                .filter(|agent| {
                    agent
                        .current_work_ids
                        .iter()
                        .any(|id| work.iter().any(|work| &work.header.id == id))
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
    let gateway = model
        .sessions
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.host_id.clone())
        .unwrap_or_default();
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
pub fn conversation(
    timeline: &[TimelineEntry],
    messages: &[st3_client::Message],
    names: &BTreeMap<String, String>,
) -> Vec<Entry> {
    let name = |id: &str| -> String {
        names.get(id).cloned().unwrap_or_else(|| match id {
            "daemon/runtime" => "st".into(),
            id if id.starts_with("person/") => id.trim_start_matches("person/").to_owned(),
            id => short(id),
        })
    };
    let mut stamped: Vec<(String, Entry)> = Vec::new();
    let mut tools: BTreeMap<String, usize> = BTreeMap::new();
    for entry in timeline {
        let at = clock(&entry.timestamp);
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
    for message in messages {
        let body = clean_message_text(&message.content);
        if message.from == "daemon/runtime" {
            // Step-ready pings are graph events, not conversation.
            let title = message
                .title
                .clone()
                .unwrap_or_else(|| body.lines().next().unwrap_or("").to_owned());
            stamped.push((
                message.sent_at.clone(),
                Entry {
                    id: message.header.id.clone(),
                    at: clock(&message.sent_at),
                    body: Body::Event(title),
                },
            ));
            continue;
        }
        stamped.push((
            message.sent_at.clone(),
            Entry {
                id: message.header.id.clone(),
                at: clock(&message.sent_at),
                body: Body::Mail {
                    from: name(&message.from),
                    to: name(&message.to),
                    subject: message.title.clone().unwrap_or_default(),
                    body: if body.is_empty() {
                        "(notification)".into()
                    } else {
                        body
                    },
                },
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

    fn rendered(fixture: &str) -> String {
        let timeline: Vec<TimelineEntry> = serde_json::from_str(fixture).unwrap();
        let entries = conversation(&timeline, &[], &BTreeMap::new());
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
        let text = rendered(include_str!("../../tests/fixtures/transcripts/claude.json"));
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
        let text = rendered(include_str!("../../tests/fixtures/transcripts/codex.json"));
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
        let entries = conversation(&timeline, &[], &BTreeMap::new());
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
}
