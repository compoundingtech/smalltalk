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
use st3_client::{Client, ClientError, Envelope, Page, Resource};

/// All daemon calls of one completion request, joins included, finish within this deadline.
pub const DEADLINE: Duration = Duration::from_millis(300);

/// Short-name resolution before a command runs may wait longer than a TAB; past it the word
/// keeps its literal meaning.
pub const RESOLVE_DEADLINE: Duration = Duration::from_secs(2);

const LIST_LIMIT: usize = 200;
/// A join only enriches candidates, so it gets less than the whole deadline and is dropped when
/// it runs out; the primary list still answers.
const JOIN_DEADLINE: Duration = Duration::from_millis(200);
/// Pages are followed until this many items; the deadline still bounds the whole request.
const MAX_ITEMS: usize = 5_000;
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
    /// Current mission steps, filtered by what the command can act on.
    Work(WorkFilter),
    /// Open attention items.
    Attention,
    /// Unarchived messages in the caller's mailbox.
    Message,
    /// Open lanes.
    Lane,
    /// Fleet hosts.
    Host,
    /// Planner-backed launch sessions.
    Launch,
    /// Active paired devices of the configured person.
    Device,
    /// Resource subscriptions that request missions.
    Subscription,
    /// Normalized harness sessions that have a conversation timeline.
    Session,
    /// Native harness sessions st does not manage yet; `importable_only` keeps ones `import run`
    /// accepts.
    NativeSession { importable_only: bool },
    /// Persons: the configured person and the persons current attention items and devices name.
    Person,
    /// Persons and agents that can act: the caller, the configured person, and current agents.
    Actor,
}

/// Which mission steps a work command accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkFilter {
    /// Every current step.
    Any,
    /// Steps another seat or person could claim or wake.
    Ready,
    /// Failed steps that can be retried.
    Failed,
    /// Claimed steps; in an agent seat, only the ones this seat holds.
    Claimed,
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
    let target = LocalTarget::discover(&completion_words())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime
        .block_on(async { tokio::time::timeout(deadline(), candidates(&target, entity)).await })
        .ok()?
        .ok()
}

/// [`DEADLINE`], unless `ST3_COMPLETION_DEADLINE_MS` names another bound in milliseconds, as tests
/// against a debug daemon on a loaded host do.
fn deadline() -> Duration {
    std::env::var("ST3_COMPLETION_DEADLINE_MS")
        .ok()
        .and_then(|millis| millis.parse().ok())
        .map_or(DEADLINE, Duration::from_millis)
}

/// The command line being completed: clap_complete passes it after `--`.
fn completion_words() -> Vec<String> {
    std::env::args()
        .skip_while(|word| word != "--")
        .skip(1)
        .collect()
}

/// The trusted local endpoint and identities, resolved the way ordinary commands do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalTarget {
    pub socket: PathBuf,
    /// `ST_AGENT` in a seat, otherwise the configured person.
    pub caller: Option<String>,
    /// `person` in the st config.
    pub person: Option<String>,
}

impl LocalTarget {
    /// `--endpoint` in `words` wins over `ST3_ENDPOINT`, which wins over the configured socket.
    /// A non-Unix endpoint yields `None`: client-v0 lists need the trusted local socket.
    pub fn discover(words: &[String]) -> Option<Self> {
        let config = st3::config::Config::load_unvalidated(None).ok()?;
        let endpoint = endpoint_word(words).or_else(|| std::env::var("ST3_ENDPOINT").ok());
        let socket = match endpoint.map(st3::client::Endpoint::parse) {
            Some(st3::client::Endpoint::Unix(socket)) => socket,
            Some(_) => return None,
            None => config.socket,
        };
        let agent = std::env::var("ST_AGENT")
            .ok()
            .filter(|agent| !agent.is_empty())
            .map(|agent| {
                if agent.starts_with("agent/") {
                    agent
                } else {
                    format!("agent/{agent}")
                }
            });
        Some(Self {
            socket,
            caller: agent.or_else(|| config.person.clone()),
            person: config.person,
        })
    }

    /// A client that fails at once while the daemon is unreachable.
    pub fn client(&self) -> Client {
        Client::unix(&self.socket).with_outage_wait(Duration::ZERO, false)
    }
}

/// The value of the last `--endpoint VALUE` or `--endpoint=VALUE` before the word being completed.
fn endpoint_word(words: &[String]) -> Option<String> {
    let typed = words.split_last().map_or(words, |(_, typed)| typed);
    let mut found = None;
    let mut words = typed.iter();
    while let Some(word) = words.next() {
        if word == "--endpoint" {
            found = words.next().cloned();
        } else if let Some(value) = word.strip_prefix("--endpoint=") {
            found = Some(value.to_owned());
        }
    }
    found
}

/// Follows a collection's pages until it ends or [`MAX_ITEMS`] are read.
async fn all_items<F, Fut>(mut page: F) -> Result<Vec<Resource>, ClientError>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<Envelope<Page>, ClientError>>,
{
    let mut items = Vec::new();
    let mut cursor = None;
    loop {
        let response = page(cursor.take()).await?.value;
        items.extend(response.items);
        match response.page.next_cursor {
            Some(next) if response.page.has_more && items.len() < MAX_ITEMS => cursor = Some(next),
            _ => return Ok(items),
        }
    }
}

/// Everything one entity kind needs from the daemon.
#[derive(Debug, Default)]
pub struct Listed {
    pub items: Vec<Resource>,
    /// Agents joined into terminal and actor descriptions.
    pub agents: Vec<Resource>,
    /// Devices joined into person candidates.
    pub devices: Vec<Resource>,
}

/// Current entities of one kind, filtered for the command, with curated descriptions.
pub async fn candidates(
    target: &LocalTarget,
    entity: Entity,
) -> Result<Vec<Candidate>, ClientError> {
    let listed = list(target, entity).await?;
    Ok(select(entity, &listed, target, chrono::Utc::now()))
}

async fn joined<F: std::future::Future<Output = Result<Vec<Resource>, ClientError>>>(
    join: F,
) -> Vec<Resource> {
    tokio::time::timeout(JOIN_DEADLINE, join)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

async fn list(target: &LocalTarget, entity: Entity) -> Result<Listed, ClientError> {
    let client = &target.client();
    let agents = || {
        all_items(|cursor| async move {
            client
                .agents_list(cursor.as_deref(), Some(LIST_LIMIT), false)
                .await
        })
    };
    let devices = || async {
        let Some(person) = &target.person else {
            return Ok(Vec::new());
        };
        let client =
            &Client::unix_as(&target.socket, person).with_outage_wait(Duration::ZERO, false);
        all_items(|cursor| async move {
            client
                .devices_list(cursor.as_deref(), Some(LIST_LIMIT), false)
                .await
        })
        .await
    };
    macro_rules! items {
        ($method:ident $(, $argument:expr)*) => {
            all_items(|cursor| async move {
                client.$method($($argument,)* cursor.as_deref(), Some(LIST_LIMIT), false).await
            })
            .await?
        };
    }
    let mut listed = Listed::default();
    match entity {
        Entity::Terminal => {
            let (terminals, agents) = tokio::join!(
                all_items(|cursor| async move {
                    client
                        .terminals_list(cursor.as_deref(), Some(LIST_LIMIT), false)
                        .await
                }),
                joined(agents()),
            );
            listed.items = terminals?;
            // The agent join adds harness and activity; a terminal still completes without it.
            listed.agents = agents;
        }
        Entity::Agent { .. } => listed.items = agents().await?,
        Entity::Actor => listed.agents = joined(agents()).await,
        Entity::Mission | Entity::MissionRun { .. } | Entity::MissionOrRun => {
            listed.items = items!(missions_list)
        }
        Entity::Work(_) => listed.items = items!(work_list),
        Entity::Attention => listed.items = items!(attention_list),
        Entity::Message => {
            if let Some(caller) = target.caller.as_deref() {
                listed.items = items!(messages_list_for_recipient, caller);
            }
        }
        Entity::Lane => listed.items = items!(lanes_list),
        Entity::Host => listed.items = items!(machines_list),
        Entity::Launch => listed.items = items!(launches_list),
        Entity::Device => listed.items = devices().await?,
        Entity::Subscription => listed.items = items!(subscriptions_list),
        Entity::Session => {
            // Conversation views are scoped to a person, as `conversations sessions` is.
            if let Some(person) = &target.person {
                let client = &Client::unix_as(&target.socket, person)
                    .with_outage_wait(Duration::ZERO, false);
                listed.items = all_items(|cursor| async move {
                    client
                        .sessions_list(cursor.as_deref(), Some(LIST_LIMIT), false)
                        .await
                })
                .await?;
            }
        }
        Entity::NativeSession { .. } => listed.items = items!(sessions_list_native),
        Entity::Person => {
            let (attention, devices) = tokio::join!(
                joined(all_items(|cursor| async move {
                    client
                        .attention_list(cursor.as_deref(), Some(LIST_LIMIT), false)
                        .await
                })),
                joined(devices()),
            );
            listed.items = attention;
            listed.devices = devices;
        }
    }
    Ok(listed)
}

/// Filters and describes listed entities for one argument. Pure, so tests can drive it.
pub fn select(
    entity: Entity,
    listed: &Listed,
    target: &LocalTarget,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<Candidate> {
    let agent_by_id = |id: &str| {
        listed.agents.iter().find_map(|agent| match agent {
            Resource::Agent(agent) if agent.header.id == id => Some(agent),
            _ => None,
        })
    };
    let mut found = Vec::new();
    match entity {
        Entity::Person | Entity::Actor => {
            let mut seen = std::collections::BTreeSet::new();
            let mut push = |subject: &str, parts: Vec<String>| {
                if seen.insert(subject.to_owned()) {
                    found.push(Candidate::new(subject.to_owned(), parts));
                }
            };
            if let Some(person) = &target.person {
                push(person, vec!["configured person".into()]);
            }
            if entity == Entity::Actor {
                if let Some(caller) = target
                    .caller
                    .as_deref()
                    .filter(|caller| caller.starts_with("agent/"))
                {
                    push(caller, vec!["this seat".into()]);
                }
                for agent in &listed.agents {
                    if let Resource::Agent(agent) = agent {
                        push(&agent.header.id, agent_parts(agent));
                    }
                }
            } else {
                for item in listed.items.iter().chain(&listed.devices) {
                    match item {
                        Resource::Attention(attention) => {
                            push(&attention.person_id, vec!["has attention items".into()])
                        }
                        Resource::Device(device) => {
                            push(&device.person_id, vec!["has paired devices".into()])
                        }
                        _ => {}
                    }
                }
            }
            return found;
        }
        _ => {}
    }
    for item in &listed.items {
        let candidate = match (entity, item) {
            (Entity::Terminal, Resource::Runtime(runtime)) if runtime.state == "running" => {
                let mut parts = vec![
                    runtime.state.clone(),
                    short_host(&runtime.owner_host_id).to_owned(),
                ];
                if let Some(agent) = agent_by_id(&runtime.owner_id) {
                    parts.extend(agent.driver.clone());
                    parts.extend(agent.harness_state.clone());
                    parts.extend(agent_work(agent));
                }
                // An incarnation is `PID:STARTED_AT`; `updated_at` moves with every observation.
                let started = runtime
                    .incarnation_id
                    .as_deref()
                    .and_then(|incarnation| incarnation.split_once(':'))
                    .map(|(_, at)| at);
                parts.extend(
                    started
                        .and_then(|at| since(at, now))
                        .map(|age| format!("up {age}")),
                );
                Candidate::new(runtime.owner_id.clone(), parts)
            }
            (Entity::Agent { running_only }, Resource::Agent(agent))
                if !running_only || agent.state == "running" =>
            {
                Candidate::new(agent.header.id.clone(), agent_parts(agent))
            }
            (
                Entity::Mission | Entity::MissionRun { .. } | Entity::MissionOrRun,
                Resource::Mission(mission),
            ) => {
                if matches!(entity, Entity::Mission | Entity::MissionOrRun) {
                    let mut parts = vec![mission.title.clone(), mission.state.clone()];
                    if let Some(active) = mission.active_runs.filter(|active| *active > 0) {
                        parts.push(plural(active, "active run"));
                    }
                    found.push(Candidate::new(mission.header.id.clone(), parts));
                }
                if entity != Entity::Mission {
                    let unfinished_only = entity
                        == (Entity::MissionRun {
                            unfinished_only: true,
                        });
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
                continue;
            }
            (Entity::Work(filter), Resource::Work(work))
                if work_accepts(filter, work, target.caller.as_deref()) =>
            {
                let mut parts = vec![
                    work.title.clone().unwrap_or_else(|| work.path.clone()),
                    work.state.clone(),
                ];
                parts.push(
                    work.mission_id
                        .clone()
                        .unwrap_or_else(|| work.mission_run_id.clone()),
                );
                parts.extend(work.claimant.clone().or_else(|| work.assigned_to.clone()));
                Candidate::new(work.header.id.clone(), parts)
            }
            (Entity::Attention, Resource::Attention(attention)) if attention.state == "open" => {
                let mut parts = vec![quote(&attention.title), attention.priority.clone()];
                parts.extend(attention.requester_id.clone());
                parts.extend(since(&attention.requested_at, now));
                Candidate::new(attention.header.id.clone(), parts)
            }
            (Entity::Message, Resource::Message(message)) if message.state != "archived" => {
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
                Candidate::new(message.header.id.clone(), parts)
            }
            (Entity::Lane, Resource::Lane(lane)) => Candidate::new(
                lane.header.id.clone(),
                vec![
                    lane.name.clone(),
                    lane.state.clone(),
                    plural(lane.entries.len(), "entry"),
                ],
            ),
            (Entity::Host, Resource::Machine(machine)) => Candidate::new(
                machine.host_id.clone(),
                vec![
                    machine.state.clone(),
                    plural(machine.runtime_ids.len(), "runtime"),
                ],
            ),
            (Entity::Launch, Resource::Launch(launch)) => Candidate::new(
                launch.header.id.clone(),
                vec![
                    quote(&launch.title),
                    launch.phase.clone(),
                    launch.planner.clone(),
                    plural(launch.variants.len(), "variant"),
                ],
            ),
            (Entity::Device, Resource::Device(device)) if device.state == "active" => {
                let mut parts = vec![
                    device.name.clone().unwrap_or_default(),
                    device.state.clone(),
                ];
                parts.push(plural(device.scopes.len(), "scope"));
                parts.push(format!(
                    "expires {}",
                    device.expires_at.get(..10).unwrap_or(&device.expires_at)
                ));
                Candidate::new(device.header.id.clone(), parts)
            }
            (Entity::Subscription, Resource::Subscription(subscription)) => {
                let mut parts = vec![
                    subscription.state.clone(),
                    format!("on {}", subscription.spec.observer),
                ];
                parts.extend(
                    subscription
                        .spec
                        .mission
                        .clone()
                        .map(|mission| format!("starts {mission}")),
                );
                Candidate::new(subscription.header.id.clone(), parts)
            }
            (Entity::Session, Resource::Session(session)) => {
                let mut parts = vec![session.owner_id.clone(), session.state.clone()];
                parts.extend(
                    since(&session.started_at, now).map(|age| format!("started {age} ago")),
                );
                Candidate::new(session.header.id.clone(), parts)
            }
            (Entity::NativeSession { importable_only }, Resource::Session(session))
                if session.extra.get("managed") == Some(&serde_json::Value::Bool(false))
                    && (!importable_only
                        || session.extra.get("importable")
                            == Some(&serde_json::Value::Bool(true))) =>
            {
                let text = |key: &str| {
                    session
                        .extra
                        .get(key)
                        .and_then(|value| value.as_str())
                        .map(str::to_owned)
                };
                let mut parts = vec![session.state.clone()];
                parts.extend(text("harness"));
                parts.extend(text("workspace"));
                parts.extend(
                    since(&session.started_at, now).map(|age| format!("started {age} ago")),
                );
                Candidate::new(session.header.id.clone(), parts)
            }
            _ => continue,
        };
        found.push(candidate);
    }
    found
}

fn work_accepts(filter: WorkFilter, work: &st3_client::Work, caller: Option<&str>) -> bool {
    match filter {
        WorkFilter::Any => true,
        WorkFilter::Ready => work.state == "ready",
        WorkFilter::Failed => work.state == "failed",
        WorkFilter::Claimed => {
            work.state == "claimed"
                && match caller.filter(|caller| caller.starts_with("agent/")) {
                    Some(seat) => work.claimant.as_deref() == Some(seat),
                    None => true,
                }
        }
    }
}

fn agent_parts(agent: &st3_client::Agent) -> Vec<String> {
    let mut parts = vec![agent.state.clone()];
    parts.extend(agent.host_id.as_deref().map(short_host).map(str::to_owned));
    parts.extend(agent.driver.clone());
    parts.extend(agent.harness_state.clone());
    parts.extend(agent_work(agent));
    parts
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
            "agent/project/steward",
            "agent/host-a.ci-watcher",
            "agent/interactive/host-a/a13d6263-3bb5-4b",
            "agent/interactive/host-a/43b16cd2-ec69-44",
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
        assert_eq!(subject("steward"), "agent/project/steward");
        assert_eq!(subject("a13d"), "agent/interactive/host-a/a13d6263-3bb5-4b");
        assert_eq!(subject("ci-w"), "agent/host-a.ci-watcher");
        assert_eq!(subject("agent/other"), "agent/other");
        // Step 3 wins before step 4: `host-a` prefixes one last segment.
        assert_eq!(subject("host-a"), "agent/host-a.ci-watcher");
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
            candidate("agent/project/steward"),
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

    fn fixture(kind: &str, edit: impl FnOnce(&mut serde_json::Value)) -> Resource {
        let all: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../../docs/st3/client-v0/fixtures/resources.json"
        ))
        .unwrap();
        let mut item = all.into_iter().find(|item| item["kind"] == kind).unwrap();
        edit(&mut item);
        serde_json::from_value(item).unwrap()
    }

    fn target(caller: Option<&str>) -> LocalTarget {
        LocalTarget {
            socket: "/nonexistent".into(),
            caller: caller.map(str::to_owned),
            person: Some("person/johannes".into()),
        }
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-09-30T12:00:00Z")
            .unwrap()
            .to_utc()
    }

    #[test]
    fn terminals_join_agents_and_age_from_the_incarnation() {
        let listed = Listed {
            items: vec![
                fixture("runtime", |item| {
                    item["incarnation_id"] = "42:2026-09-30T10:00:00Z".into();
                    item["updated_at"] = "2026-09-30T11:59:00Z".into();
                }),
                fixture("runtime", |item| item["state"] = "stopped".into()),
            ],
            agents: vec![fixture("agent", |item| {
                item["id"] = "agent/release".into();
                item["driver"] = "omp".into();
                item["harness_state"] = "idle".into();
            })],
            devices: vec![],
        };
        let found = select(Entity::Terminal, &listed, &target(None), now());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].subject, "agent/release");
        assert!(
            found[0].description.contains("omp · idle"),
            "{}",
            found[0].description
        );
        assert!(
            found[0].description.ends_with("up 2h"),
            "{}",
            found[0].description
        );
    }

    #[test]
    fn work_filters_follow_the_command() {
        let work = |state: &str, claimant: Option<&str>| {
            fixture("work", |item| {
                item["state"] = state.into();
                item["claimant"] = claimant.into();
                item["id"] = format!("step-run/{state}-{}", claimant.unwrap_or("none")).into();
            })
        };
        let listed = Listed {
            items: vec![
                work("ready", None),
                work("failed", None),
                work("claimed", Some("agent/me")),
                work("claimed", Some("agent/other")),
            ],
            ..Listed::default()
        };
        let subjects = |filter, caller| {
            select(Entity::Work(filter), &listed, &target(caller), now())
                .into_iter()
                .map(|candidate| candidate.subject)
                .collect::<Vec<_>>()
        };
        assert_eq!(subjects(WorkFilter::Ready, None), ["step-run/ready-none"]);
        assert_eq!(subjects(WorkFilter::Failed, None), ["step-run/failed-none"]);
        assert_eq!(
            subjects(WorkFilter::Claimed, Some("agent/me")),
            ["step-run/claimed-agent/me"]
        );
        assert_eq!(
            subjects(WorkFilter::Claimed, Some("person/johannes")).len(),
            2
        );
        assert_eq!(subjects(WorkFilter::Any, None).len(), 4);
    }

    #[test]
    fn native_sessions_keep_only_unmanaged_and_importable() {
        let session = |id: &str, managed: bool, importable: bool| {
            fixture("session", |item| {
                item["id"] = id.into();
                item["managed"] = managed.into();
                item["importable"] = importable.into();
                item["harness"] = "claude".into();
            })
        };
        let listed = Listed {
            items: vec![
                session("session/managed", true, true),
                session("session/saved", false, false),
                session("session/live", false, true),
            ],
            ..Listed::default()
        };
        let subjects = |importable_only| {
            select(
                Entity::NativeSession { importable_only },
                &listed,
                &target(None),
                now(),
            )
            .into_iter()
            .map(|candidate| candidate.subject)
            .collect::<Vec<_>>()
        };
        assert_eq!(subjects(false), ["session/saved", "session/live"]);
        assert_eq!(subjects(true), ["session/live"]);
    }

    #[test]
    fn people_and_actors_are_deduplicated() {
        let listed = Listed {
            items: vec![
                fixture("attention", |item| {
                    item["person_id"] = "person/johannes".into()
                }),
                fixture("attention", |item| {
                    item["person_id"] = "person/nathan".into()
                }),
            ],
            agents: vec![fixture("agent", |item| item["id"] = "agent/me".into())],
            devices: vec![fixture("device", |item| {
                item["person_id"] = "person/nathan".into()
            })],
        };
        let subjects = |entity| {
            select(entity, &listed, &target(Some("agent/me")), now())
                .into_iter()
                .map(|candidate| candidate.subject)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            subjects(Entity::Person),
            ["person/johannes", "person/nathan"]
        );
        assert_eq!(subjects(Entity::Actor), ["person/johannes", "agent/me"]);
    }

    #[tokio::test]
    async fn pages_are_followed_to_the_end() {
        let cursors = std::cell::RefCell::new(Vec::new());
        let items = all_items(|cursor: Option<String>| {
            let next = match cursor.as_deref() {
                None => Some("second"),
                Some("second") => Some("third"),
                _ => None,
            };
            cursors.borrow_mut().push(cursor);
            let page = serde_json::json!({
                "api_version": "st3.client.v0",
                "request_id": "request/test",
                "snapshot": {"id": "snapshot/t", "host_id": "host/t", "store_index": 0,
                             "projection_version": "v0", "created_at": "2026-09-30T12:00:00Z"},
                "value": {"kind": "page", "collection": "lanes", "items": [],
                          "page": {"limit": 1, "has_more": next.is_some(), "next_cursor": next}},
            });
            async move { Ok(serde_json::from_value(page).unwrap()) }
        })
        .await
        .unwrap();
        assert!(items.is_empty());
        assert_eq!(
            cursors.into_inner(),
            [None, Some("second".to_owned()), Some("third".to_owned())]
        );
    }

    #[test]
    fn the_typed_endpoint_wins() {
        let words = |line: &str| line.split(' ').map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            endpoint_word(&words("st --endpoint /a.sock terminals attach x")).as_deref(),
            Some("/a.sock")
        );
        assert_eq!(
            endpoint_word(&words("st --endpoint=/b.sock agents stop x")).as_deref(),
            Some("/b.sock")
        );
        assert_eq!(
            endpoint_word(&words("st terminals attach --endpoint")),
            None
        );
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
