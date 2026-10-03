//! Runtime shell completion and short-name resolution for entity arguments.
//!
//! Contract: docs/st3/cli-completion/spec.md. The shell stub calls back into `st` on every TAB
//! through clap_complete's `CompleteEnv`; each entity argument carries a [`Complete`] completer
//! that lists the daemon's current entities of one [`Entity`] kind. Completion never waits for
//! the daemon and never prints: any failure yields no candidates.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Duration;

use clap_complete::engine::{CompletionCandidate, ValueCompleter};
use st3_client::{Client, ClientError, Resource};

/// All daemon calls of one completion request, joins included, finish within this deadline.
pub const DEADLINE: Duration = Duration::from_millis(300);

/// Short-name resolution before a command runs may wait longer than a TAB; past it the word
/// keeps its literal meaning.
pub const RESOLVE_DEADLINE: Duration = Duration::from_secs(2);

const LIST_LIMIT: usize = 200;
const DESCRIPTION_WIDTH: usize = 96;

/// The entity kind an argument names, with the filter its command accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entity {
    /// Running terminal members, named by their owner subject (`agent/...`).
    Terminal,
    /// Current agents; `running_only` keeps only running seats.
    Agent { running_only: bool },
    /// Current missions.
    Mission,
    /// Mission runs of current missions; `unfinished_only` drops runs in the terminal phase.
    MissionRun { unfinished_only: bool },
    /// Current missions and their runs, for commands that accept either.
    MissionOrRun,
    /// Current mission steps; `state` keeps only steps in that state, such as `ready`.
    Work { state: Option<&'static str> },
    /// Open attention items.
    Attention,
    /// Unarchived messages in the caller's mailbox.
    Message,
    /// Open lanes.
    Lane,
    /// Fleet hosts.
    Host,
}

/// One entity a person can pick: its exact subject and a one-line description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub subject: String,
    pub description: String,
}

/// clap completer for one entity argument.
#[derive(Clone, Copy, Debug)]
pub struct Complete(pub Entity);

impl ValueCompleter for Complete {
    fn complete(&self, current: &OsStr) -> Vec<CompletionCandidate> {
        let Some(prefix) = current.to_str() else {
            return Vec::new();
        };
        fetch_blocking(self.0)
            .unwrap_or_default()
            .into_iter()
            .filter(|candidate| candidate.subject.starts_with(prefix))
            .map(|candidate| {
                CompletionCandidate::new(candidate.subject).help(Some(candidate.description.into()))
            })
            .collect()
    }
}

/// Lists candidates on a private current-thread runtime, bounded by [`DEADLINE`].
fn fetch_blocking(entity: Entity) -> Option<Vec<Candidate>> {
    let target = LocalTarget::discover()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime
        .block_on(async {
            tokio::time::timeout(
                DEADLINE,
                candidates(&target.client(), entity, target.caller.as_deref()),
            )
            .await
        })
        .ok()?
        .ok()
}

/// The trusted local endpoint and caller identity, resolved the way ordinary commands do.
pub struct LocalTarget {
    pub socket: PathBuf,
    /// `ST_AGENT` in a seat, otherwise the configured person.
    pub caller: Option<String>,
}

impl LocalTarget {
    pub fn discover() -> Option<Self> {
        let config = st3::config::Config::load_unvalidated(None).ok()?;
        let socket = match std::env::var("ST3_ENDPOINT")
            .ok()
            .map(st3::client::Endpoint::parse)
        {
            Some(st3::client::Endpoint::Unix(socket)) => socket,
            Some(_) => return None,
            None => config.socket,
        };
        let caller = std::env::var("ST_AGENT")
            .ok()
            .filter(|agent| !agent.is_empty())
            .map(|agent| {
                if agent.starts_with("agent/") {
                    agent
                } else {
                    format!("agent/{agent}")
                }
            })
            .or(config.person);
        Some(Self { socket, caller })
    }

    /// A client that fails at once while the daemon is unreachable.
    pub fn client(&self) -> Client {
        Client::unix(&self.socket).with_outage_wait(Duration::ZERO, false)
    }
}

/// Current entities of one kind, filtered for the command, with curated descriptions.
pub async fn candidates(
    client: &Client,
    entity: Entity,
    caller: Option<&str>,
) -> Result<Vec<Candidate>, ClientError> {
    let now = chrono::Utc::now();
    let items = |page: st3_client::Envelope<st3_client::Page>| page.value.items;
    let found = match entity {
        Entity::Terminal => {
            let (terminals, agents) = tokio::join!(
                client.terminals_list(None, Some(LIST_LIMIT), false),
                client.agents_list(None, Some(LIST_LIMIT), false),
            );
            // The agent join adds harness and activity; a terminal still completes without it.
            let agents = agents.map(items).unwrap_or_default();
            items(terminals?)
                .into_iter()
                .filter_map(|item| match item {
                    Resource::Runtime(runtime) if runtime.state == "running" => {
                        let agent = agents.iter().find_map(|agent| match agent {
                            Resource::Agent(agent) if agent.header.id == runtime.owner_id => {
                                Some(agent)
                            }
                            _ => None,
                        });
                        let mut parts = vec![
                            runtime.state.clone(),
                            short_host(&runtime.owner_host_id).to_owned(),
                        ];
                        if let Some(agent) = agent {
                            parts.extend(agent.driver.clone());
                            parts.extend(agent.harness_state.clone());
                            parts.extend(agent_work(agent));
                        }
                        parts.extend(
                            since(&runtime.header.updated_at, now).map(|age| format!("up {age}")),
                        );
                        Some(Candidate::new(runtime.owner_id, parts))
                    }
                    _ => None,
                })
                .collect()
        }
        Entity::Agent { running_only } => {
            items(client.agents_list(None, Some(LIST_LIMIT), false).await?)
                .into_iter()
                .filter_map(|item| match item {
                    Resource::Agent(agent) if !running_only || agent.state == "running" => {
                        let mut parts = vec![agent.state.clone()];
                        parts.extend(agent.host_id.as_deref().map(short_host).map(str::to_owned));
                        parts.extend(agent.driver.clone());
                        parts.extend(agent.harness_state.clone());
                        parts.extend(agent_work(&agent));
                        Some(Candidate::new(agent.header.id, parts))
                    }
                    _ => None,
                })
                .collect()
        }
        Entity::Mission | Entity::MissionRun { .. } | Entity::MissionOrRun => {
            let missions = items(client.missions_list(None, Some(LIST_LIMIT), false).await?);
            let mut found = Vec::new();
            for item in missions {
                let Resource::Mission(mission) = item else {
                    continue;
                };
                if matches!(entity, Entity::Mission | Entity::MissionOrRun) {
                    let mut parts = vec![mission.title.clone(), mission.state.clone()];
                    if let Some(active) = mission.active_runs.filter(|active| *active > 0) {
                        parts.push(plural(active, "active run"));
                    }
                    found.push(Candidate::new(mission.header.id.clone(), parts));
                }
                if matches!(entity, Entity::Mission) {
                    continue;
                }
                let unfinished_only = matches!(
                    entity,
                    Entity::MissionRun {
                        unfinished_only: true
                    }
                );
                for run in &mission.run_details {
                    if unfinished_only && (run.outcome.is_some() || run.phase == "terminal") {
                        continue;
                    }
                    let mut parts =
                        vec![mission.title.clone(), run.status.clone(), run.phase.clone()];
                    parts.extend(
                        run.current_steps
                            .first()
                            .and_then(|step| step.get("path").and_then(|path| path.as_str()))
                            .map(|path| format!("step {path}")),
                    );
                    parts.extend(since(&run.state_since, now));
                    found.push(Candidate::new(run.id.clone(), parts));
                }
            }
            found
        }
        Entity::Work { state } => items(client.work_list(None, Some(LIST_LIMIT), false).await?)
            .into_iter()
            .filter_map(|item| match item {
                Resource::Work(work) if state.is_none_or(|state| work.state == state) => {
                    let mut parts = vec![
                        work.title.clone().unwrap_or_else(|| work.path.clone()),
                        work.state.clone(),
                    ];
                    parts.push(
                        work.mission_id
                            .clone()
                            .unwrap_or(work.mission_run_id.clone()),
                    );
                    parts.extend(work.claimant.clone().or(work.assigned_to.clone()));
                    Some(Candidate::new(work.header.id, parts))
                }
                _ => None,
            })
            .collect(),
        Entity::Attention => items(client.attention_list(None, Some(LIST_LIMIT), false).await?)
            .into_iter()
            .filter_map(|item| match item {
                Resource::Attention(attention) if attention.state == "open" => {
                    let mut parts = vec![quote(&attention.title), attention.priority.clone()];
                    parts.extend(attention.requester_id.clone());
                    parts.extend(since(&attention.requested_at, now));
                    Some(Candidate::new(attention.header.id, parts))
                }
                _ => None,
            })
            .collect(),
        Entity::Message => {
            let Some(caller) = caller else {
                return Ok(Vec::new());
            };
            items(
                client
                    .messages_list_for_recipient(caller, None, Some(LIST_LIMIT), false)
                    .await?,
            )
            .into_iter()
            .filter_map(|item| match item {
                Resource::Message(message) if message.state != "archived" => {
                    let title = message.title.clone().unwrap_or_else(|| {
                        message
                            .content
                            .lines()
                            .next()
                            .unwrap_or_default()
                            .to_owned()
                    });
                    let mut parts = vec![format!("from {}", message.from), quote(&title)];
                    parts.extend(since(&message.sent_at, now));
                    Some(Candidate::new(message.header.id, parts))
                }
                _ => None,
            })
            .collect()
        }
        Entity::Lane => items(client.lanes_list(None, Some(LIST_LIMIT), false).await?)
            .into_iter()
            .filter_map(|item| match item {
                Resource::Lane(lane) => {
                    let parts = vec![
                        lane.name.clone(),
                        lane.state.clone(),
                        plural(lane.entries.len(), "entry"),
                    ];
                    Some(Candidate::new(lane.header.id, parts))
                }
                _ => None,
            })
            .collect(),
        Entity::Host => items(client.machines_list(None, Some(LIST_LIMIT), false).await?)
            .into_iter()
            .filter_map(|item| match item {
                Resource::Machine(machine) => {
                    let parts = vec![
                        machine.state.clone(),
                        plural(machine.runtime_ids.len(), "runtime"),
                    ];
                    Some(Candidate::new(machine.host_id, parts))
                }
                _ => None,
            })
            .collect(),
    };
    Ok(found)
}

impl Candidate {
    fn new(subject: String, parts: Vec<String>) -> Self {
        let description = parts
            .into_iter()
            .map(|part| part.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        Self {
            subject,
            description: truncate(&description, DESCRIPTION_WIDTH),
        }
    }
}

fn agent_work(agent: &st3_client::Agent) -> Option<String> {
    agent
        .current_work
        .first()
        .map(|work| format!("on {}", work.title.as_deref().unwrap_or(&work.path)))
        .or_else(|| {
            (agent.queued_work_count > 0)
                .then(|| plural(agent.queued_work_count as usize, "queued run"))
        })
}

fn short_host(host: &str) -> &str {
    host.strip_prefix("host/").unwrap_or(host)
}

fn plural(count: usize, noun: &str) -> String {
    match (count, noun.strip_suffix('y')) {
        (1, _) => format!("1 {noun}"),
        (_, Some(stem)) => format!("{count} {stem}ies"),
        _ => format!("{count} {noun}s"),
    }
}

fn quote(text: &str) -> String {
    format!("\"{}\"", truncate(text.trim(), 48))
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(width - 1).collect();
    short.push('…');
    short
}

/// Compact age of an RFC 3339 timestamp, such as `3m` or `2h`.
fn since(timestamp: &str, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let at = chrono::DateTime::parse_from_rfc3339(timestamp).ok()?;
    let seconds = (now - at.with_timezone(&chrono::Utc)).num_seconds().max(0);
    Some(match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    })
}

/// How a bare word on the command line resolved to one subject.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Use this exact subject.
    Subject(String),
    /// The word matches several subjects at the first step that matched anything.
    Ambiguous(Vec<Candidate>),
    /// Nothing matched; the caller keeps its literal interpretation.
    Unmatched,
}

/// Resolves a bare word against a command's candidates (spec: short-name ladder).
///
/// Input containing `/` is a full subject and is never resolved. Steps, first match wins:
/// exact `<namespace>/<word>`, exact last path segment, unique last-segment prefix, unique
/// substring of the subject. A step with several matches is ambiguous.
pub fn resolve(word: &str, namespace: &str, candidates: &[Candidate]) -> Resolution {
    if word.contains('/') {
        return Resolution::Subject(word.to_owned());
    }
    let literal = format!("{namespace}/{word}");
    let last = |candidate: &Candidate| {
        candidate
            .subject
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    let steps: [&dyn Fn(&Candidate) -> bool; 4] = [
        &|candidate| candidate.subject == literal,
        &|candidate| last(candidate) == word,
        &|candidate| last(candidate).starts_with(word),
        &|candidate| candidate.subject.contains(word),
    ];
    for step in steps {
        let matched: Vec<Candidate> = candidates
            .iter()
            .filter(|candidate| step(candidate))
            .cloned()
            .collect();
        match matched.len() {
            0 => continue,
            1 => return Resolution::Subject(matched[0].subject.clone()),
            _ => return Resolution::Ambiguous(matched),
        }
    }
    Resolution::Unmatched
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(subject: &str) -> Candidate {
        Candidate {
            subject: subject.into(),
            description: String::new(),
        }
    }

    fn fleet() -> Vec<Candidate> {
        [
            "agent/dotfiles/steward",
            "agent/dev3.eu-ci-bottleneck",
            "agent/interactive/dev3/a13d6263-3bb5-4b",
            "agent/interactive/dev3/43b16cd2-ec69-44",
        ]
        .map(candidate)
        .to_vec()
    }

    #[test]
    fn resolution_ladder() {
        let fleet = fleet();
        let subject = |word| match resolve(word, "agent", &fleet) {
            Resolution::Subject(subject) => subject,
            other => panic!("{word}: {other:?}"),
        };
        assert_eq!(subject("steward"), "agent/dotfiles/steward");
        assert_eq!(subject("a13d"), "agent/interactive/dev3/a13d6263-3bb5-4b");
        assert_eq!(subject("eu-ci"), "agent/dev3.eu-ci-bottleneck");
        assert_eq!(subject("agent/other"), "agent/other");
        // Step 3 wins before step 4: `dev3` prefixes one last segment.
        assert_eq!(subject("dev3"), "agent/dev3.eu-ci-bottleneck");
        let Resolution::Ambiguous(matches) = resolve("interactive", "agent", &fleet) else {
            panic!("interactive names two subjects");
        };
        assert_eq!(matches.len(), 2);
        assert_eq!(resolve("nothing", "agent", &fleet), Resolution::Unmatched);
    }

    #[test]
    fn exact_namespace_subject_wins_over_segments() {
        let fleet = vec![
            candidate("pty/steward"),
            candidate("agent/dotfiles/steward"),
        ];
        assert_eq!(
            resolve("steward", "pty", &fleet),
            Resolution::Subject("pty/steward".into())
        );
    }

    #[test]
    fn descriptions_are_one_bounded_line() {
        let candidate = Candidate::new(
            "agent/x".into(),
            vec![
                "running".into(),
                String::new(),
                "two\nlines".into(),
                "x".repeat(200),
            ],
        );
        assert!(candidate.description.starts_with("running · two lines · x"));
        assert_eq!(candidate.description.chars().count(), DESCRIPTION_WIDTH);
        assert!(candidate.description.ends_with('…'));
    }

    #[test]
    fn ages_and_plurals() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-30T12:00:00Z")
            .unwrap()
            .to_utc();
        assert_eq!(since("2026-09-30T11:57:00Z", now).as_deref(), Some("3m"));
        assert_eq!(since("2026-09-28T12:00:00Z", now).as_deref(), Some("2d"));
        assert_eq!(plural(1, "entry"), "1 entry");
        assert_eq!(plural(3, "entry"), "3 entries");
        assert_eq!(plural(2, "active run"), "2 active runs");
    }
}
