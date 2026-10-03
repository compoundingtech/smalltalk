//! A seat's watch on one GitHub issue or pull request.
//!
//! A watch is a subscription that the daemon declares for a seat on the fleet's one standing
//! `github.repository` observer of the repository. That observer records every item's newest
//! comments and reviews, its state, and how the checks its base requires stand, and the write
//! that records an observation decides each watch's wakes from the item's prior facts: one for
//! each new comment or review, one for each move of the required checks into pass or fail on the
//! current head, and a final one when the thread closes or merges, which ends that watch. No
//! thread has a poller of its own.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

use smallclaims::hash::canonical_hash;

use crate::model::{MessageView, St3Error, WatchSpec};

/// How often the standing observer polls while a watch uses it. An unchanged repository answers
/// each conditional read with a free 304.
pub const WATCH_POLL: &str = "30s";

/// The data types a watch needs from the observer: comments and reviews, and the state and
/// checks of issues and pull requests.
pub const WATCH_FIELDS: [&str; 3] = ["comments", "issues", "pull_requests"];

/// How far before a watch began a comment can be and still wake it, for clock skew with GitHub
/// and a reply that the observer first sees after the watch began.
const WATCH_SKEW_MS: u128 = 5 * 60_000;

/// The longest excerpt of a comment or review a wake carries.
pub const EXCERPT_CHARS: usize = 600;

/// How long a delivering host waits for GitHub to read an excerpt.
const EXCERPT_TIMEOUT: Duration = Duration::from_secs(2);

/// The tag a watch's wake carries, and the tag that names the comment or review it reports.
pub const WATCH_TAG: &str = "github-watch";
const COMMENT_TAG: &str = "github-comment:";

/// One issue or pull request: `OWNER/REPO#N`. Owner and repository are kept lowercase, as GitHub
/// treats them, so one thread always has one watch subject.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadRef {
    pub owner: String,
    pub repository: String,
    pub number: u64,
}

impl ThreadRef {
    /// Read `OWNER/REPO#N` or a GitHub issue, pull request or comment URL.
    pub fn parse(text: &str) -> Result<Self, St3Error> {
        let invalid = || {
            St3Error::new(
                "invalid-github-thread",
                format!("`{text}` is not OWNER/REPO#NUMBER or a GitHub issue or pull request URL"),
            )
        };
        let text = text.trim();
        let (owner, repository, number) = if let Some(path) = text
            .strip_prefix("https://github.com/")
            .or_else(|| text.strip_prefix("http://github.com/"))
        {
            let path = path.split(['#', '?']).next().unwrap_or_default();
            let parts = path.split('/').collect::<Vec<_>>();
            match parts.as_slice() {
                [owner, repository, "issues" | "pull", number, ..] => {
                    (*owner, *repository, *number)
                }
                _ => return Err(invalid()),
            }
        } else {
            let (repository, number) = text.split_once('#').ok_or_else(invalid)?;
            let (owner, repository) = repository.split_once('/').ok_or_else(invalid)?;
            (owner, repository, number)
        };
        let name = |part: &str| {
            !part.is_empty()
                && part.len() <= 100
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
                && part != "."
                && part != ".."
        };
        if !name(owner) || !name(repository) {
            return Err(invalid());
        }
        let number = number
            .parse::<u64>()
            .ok()
            .filter(|number| *number > 0)
            .ok_or_else(invalid)?;
        Ok(Self {
            owner: owner.to_ascii_lowercase(),
            repository: repository.to_ascii_lowercase(),
            number,
        })
    }

    /// `OWNER/REPO`, the repository observer's locator.
    pub fn locator(&self) -> String {
        format!("{}/{}", self.owner, self.repository)
    }

    /// The fleet's standing observer of this thread's repository.
    pub fn observer(&self) -> String {
        format!("observer/github/{}", self.locator())
    }

    /// The repository resource the standing observer records into.
    pub fn resource(&self) -> String {
        format!("resource/github/{}", self.locator())
    }

    /// This seat's watch on this thread.
    pub fn watch(&self, agent: &str) -> String {
        format!(
            "subscription/watch/{}/{}/{}",
            self.locator(),
            self.number,
            agent.trim_start_matches("agent/")
        )
    }
}

impl std::fmt::Display for ThreadRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}#{}", self.locator(), self.number)
    }
}

/// Whether an observer is a repository's standing observer, as a watch declares it:
/// `observer/github/OWNER/REPO` on `resource/github/OWNER/REPO`.
pub fn is_standing_observer(subject: &str, resource: &str) -> bool {
    subject
        .strip_prefix("observer/github/")
        .is_some_and(|locator| {
            locator.split('/').count() == 2 && resource == format!("resource/github/{locator}")
        })
}

/// The thread a watch subject names, and the seat it belongs to.
pub fn watch_parts(subject: &str) -> Option<(ThreadRef, String)> {
    let rest = subject.strip_prefix("subscription/watch/")?;
    let mut parts = rest.splitn(4, '/');
    let owner = parts.next()?;
    let repository = parts.next()?;
    let number = parts.next()?.parse().ok()?;
    let agent = parts.next()?;
    Some((
        ThreadRef {
            owner: owner.into(),
            repository: repository.into(),
            number,
        },
        format!("agent/{agent}"),
    ))
}

fn rfc3339(unix_ms: u128) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
        i64::try_from(unix_ms).unwrap_or(i64::MAX),
    )
    .unwrap_or_default()
    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn parse_rfc3339_ms(text: &str) -> Option<u128> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .and_then(|at| u128::try_from(at.timestamp_millis()).ok())
}

/// The declarations that start the repository's standing observer.
pub fn observer_source(thread: &ThreadRef) -> String {
    let locator = thread.locator();
    format!(
        r#"version 2
resource "github/{locator}" {{ kind "vcs.repository" }}
observer "github/{locator}" {{
  resource "{resource}"
  provider "github.repository"
  locator "{locator}"
  field "issues"
  every "{WATCH_POLL}"
}}
"#,
        resource = thread.resource(),
    )
}

/// The declaration of one seat's watch, from `since`, until `until` when one is named.
pub fn watch_source(
    thread: &ThreadRef,
    agent: &str,
    since_unix_ms: u128,
    until_unix_ms: Option<u128>,
) -> String {
    let name = thread.watch(agent);
    let name = name.trim_start_matches("subscription/");
    let until = until_unix_ms.map_or(String::new(), |until| {
        format!("; until \"{}\"", rfc3339(until))
    });
    let on = WATCH_FIELDS
        .iter()
        .map(|field| format!("  on \"{field}\"\n"))
        .collect::<String>();
    format!(
        r#"version 2
subscription "{name}" {{
  observer "{observer}"
  to "{agent}"
{on}  delivery "watch" {{ item {number}; since "{since}"{until} }}
}}
"#,
        observer = thread.observer(),
        number = thread.number,
        since = rfc3339(since_unix_ms),
    )
}

/// The declaration that stops a watch or the standing observer.
pub fn stop_source(subject: &str) -> Option<String> {
    let (kind, name) = subject.split_once('/')?;
    Some(format!("version 2\n{kind} \"{name}\" {{ stop }}\n"))
}

/// What one observation tells a watch.
#[derive(Clone, Debug, PartialEq)]
pub enum WatchEvent {
    /// A comment or review it did not know: `{kind, id, author, at}`, and a review's `state`.
    Comment(Value),
    /// The checks the base requires moved into `pass` or `fail` on the current head.
    Checks(Value),
    /// The thread closed or merged.
    Ended { merged: bool },
}

impl WatchEvent {
    /// What tells this event from every other one the watch hears. The checks key names the item
    /// observation that saw the move, so a rerun that fails again on the same head is new, and
    /// a replay of that observation is not.
    fn key(&self, observation: &str) -> String {
        match self {
            Self::Comment(entry) => {
                let (kind, id) = crate::resource::recent_comment_key(entry);
                format!("{kind}:{id}")
            }
            Self::Checks(_) => format!("checks:{observation}"),
            Self::Ended { .. } => "ended".into(),
        }
    }
}

/// The events one observation of a watched item holds, oldest first: the comments and reviews
/// `seen` names that `prior` did not, made no earlier than five minutes before the watch
/// began; a move of the required checks into pass or fail, or a new head first seen there; and
/// the thread closing. With no prior facts the item is new to the observer, so it holds no
/// check move and no close.
pub fn watch_events(
    watch: &WatchSpec,
    prior: Option<&Value>,
    seen: &Value,
    facts: &Value,
) -> Vec<WatchEvent> {
    let mut events = Vec::new();
    let known = prior
        .and_then(|prior| prior.get("recent_comments"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let known_keys = known
        .iter()
        .map(crate::resource::recent_comment_key)
        .collect::<std::collections::BTreeSet<_>>();
    // An entry older than every one the item kept fell off its list; seeing it again, as an edit
    // does, is not news.
    let oldest_kept = (known.len() >= crate::resource::RECENT_COMMENTS)
        .then(|| {
            known
                .first()
                .and_then(|entry| entry.get("at"))
                .and_then(Value::as_str)
        })
        .flatten();
    let earliest = watch.since_unix_ms.saturating_sub(WATCH_SKEW_MS);
    let seen_entries = crate::resource::merge_recent_comments(
        [],
        seen.get("recent_comments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten(),
        None,
    );
    for entry in seen_entries {
        if known_keys.contains(&crate::resource::recent_comment_key(&entry)) {
            continue;
        }
        let at = entry.get("at").and_then(Value::as_str).unwrap_or_default();
        if oldest_kept.is_some_and(|oldest| at < oldest)
            || parse_rfc3339_ms(at).is_none_or(|at| at < earliest)
        {
            continue;
        }
        events.push(WatchEvent::Comment(entry));
    }
    let Some(prior) = prior else {
        return events;
    };
    let head = |facts: &Value| {
        facts
            .get("head_sha")
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let state = |facts: &Value| {
        facts
            .pointer("/required_checks/state")
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    if let Some(now) = state(facts).filter(|state| state == "pass" || state == "fail")
        && (head(prior) != head(facts) || state(prior).as_deref() != Some(now.as_str()))
    {
        events.push(WatchEvent::Checks(
            facts.get("required_checks").cloned().unwrap_or(Value::Null),
        ));
    }
    let open = |facts: &Value| facts.get("state").and_then(Value::as_str) == Some("open");
    if open(prior) && facts.get("state").is_some() && !open(facts) {
        events.push(WatchEvent::Ended {
            merged: facts.get("merged").and_then(Value::as_bool) == Some(true),
        });
    }
    events
}

/// One wake: its message subject, the key it was decided by, title, content and tags.
pub struct Wake {
    pub subject: String,
    pub delivery_key: String,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
}

/// The wake one event sends a watch's seat. It says who did what where in prose, and names the
/// comment or review so the delivering host can add an excerpt and the seat can tell its own.
pub fn wake(
    watch_subject: &str,
    watch: &WatchSpec,
    thread: &ThreadRef,
    facts: &Value,
    event: &WatchEvent,
    observation: &str,
    posted_by: Option<&str>,
) -> Result<Wake, St3Error> {
    let delivery_key = canonical_hash(&(
        WATCH_TAG,
        watch_subject,
        watch.since_unix_ms.to_string(),
        event.key(observation),
    ))
    .map_err(|error| St3Error::new("internal-error", error.to_string()))?;
    let pull = facts.get("head_sha").is_some()
        || facts
            .get("url")
            .and_then(Value::as_str)
            .is_some_and(|url| url.contains("/pull/"));
    let url = facts
        .get("url")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                "https://github.com/{}/{}/{}",
                thread.locator(),
                if pull { "pull" } else { "issues" },
                thread.number
            )
        });
    let title = facts
        .get("title")
        .and_then(Value::as_str)
        .map(|title| format!(" \"{title}\""))
        .unwrap_or_default();
    let what = if pull { "pull request" } else { "issue" };
    let short_head = facts
        .get("head_sha")
        .and_then(Value::as_str)
        .map(|head| head.chars().take(7).collect::<String>())
        .unwrap_or_default();
    let stop = format!("To stop watching: st gh unwatch {thread}");
    let mut tags = vec![WATCH_TAG.to_owned(), watch_subject.to_owned()];
    let (headline, body) = match event {
        WatchEvent::Comment(entry) => {
            let (kind, id) = crate::resource::recent_comment_key(entry);
            let login = entry
                .get("author")
                .and_then(Value::as_str)
                .unwrap_or("someone");
            let who = posted_by.map_or_else(
                || format!("@{login}"),
                |seat| format!("{seat} (as @{login})"),
            );
            let did = if kind == "review" {
                match entry.get("state").and_then(Value::as_str) {
                    Some("approved") => "approved",
                    Some("changes_requested") => "requested changes on",
                    _ => "reviewed",
                }
            } else {
                "commented on"
            };
            let anchor = if kind == "review" {
                format!("#pullrequestreview-{id}")
            } else {
                format!("#issuecomment-{id}")
            };
            tags.push(format!(
                "{COMMENT_TAG}{}:{kind}:{id}:{}",
                thread.locator(),
                thread.number
            ));
            (
                format!("{who} {did} {thread}"),
                format!(
                    "{who} {did} {thread}{title} ({what}, {state})\n{url}{anchor}",
                    state = facts.get("state").and_then(Value::as_str).unwrap_or("open"),
                ),
            )
        }
        WatchEvent::Checks(checks) => {
            let state = checks
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let which = if checks.get("source").and_then(Value::as_str) == Some("all") {
                "All checks"
            } else {
                "Required checks"
            };
            let names = |name: &str| {
                checks
                    .get(name)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let (verb, named) = if state == "fail" {
                ("failed", names("failed"))
            } else {
                ("passed", names("checks"))
            };
            (
                format!("{which} {verb} on {thread}"),
                format!("{which} {verb} on {thread}{title} at {short_head}: {named}\n{url}/checks"),
            )
        }
        WatchEvent::Ended { merged } => {
            let how = if *merged {
                "merged".to_owned()
            } else {
                match facts.get("state_reason").and_then(Value::as_str) {
                    Some("not_planned") => "closed as not planned".to_owned(),
                    Some("completed") => "closed as completed".to_owned(),
                    _ => "closed".to_owned(),
                }
            };
            (
                format!("{thread} {how}"),
                format!("{thread}{title} {how}. This watch has ended.\n{url}"),
            )
        }
    };
    let content = if matches!(event, WatchEvent::Ended { .. }) {
        body
    } else {
        format!("{body}\n\n{stop}")
    };
    Ok(Wake {
        subject: format!("message/watch-{}", &delivery_key[..20]),
        delivery_key,
        title: headline,
        content,
        tags,
    })
}

/// The final wake of a watch whose deadline passed.
pub fn deadline_wake(
    watch_subject: &str,
    watch: &WatchSpec,
    thread: &ThreadRef,
) -> Result<Wake, St3Error> {
    let delivery_key = canonical_hash(&(
        WATCH_TAG,
        watch_subject,
        watch.since_unix_ms.to_string(),
        "deadline",
    ))
    .map_err(|error| St3Error::new("internal-error", error.to_string()))?;
    let until = watch.until_unix_ms.map(rfc3339).unwrap_or_default();
    Ok(Wake {
        subject: format!("message/watch-{}", &delivery_key[..20]),
        delivery_key,
        title: format!("Your watch on {thread} reached its deadline"),
        content: format!(
            "Your watch on {thread} reached its deadline ({until}) and has ended.\n\
             To watch it again: st gh watch {thread}"
        ),
        tags: vec![WATCH_TAG.to_owned(), watch_subject.to_owned()],
    })
}

/// The comment or review a wake names: the repository, its kind and ID, and the thread.
fn named_comment(message: &MessageView) -> Option<(String, String, u64, u64)> {
    message.tags.iter().find_map(|tag| {
        let rest = tag.strip_prefix(COMMENT_TAG)?;
        let mut parts = rest.rsplitn(4, ':');
        let number = parts.next()?.parse().ok()?;
        let id = parts.next()?.parse().ok()?;
        let kind = parts.next()?.to_owned();
        let locator = parts.next()?.to_owned();
        Some((locator, kind, id, number))
    })
}

/// The subject that records which seat posted one comment or review, by its GitHub ID.
pub fn github_post_subject(locator: &str, kind: &str, id: u64) -> String {
    format!("github-post/{locator}/{kind}/{id}")
}

/// The posts seats on this host have in flight: a seat, a repository and a thread. While one is,
/// that thread's wakes wait for the seat, so the seat never sees its own comment before st knows
/// its ID.
fn posts_in_flight() -> &'static Mutex<HashMap<(String, String, u64), usize>> {
    static IN_FLIGHT: OnceLock<Mutex<HashMap<(String, String, u64), usize>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(Default::default)
}

/// A seat's post in flight; dropping it ends the post.
pub struct PostInFlight {
    key: (String, String, u64),
}

impl PostInFlight {
    pub fn begin(agent: &str, thread: &ThreadRef) -> Self {
        let key = (agent.to_owned(), thread.locator(), thread.number);
        *posts_in_flight()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(key.clone())
            .or_default() += 1;
        Self { key }
    }
}

impl Drop for PostInFlight {
    fn drop(&mut self) {
        let mut posts = posts_in_flight()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = posts.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                posts.remove(&self.key);
            }
        }
    }
}

/// Whether a seat has a post in flight on the thread a wake names.
pub fn post_in_flight(agent: &str, message: &MessageView) -> bool {
    let Some((locator, _, _, number)) = named_comment(message) else {
        return false;
    };
    posts_in_flight()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains_key(&(agent.to_owned(), locator, number))
}

/// The registered GitHub object a wake names, as `github.posted` keys it.
pub fn named_object(message: &MessageView) -> Option<(String, String, u64)> {
    named_comment(message).map(|(locator, kind, id, _)| (locator, kind, id))
}

/// Excerpts this host read, by comment or review, including the ones GitHub would not give.
fn excerpts() -> &'static Mutex<HashMap<(String, String, u64), Option<String>>> {
    static EXCERPTS: OnceLock<Mutex<HashMap<(String, String, u64), Option<String>>>> =
        OnceLock::new();
    EXCERPTS.get_or_init(Default::default)
}

/// `text` cut to `EXCERPT_CHARS` at a word, quoted line by line.
pub fn quote(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut cut = text.chars().take(EXCERPT_CHARS).collect::<String>();
    if cut.len() < text.len() {
        if let Some(space) = cut.rfind(char::is_whitespace).filter(|space| *space > 0) {
            cut.truncate(space);
        }
        cut = format!("{} …", cut.trim_end());
    }
    Some(
        cut.lines()
            .map(|line| format!("> {line}").trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Read a comment's or review's text from GitHub, at most `EXCERPT_TIMEOUT`. A review without a
/// body is described by its first inline comment.
async fn read_excerpt(
    api_base: &str,
    token: &str,
    locator: &str,
    kind: &str,
    id: u64,
    number: u64,
) -> Option<String> {
    let client = crate::resource::github_api_client();
    let get = |url: String| {
        let client = client.clone();
        async move {
            let response = client
                .get(url)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .bearer_auth(token)
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?;
            response.json::<Value>().await.ok()
        }
    };
    let base = format!("{api_base}/repos/{locator}");
    let read = async {
        if kind == "review" {
            let review = get(format!("{base}/pulls/{number}/reviews/{id}")).await?;
            if let Some(body) = review.get("body").and_then(Value::as_str).and_then(quote) {
                return Some(body);
            }
            let comments = get(format!(
                "{base}/pulls/{number}/reviews/{id}/comments?per_page=100"
            ))
            .await?;
            let comments = comments.as_array()?;
            let first = comments.first()?;
            let place = first
                .get("path")
                .and_then(Value::as_str)
                .map(|path| match first.get("line").and_then(Value::as_u64) {
                    Some(line) => format!("{path}:{line}"),
                    None => path.to_owned(),
                })
                .unwrap_or_default();
            let lead = format!(
                "left {} inline comment{}; the first, on {place}:",
                comments.len(),
                if comments.len() == 1 { "" } else { "s" }
            );
            let body = first.get("body").and_then(Value::as_str).and_then(quote)?;
            Some(format!("{lead}\n{body}"))
        } else {
            let comment = get(format!("{base}/issues/comments/{id}")).await?;
            comment.get("body").and_then(Value::as_str).and_then(quote)
        }
    };
    tokio::time::timeout(EXCERPT_TIMEOUT, read)
        .await
        .ok()
        .flatten()
}

/// Add each wake's excerpt to what its seat sees, read from GitHub when the wake is delivered
/// and kept only in this host's memory: comment text never enters the graph. A read that fails or
/// takes too long leaves the wake as it is, and the outcome is kept, so the seat sees the same
/// message each time it is sent.
pub async fn add_excerpts(messages: &mut [MessageView]) {
    let wanted = messages
        .iter()
        .filter(|message| message.tags.iter().any(|tag| tag == WATCH_TAG))
        .filter_map(named_comment)
        .collect::<Vec<_>>();
    if wanted.is_empty() {
        return;
    }
    let missing = {
        let known = excerpts().lock().unwrap_or_else(PoisonError::into_inner);
        wanted
            .iter()
            .filter(|(locator, kind, id, _)| {
                !known.contains_key(&(locator.clone(), kind.clone(), *id))
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    if !missing.is_empty() {
        let token = crate::resource::github_token().await.ok();
        let api_base = crate::resource::github_api_base();
        let reads = missing.iter().map(|(locator, kind, id, number)| {
            let token = token.clone();
            let api_base = api_base.clone();
            async move {
                match token {
                    Some(token) => {
                        read_excerpt(&api_base, &token, locator, kind, *id, *number).await
                    }
                    None => None,
                }
            }
        });
        let read = futures_util::future::join_all(reads).await;
        let mut known = excerpts().lock().unwrap_or_else(PoisonError::into_inner);
        if known.len() > 4_096 {
            known.clear();
        }
        for ((locator, kind, id, _), excerpt) in missing.into_iter().zip(read) {
            known.insert((locator, kind, id), excerpt);
        }
    }
    let known = excerpts().lock().unwrap_or_else(PoisonError::into_inner);
    for message in messages.iter_mut() {
        let Some((locator, kind, id, _)) = named_comment(message) else {
            continue;
        };
        if !message.tags.iter().any(|tag| tag == WATCH_TAG) {
            continue;
        }
        let Some(Some(excerpt)) = known.get(&(locator, kind, id)) else {
            continue;
        };
        // The excerpt follows the link, the second line.
        let mut lines = message.content.splitn(3, '\n');
        let headline = lines.next().unwrap_or_default();
        let link = lines.next().unwrap_or_default();
        let rest = lines.next().unwrap_or_default();
        message.content = format!("{headline}\n{link}\n{excerpt}\n{rest}");
    }
}

/// A watch as `st gh ls` shows it.
pub fn watch_view(
    subject: &str,
    spec: &WatchSpec,
    ended: Option<&Value>,
    facts: Option<&Value>,
    observer_state: Option<&Value>,
) -> Value {
    let (thread, agent) = watch_parts(subject).unzip();
    let state = match (ended, observer_state) {
        (Some(_), _) => "ended",
        (None, Some(observer))
            if observer.get("state").and_then(Value::as_str) == Some("degraded") =>
        {
            "degraded"
        }
        _ => "active",
    };
    json!({
        "subject": subject,
        "thread": thread.map(|thread| thread.to_string()),
        "agent": agent,
        "item": spec.item,
        "since": rfc3339(spec.since_unix_ms),
        "until": spec.until_unix_ms.map(rfc3339),
        "state": state,
        "ended": ended.and_then(|ended| ended.get("reason")).cloned(),
        "reason": observer_state
            .filter(|_| state == "degraded")
            .and_then(|observer| observer.get("reason"))
            .cloned(),
        "title": facts.and_then(|facts| facts.get("title")).cloned(),
        "url": facts.and_then(|facts| facts.get("url")).cloned(),
        "thread_state": facts.and_then(|facts| facts.get("state")).cloned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch() -> WatchSpec {
        WatchSpec {
            item: 12,
            since_unix_ms: parse_rfc3339_ms("2026-09-10T05:00:00Z").unwrap(),
            until_unix_ms: None,
        }
    }

    fn entry(kind: &str, id: u64, at: &str) -> Value {
        json!({"kind": kind, "id": id, "author": "fern", "at": at})
    }

    #[test]
    fn a_thread_reads_from_a_reference_or_a_url() {
        let thread = ThreadRef::parse("Acme/Garden#12").unwrap();
        assert_eq!(thread.to_string(), "acme/garden#12");
        for url in [
            "https://github.com/acme/garden/pull/12",
            "https://github.com/acme/garden/issues/12#issuecomment-5",
            "https://github.com/acme/garden/pull/12/files",
        ] {
            assert_eq!(ThreadRef::parse(url).unwrap(), thread, "{url}");
        }
        for invalid in [
            "acme/garden",
            "acme#12",
            "acme/garden#0",
            "https://github.com/acme/garden",
        ] {
            assert!(ThreadRef::parse(invalid).is_err(), "{invalid}");
        }
        assert_eq!(
            thread.watch("agent/example/planner"),
            "subscription/watch/acme/garden/12/example/planner"
        );
        assert_eq!(
            watch_parts("subscription/watch/acme/garden/12/example/planner"),
            Some((thread, "agent/example/planner".into()))
        );
    }

    #[test]
    fn each_new_comment_is_one_event_and_old_or_known_ones_are_not() {
        let prior = json!({"state": "open", "recent_comments": [entry("comment", 1, "2026-09-10T05:01:00Z")]});
        let seen = json!({"recent_comments": [
            entry("comment", 3, "2026-09-10T05:03:00Z"),
            entry("comment", 2, "2026-09-10T05:02:00Z"),
            entry("comment", 1, "2026-09-10T05:01:00Z"),
            // Made long before the watch began, and first seen now.
            entry("comment", 0, "2026-09-09T00:00:00Z"),
        ]});
        let events = watch_events(&watch(), Some(&prior), &seen, &prior);
        assert_eq!(
            events,
            vec![
                WatchEvent::Comment(entry("comment", 2, "2026-09-10T05:02:00Z")),
                WatchEvent::Comment(entry("comment", 3, "2026-09-10T05:03:00Z")),
            ]
        );

        // An item that kept twenty entries: one older than all of them fell off, and is no news.
        let full = (0..20)
            .map(|index| {
                entry(
                    "comment",
                    100 + index,
                    &format!("2026-09-10T06:{index:02}:00Z"),
                )
            })
            .collect::<Vec<_>>();
        let prior = json!({"state": "open", "recent_comments": full});
        let seen = json!({"recent_comments": [entry("comment", 50, "2026-09-10T05:30:00Z")]});
        assert!(watch_events(&watch(), Some(&prior), &seen, &prior).is_empty());
    }

    #[test]
    fn required_checks_wake_on_a_move_into_pass_or_fail_and_a_new_head_starts_over() {
        let at = |head: &str, state: &str| {
            json!({"state": "open", "head_sha": head, "required_checks": {"state": state, "source": "rules",
                "checks": ["build"], "failed": if state == "fail" { json!(["build"]) } else { json!([]) }}})
        };
        let checks = |facts: &Value| {
            watch_events(&watch(), None, &json!({}), facts);
            facts["required_checks"].clone()
        };
        let moved =
            |before: &Value, after: &Value| watch_events(&watch(), Some(before), &json!({}), after);
        assert_eq!(
            moved(&at("a", "pending"), &at("a", "fail")),
            vec![WatchEvent::Checks(checks(&at("a", "fail")))]
        );
        assert!(moved(&at("a", "fail"), &at("a", "fail")).is_empty());
        assert!(moved(&at("a", "fail"), &at("a", "pending")).is_empty());
        assert_eq!(moved(&at("a", "pending"), &at("a", "pass")).len(), 1);
        // A new head already green is news; the old head's green is not carried over.
        assert_eq!(moved(&at("a", "pass"), &at("b", "pass")).len(), 1);
        assert!(moved(&at("a", "pass"), &at("b", "pending")).is_empty());
        assert!(moved(&at("a", "pending"), &at("a", "none")).is_empty());
    }

    #[test]
    fn a_close_or_merge_ends_the_watch() {
        let open = json!({"state": "open"});
        assert_eq!(
            watch_events(
                &watch(),
                Some(&open),
                &json!({}),
                &json!({"state": "closed", "merged": true})
            ),
            vec![WatchEvent::Ended { merged: true }]
        );
        assert_eq!(
            watch_events(
                &watch(),
                Some(&open),
                &json!({}),
                &json!({"state": "closed"})
            ),
            vec![WatchEvent::Ended { merged: false }]
        );
        assert!(watch_events(&watch(), None, &json!({}), &json!({"state": "closed"})).is_empty());
    }

    #[test]
    fn a_wake_says_who_did_what_where_and_names_its_comment() {
        let thread = ThreadRef::parse("acme/garden#12").unwrap();
        let facts = json!({"url": "https://github.com/acme/garden/pull/12", "title": "Add the seed catalog",
            "state": "open", "head_sha": "3f2a1bc0000"});
        let subject = thread.watch("agent/example/planner");
        let comment = wake(
            &subject,
            &watch(),
            &thread,
            &facts,
            &WatchEvent::Comment(entry("comment", 123456, "2026-09-10T05:02:00Z")),
            "claim-1",
            None,
        )
        .unwrap();
        assert_eq!(comment.title, "@fern commented on acme/garden#12");
        assert_eq!(
            comment.content,
            "@fern commented on acme/garden#12 \"Add the seed catalog\" (pull request, open)\n\
             https://github.com/acme/garden/pull/12#issuecomment-123456\n\n\
             To stop watching: st gh unwatch acme/garden#12"
        );
        let mut message = MessageView {
            subject: comment.subject.clone(),
            from: "daemon/node".into(),
            to: "agent/example/planner".into(),
            content: comment.content.clone(),
            status: "sent".into(),
            title: Some(comment.title.clone()),
            in_reply_to: None,
            tags: comment.tags.clone(),
            attachments: Vec::new(),
            created_index: 1,
        };
        assert_eq!(
            named_object(&message),
            Some(("acme/garden".into(), "comment".into(), 123456))
        );
        excerpts().lock().unwrap().insert(
            ("acme/garden".into(), "comment".into(), 123456),
            quote("Here is the seed list.\nIt has two parts."),
        );
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(add_excerpts(std::slice::from_mut(&mut message)));
        assert_eq!(
            message.content,
            "@fern commented on acme/garden#12 \"Add the seed catalog\" (pull request, open)\n\
             https://github.com/acme/garden/pull/12#issuecomment-123456\n\
             > Here is the seed list.\n> It has two parts.\n\n\
             To stop watching: st gh unwatch acme/garden#12"
        );

        let failed = wake(
            &subject,
            &watch(),
            &thread,
            &facts,
            &WatchEvent::Checks(json!({"state": "fail", "source": "rules", "checks": ["build", "test"], "failed": ["build"]})),
            "claim-2",
            None,
        )
        .unwrap();
        assert_eq!(
            failed.content.lines().next().unwrap(),
            "Required checks failed on acme/garden#12 \"Add the seed catalog\" at 3f2a1bc: build"
        );
        let merged = wake(
            &subject,
            &watch(),
            &thread,
            &facts,
            &WatchEvent::Ended { merged: true },
            "claim-3",
            None,
        )
        .unwrap();
        assert!(
            merged.content.starts_with(
                "acme/garden#12 \"Add the seed catalog\" merged. This watch has ended."
            )
        );
        // The same event from a replayed observation has the same subject.
        let again = wake(
            &subject,
            &watch(),
            &thread,
            &facts,
            &WatchEvent::Comment(entry("comment", 123456, "2026-09-10T05:02:00Z")),
            "claim-9",
            None,
        )
        .unwrap();
        assert_eq!(again.subject, comment.subject);
    }

    #[test]
    fn a_seats_post_in_flight_holds_only_its_wakes_on_that_thread() {
        let thread = ThreadRef::parse("acme/garden#12").unwrap();
        let message = |thread: &str| MessageView {
            subject: "message/watch-one".into(),
            from: "daemon/node".into(),
            to: "agent/example.planner".into(),
            content: String::new(),
            status: "sent".into(),
            title: None,
            in_reply_to: None,
            tags: vec![
                WATCH_TAG.into(),
                format!("{COMMENT_TAG}{thread}:comment:5:12"),
            ],
            attachments: Vec::new(),
            created_index: 1,
        };
        let post = PostInFlight::begin("agent/example.planner", &thread);
        assert!(post_in_flight(
            "agent/example.planner",
            &message("acme/garden")
        ));
        assert!(!post_in_flight(
            "agent/example.reviewer",
            &message("acme/garden")
        ));
        assert!(!post_in_flight(
            "agent/example.planner",
            &message("acme/orchard")
        ));
        drop(post);
        assert!(!post_in_flight(
            "agent/example.planner",
            &message("acme/garden")
        ));
    }

    #[test]
    fn an_excerpt_is_cut_at_a_word() {
        let long = "word ".repeat(200);
        let quoted = quote(&long).unwrap();
        assert!(quoted.chars().count() <= EXCERPT_CHARS + 4);
        assert!(quoted.ends_with("word …"));
        assert_eq!(quote("  "), None);
    }
}
