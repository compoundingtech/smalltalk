//! The `github.repository` provider: one observer per repository.
//!
//! One poll reads what changed in the repository since the observer's cursor and returns each
//! changed item with only the facts it read. The store merges each item into its own resource,
//! so a change to one item records only that item and the repository resource keeps only its
//! own facts. The data types an observer emits are its fields: `pull_requests`, `issues`,
//! `comments`, `reactions` and `mentions`.
//!
//! The REST reads are conditional, so an unchanged listing costs nothing and returns nothing to
//! record. Pull request heads, checks, reviews and merge-queue state need GraphQL, which has no
//! conditional request; that one query runs only when it can have something new to say.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::{
    GITHUB_AUTH_REMEDY, GithubListing, ObservationRequest, ProviderObservation, github_cache_for,
    github_client, github_graphql, github_json, github_listing,
};

/// The data types a repository observer can emit.
pub(crate) const DATA_TYPES: [&str; 5] = [
    "pull_requests",
    "issues",
    "comments",
    "reactions",
    "mentions",
];

/// How often a repository whose pull requests are all settled asks GraphQL again, for a review or
/// a check rerun that changed nothing the REST reads can see.
const SETTLED_PULL_REQUEST_CHECK: Duration = Duration::from_secs(15 * 60);

/// The least time between two GraphQL reads of one repository on this host, however often its
/// observers poll and whatever they saw change. Only a refresh asks sooner.
const PULL_REQUEST_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// The most comments and reviews each item keeps in `recent_comments`, newest last.
pub(crate) const RECENT_COMMENTS: usize = 20;

/// The most pages of a hundred items one REST listing reads in one poll.
const LISTING_PAGES: usize = 10;

/// The most open pull requests the GraphQL read pages through, a hundred at a time.
const PULL_REQUEST_PAGES: usize = 10;

/// The most mentions one item keeps, newest first.
const ITEM_MENTIONS: usize = 50;

/// How far behind the local clock a first watermark starts, for clock skew with GitHub.
const WATERMARK_SKEW: Duration = Duration::from_secs(5 * 60);

/// What a repository observer remembers between polls. Each watermark is the newest `updated_at`
/// a listing has returned, so a listing's URL stays the same, and its conditional request free,
/// until something changes. Each digest names the listing version whose items were already
/// returned, so an unchanged listing returns nothing to record. A cursor from an older build
/// names neither, and its first poll reads every open item.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RepositoryCursor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) items_since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) comments_since: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) seen: BTreeMap<String, String>,
}

impl RepositoryCursor {
    fn parse(cursor: Option<&str>) -> Self {
        cursor
            .and_then(|cursor| serde_json::from_str(cursor).ok())
            .unwrap_or_default()
    }

    /// Whether `listing` is the version whose items this observer already returned. Either way
    /// the cursor now names it.
    fn already_seen(&mut self, listing: &str, digest: String) -> bool {
        self.seen.insert(listing.to_owned(), digest.clone()) == Some(digest)
    }
}

/// When this host last asked GraphQL about a repository's open pull requests, what it answered,
/// and whether any of them was still settling: checks pending or a place in the merge queue.
/// `changed` says an observer saw a pull request change while the answer was too recent to ask
/// again, so the next read after the interval asks. Every observer of the repository on this host
/// shares the answer.
#[derive(Clone)]
struct PullRequestCheck {
    at: Instant,
    settling: bool,
    changed: bool,
    open: Arc<Vec<serde_json::Map<String, Value>>>,
}

fn pull_request_checks() -> &'static Mutex<HashMap<String, PullRequestCheck>> {
    static CHECKS: OnceLock<Mutex<HashMap<String, PullRequestCheck>>> = OnceLock::new();
    CHECKS.get_or_init(Default::default)
}

/// One item as this poll saw it. `pull` says which collection it belongs to. `listed` says that a
/// listing of items named it, which can tell whether it is new; an item seen only through a
/// comment or a reaction is never new.
#[derive(Default)]
struct Item {
    pull: bool,
    listed: bool,
    facts: serde_json::Map<String, Value>,
}

/// Mark an item created before the observer's watermark as not new.
fn mark_known(facts: &mut serde_json::Map<String, Value>, watermark: Option<&str>) {
    if let (Some(watermark), Some(created_at)) =
        (watermark, facts.get("created_at").and_then(Value::as_str))
        && created_at <= watermark
    {
        facts.insert("new".into(), Value::Bool(false));
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn listing_digest(listing: &GithubListing) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(&listing.versions).unwrap_or_default(),
    ))
}

fn lowercase(value: Option<&Value>) -> Value {
    value
        .and_then(Value::as_str)
        .map_or(Value::Null, |text| Value::String(text.to_ascii_lowercase()))
}

fn body_digest(body: &str) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(body.as_bytes())))
}

/// The GitHub logins a body mentions, in order and without repeats. A mention is `@` followed by
/// a login, after the start of the text or a character that cannot be part of an address or a
/// word. A team (`@org/team`) is not a person.
pub(crate) fn mentioned_logins(body: &str) -> Vec<String> {
    let characters = body.char_indices().collect::<Vec<_>>();
    let mut logins = Vec::new();
    for (position, (index, character)) in characters.iter().enumerate() {
        if *character != '@' {
            continue;
        }
        let before = position.checked_sub(1).map(|before| characters[before].1);
        if before.is_some_and(|before| {
            before.is_alphanumeric() || matches!(before, '_' | '.' | '-' | '@' | '`' | '/')
        }) {
            continue;
        }
        let login = body[index + 1..]
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '-')
            .take(40)
            .collect::<String>();
        let after = body[index + 1 + login.len()..].chars().next();
        let valid = (1..=39).contains(&login.len())
            && !login.starts_with('-')
            && !login.ends_with('-')
            && after != Some('/');
        if valid && !logins.contains(&login) {
            logins.push(login);
        }
    }
    logins
}

fn mentions(
    body: Option<&str>,
    by: Option<&Value>,
    url: Option<&Value>,
    at: Option<&Value>,
) -> Vec<Value> {
    mentioned_logins(body.unwrap_or_default())
        .into_iter()
        .take(ITEM_MENTIONS)
        .map(|login| {
            json!({
                "login": login,
                "by": by.cloned().unwrap_or(Value::Null),
                "url": url.cloned().unwrap_or(Value::Null),
                "at": at.cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// One comment or review as `recent_comments` keeps it: what it is, its GitHub ID, who wrote it,
/// and when. Never its body.
fn recent_comment(
    kind: &str,
    id: Option<&Value>,
    author: Option<&Value>,
    at: Option<&Value>,
) -> Option<Value> {
    let id = id.and_then(Value::as_u64)?;
    let at = at.and_then(Value::as_str)?;
    Some(json!({
        "kind": kind,
        "id": id,
        "author": author.cloned().unwrap_or(Value::Null),
        "at": at,
    }))
}

/// What tells one comment or review from another: its kind and its GitHub ID.
pub(crate) fn recent_comment_key(entry: &Value) -> (String, u64) {
    (
        entry
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        entry.get("id").and_then(Value::as_u64).unwrap_or_default(),
    )
}

/// The comments and reviews `known` and `seen` name together, once each, oldest first. A seen
/// entry replaces a known one with the same key. `keep` cuts the oldest, so an unchanged item
/// always has the same list.
pub(crate) fn merge_recent_comments<'a>(
    known: impl IntoIterator<Item = &'a Value>,
    seen: impl IntoIterator<Item = &'a Value>,
    keep: Option<usize>,
) -> Vec<Value> {
    let mut entries = BTreeMap::<(String, u64), Value>::new();
    for entry in known.into_iter().chain(seen) {
        entries.insert(recent_comment_key(entry), entry.clone());
    }
    let mut entries = entries.into_values().collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        let at = |entry: &Value| entry.get("at").and_then(Value::as_str).map(str::to_owned);
        at(left)
            .cmp(&at(right))
            .then_with(|| recent_comment_key(left).cmp(&recent_comment_key(right)))
    });
    if let Some(keep) = keep {
        let cut = entries.len().saturating_sub(keep);
        entries.drain(..cut);
    }
    entries
}

fn reactions(value: Option<&Value>) -> Value {
    let Some(value) = value.and_then(Value::as_object) else {
        return Value::Null;
    };
    let mut summary = serde_json::Map::new();
    for name in [
        "total_count",
        "+1",
        "-1",
        "laugh",
        "hooray",
        "confused",
        "heart",
        "rocket",
        "eyes",
    ] {
        if let Some(count) = value
            .get(name)
            .and_then(Value::as_u64)
            .filter(|count| *count > 0)
        {
            summary.insert(name.into(), Value::from(count));
        }
    }
    Value::Object(summary)
}

fn insert(facts: &mut serde_json::Map<String, Value>, name: &str, value: Option<&Value>) {
    if let Some(value) = value.filter(|value| !value.is_null()) {
        facts.insert(name.into(), value.clone());
    }
}

/// An issue or pull request from the issues listing, with the facts each declared data type
/// reads. `watermark` is how far the observer had read before this poll: an item created before
/// it is not new.
fn listed_item(issue: &Value, fields: &BTreeSet<String>, watermark: Option<&str>) -> Item {
    let pull = issue.get("pull_request").is_some();
    let mut facts = serde_json::Map::new();
    insert(&mut facts, "number", issue.get("number"));
    insert(&mut facts, "url", issue.get("html_url"));
    insert(&mut facts, "title", issue.get("title"));
    insert(&mut facts, "author", issue.pointer("/user/login"));
    insert(&mut facts, "created_at", issue.get("created_at"));
    let state = issue.get("state").and_then(Value::as_str).unwrap_or("open");
    facts.insert("state".into(), Value::String(state.into()));
    if pull {
        insert(&mut facts, "draft", issue.get("draft"));
        if state != "open"
            && let Some(merged_at) = issue.pointer("/pull_request/merged_at")
        {
            facts.insert("merged".into(), Value::Bool(!merged_at.is_null()));
        }
    } else {
        // `state_reason` is null on an open issue that was never closed.
        facts.insert(
            "state_reason".into(),
            issue.get("state_reason").cloned().unwrap_or(Value::Null),
        );
    }
    if fields.contains("comments") {
        insert(&mut facts, "comments", issue.get("comments"));
    }
    if fields.contains("reactions") {
        facts.insert("reactions".into(), reactions(issue.get("reactions")));
    }
    if fields.contains("mentions") {
        let mentioned = mentions(
            issue.get("body").and_then(Value::as_str),
            issue.pointer("/user/login"),
            issue.get("html_url"),
            issue.get("created_at"),
        );
        if !mentioned.is_empty() {
            facts.insert("mentions".into(), Value::Array(mentioned));
        }
    }
    mark_known(&mut facts, watermark);
    Item {
        pull,
        listed: true,
        facts,
    }
}

/// One comment from the repository's comment listing, as what it adds to the item it belongs to.
/// A pull request's conversation comment links to `/pull/NUMBER`.
fn listed_comment(comment: &Value, fields: &BTreeSet<String>) -> Option<Item> {
    let number = comment
        .get("issue_url")
        .and_then(Value::as_str)?
        .rsplit('/')
        .next()?
        .parse::<u64>()
        .ok()?;
    let pull = comment
        .get("html_url")
        .and_then(Value::as_str)
        .is_some_and(|url| url.contains("/pull/"));
    let mut facts = serde_json::Map::from_iter([("number".into(), Value::from(number))]);
    if fields.contains("comments") {
        let mut last = json!({
            "id": comment.get("id").cloned().unwrap_or(Value::Null),
            "author": comment.pointer("/user/login").cloned().unwrap_or(Value::Null),
            "url": comment.get("html_url").cloned().unwrap_or(Value::Null),
            "created_at": comment.get("created_at").cloned().unwrap_or(Value::Null),
            "updated_at": comment.get("updated_at").cloned().unwrap_or(Value::Null),
            "body_digest": body_digest(comment.get("body").and_then(Value::as_str).unwrap_or_default()),
        });
        if fields.contains("reactions") {
            last["reactions"] = reactions(comment.get("reactions"));
        }
        facts.insert("last_comment".into(), last);
        if let Some(entry) = recent_comment(
            "comment",
            comment.get("id"),
            comment.pointer("/user/login"),
            comment.get("created_at"),
        ) {
            facts.insert("recent_comments".into(), Value::Array(vec![entry]));
        }
    }
    if fields.contains("mentions") {
        let mentioned = mentions(
            comment.get("body").and_then(Value::as_str),
            comment.pointer("/user/login"),
            comment.get("html_url"),
            comment.get("created_at"),
        );
        if !mentioned.is_empty() {
            facts.insert("mentions".into(), Value::Array(mentioned));
        }
    }
    (facts.len() > 1).then_some(Item {
        pull,
        listed: false,
        facts,
    })
}

const OPEN_PULL_REQUESTS: &str = "query($owner: String!, $name: String!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequests(states: OPEN, first: 100, after: $after) {
      pageInfo { hasNextPage endCursor }
      nodes {
        number title url isDraft headRefOid headRefName baseRefName createdAt
        author { login }
        reviewDecision
        mergeQueueEntry { state position }
        latestReviews(first: 20) { nodes { author { login } state submittedAt commit { oid } } }
        reviews(last: 20) { nodes { databaseId author { login } state submittedAt } }
        commits(last: 1) { nodes { commit { statusCheckRollup { state
          contexts(first: 100) { nodes {
            __typename
            ... on CheckRun { name status conclusion checkSuite { app { databaseId } } }
            ... on StatusContext { context state }
          } }
        } } } }
      }
    }
  }
}";

/// A check that a base branch requires before merging: its name, and the app that must report it
/// when the rule names one.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RequiredCheck {
    context: String,
    app: Option<u64>,
}

/// How one check on a head stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckOutcome {
    Pending,
    Passed,
    Failed,
}

/// One check run or commit status on a head: its name, the app that reported a check run, and how
/// it stands. Success, neutral and skipped pass; any other finished result fails.
fn check_outcome(context: &Value) -> (String, Option<u64>, CheckOutcome) {
    let upper = |name: &str| {
        context
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_uppercase()
    };
    match context.get("__typename").and_then(Value::as_str) {
        Some("StatusContext") => (
            context
                .get("context")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            None,
            match upper("state").as_str() {
                "SUCCESS" => CheckOutcome::Passed,
                "PENDING" | "EXPECTED" | "" => CheckOutcome::Pending,
                _ => CheckOutcome::Failed,
            },
        ),
        _ => (
            context
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            context
                .pointer("/checkSuite/app/databaseId")
                .and_then(Value::as_u64),
            if upper("status") != "COMPLETED" {
                CheckOutcome::Pending
            } else if matches!(
                upper("conclusion").as_str(),
                "SUCCESS" | "NEUTRAL" | "SKIPPED"
            ) {
                CheckOutcome::Passed
            } else {
                CheckOutcome::Failed
            },
        ),
    }
}

/// How the checks a head must pass stand together. With rules that require checks, only those
/// count: each must be present, and when a rule names an app only a check run that app reported
/// counts, as GitHub requires; a commit status carries no app, so it counts only for a rule that
/// names none.
/// Without them, because the base requires none or its rules could not be read, every check on
/// the head counts. `fail` needs one counted check finished and failing, `pass` needs every one
/// finished and passing, and anything else is `pending`; `none` says there is nothing to count.
fn required_checks(contexts: &[Value], required: Option<&[RequiredCheck]>) -> Value {
    let checks = contexts.iter().map(check_outcome).collect::<Vec<_>>();
    // Every check and status that shares a counted name must pass, so one that finished and
    // failed decides it, whatever else of that name still runs.
    let combine = |outcomes: &mut dyn Iterator<Item = CheckOutcome>| {
        let outcomes = outcomes.collect::<Vec<_>>();
        if outcomes.contains(&CheckOutcome::Failed) {
            CheckOutcome::Failed
        } else if outcomes.is_empty() || outcomes.contains(&CheckOutcome::Pending) {
            CheckOutcome::Pending
        } else {
            CheckOutcome::Passed
        }
    };
    let (source, counted) = match required.filter(|required| !required.is_empty()) {
        Some(required) => (
            "rules",
            required
                .iter()
                .map(|rule| {
                    let outcome = combine(&mut checks.iter().filter_map(|(name, app, outcome)| {
                        (*name == rule.context && (rule.app.is_none() || *app == rule.app))
                            .then_some(*outcome)
                    }));
                    (rule.context.clone(), outcome)
                })
                .collect::<BTreeMap<_, _>>(),
        ),
        None => {
            let mut by_name = BTreeMap::<String, Vec<CheckOutcome>>::new();
            for (name, _, outcome) in &checks {
                by_name.entry(name.clone()).or_default().push(*outcome);
            }
            (
                "all",
                by_name
                    .into_iter()
                    .map(|(name, outcomes)| (name, combine(&mut outcomes.into_iter())))
                    .collect(),
            )
        }
    };
    let failed = counted
        .iter()
        .filter(|(_, outcome)| **outcome == CheckOutcome::Failed)
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    let state = if counted.is_empty() {
        "none"
    } else if !failed.is_empty() {
        "fail"
    } else if counted
        .values()
        .any(|outcome| *outcome == CheckOutcome::Pending)
    {
        "pending"
    } else {
        "pass"
    };
    json!({
        "state": state,
        "source": source,
        "checks": counted.keys().collect::<Vec<_>>(),
        "failed": failed,
    })
}

/// An open pull request from the GraphQL read: its head, checks, reviews and merge-queue place,
/// and how the checks its base requires stand. Lists are sorted, so the same state is always the
/// same facts. `required` is what the base's rules require; `None` when they could not be read.
fn open_pull_request(
    node: &Value,
    required: Option<&[RequiredCheck]>,
) -> serde_json::Map<String, Value> {
    let mut facts = serde_json::Map::new();
    insert(&mut facts, "number", node.get("number"));
    insert(&mut facts, "url", node.get("url"));
    insert(&mut facts, "title", node.get("title"));
    insert(&mut facts, "author", node.pointer("/author/login"));
    insert(&mut facts, "created_at", node.get("createdAt"));
    insert(&mut facts, "head", node.get("headRefOid"));
    insert(&mut facts, "branch", node.get("headRefName"));
    insert(&mut facts, "base_branch", node.get("baseRefName"));
    facts.insert("state".into(), Value::String("open".into()));
    facts.insert(
        "draft".into(),
        Value::Bool(node.get("isDraft").and_then(Value::as_bool) == Some(true)),
    );
    let rollup = node.pointer("/commits/nodes/0/commit/statusCheckRollup");
    facts.insert(
        "checks_state".into(),
        lowercase(rollup.and_then(|rollup| rollup.get("state"))),
    );
    let contexts = rollup
        .and_then(|rollup| rollup.pointer("/contexts/nodes"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    facts.insert(
        "required_checks".into(),
        required_checks(&contexts, required),
    );
    let mut checks = contexts
        .iter()
        .map(
            |context| match context.get("__typename").and_then(Value::as_str) {
                Some("StatusContext") => {
                    let state = lowercase(context.get("state"));
                    let pending = matches!(state.as_str(), Some("pending" | "expected"));
                    json!({
                        "name": context.get("context").cloned().unwrap_or(Value::Null),
                        "status": if pending { "pending" } else { "completed" },
                        "conclusion": if pending { Value::Null } else { state },
                    })
                }
                _ => json!({
                    "name": context.get("name").cloned().unwrap_or(Value::Null),
                    "status": lowercase(context.get("status")),
                    "conclusion": lowercase(context.get("conclusion")),
                }),
            },
        )
        .collect::<Vec<_>>();
    checks.sort_by_key(|check| check.to_string());
    facts.insert("checks".into(), Value::Array(checks));
    facts.insert(
        "review_decision".into(),
        lowercase(node.get("reviewDecision")),
    );
    let mut reviews = node
        .pointer("/latestReviews/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|review| {
            json!({
                "author": review.pointer("/author/login").cloned().unwrap_or(Value::Null),
                "state": lowercase(review.get("state")),
                "commit": review.pointer("/commit/oid").cloned().unwrap_or(Value::Null),
                "submitted_at": review.get("submittedAt").cloned().unwrap_or(Value::Null),
            })
        })
        .collect::<Vec<_>>();
    reviews.sort_by_key(|review| review.to_string());
    facts.insert("reviews".into(), Value::Array(reviews));
    // Every submitted review, each once, for `recent_comments`. A pending review is a draft only
    // its author can see.
    let submitted = node
        .pointer("/reviews/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|review| review.get("state").and_then(Value::as_str) != Some("PENDING"))
        .filter_map(|review| {
            let mut entry = recent_comment(
                "review",
                review.get("databaseId"),
                review.pointer("/author/login"),
                review.get("submittedAt"),
            )?;
            entry["state"] = lowercase(review.get("state"));
            Some(entry)
        })
        .collect::<Vec<_>>();
    facts.insert(
        "recent_comments".into(),
        Value::Array(merge_recent_comments([], &submitted, None)),
    );
    facts.insert(
        "merge_queue".into(),
        node.get("mergeQueueEntry")
            .filter(|entry| !entry.is_null())
            .map_or(Value::Null, |entry| {
                json!({
                    "state": lowercase(entry.get("state")),
                    "position": entry.get("position").cloned().unwrap_or(Value::Null),
                })
            }),
    );
    facts
}

/// A pull request is settling while a check has not finished, while a check its base requires
/// has failed, or while it waits in the merge queue. A rerun of a failed check changes nothing the
/// REST reads see, so only reading again finds it running and then passing.
fn settling(facts: &serde_json::Map<String, Value>) -> bool {
    matches!(
        facts.get("checks_state").and_then(Value::as_str),
        Some("pending" | "expected")
    ) || facts
        .get("required_checks")
        .and_then(|checks| checks.get("state"))
        .and_then(Value::as_str)
        == Some("fail")
        || facts
            .get("merge_queue")
            .is_some_and(|entry| !entry.is_null())
}

fn merge(items: &mut BTreeMap<u64, Item>, item: Item) {
    let Some(number) = item.facts.get("number").and_then(Value::as_u64) else {
        return;
    };
    let entry = items.entry(number).or_default();
    entry.pull |= item.pull;
    entry.listed |= item.listed;
    for (name, value) in item.facts {
        match name.as_str() {
            // A comment read after the item names whether the item is new only by its absence.
            "new" if !item.listed => {}
            "last_comment"
                if entry
                    .facts
                    .get("last_comment")
                    .is_some_and(|known| !crate::store::newer_comment(&value, known)) => {}
            "mentions" => {
                if let Some(Value::Array(known)) = entry.facts.get_mut("mentions") {
                    known.extend(value.as_array().cloned().unwrap_or_default());
                } else {
                    entry.facts.insert(name, value);
                }
            }
            // A pull request's reviews and its comments arrive from different reads.
            "recent_comments" => {
                let merged = merge_recent_comments(
                    entry
                        .facts
                        .get("recent_comments")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten(),
                    value.as_array().into_iter().flatten(),
                    None,
                );
                entry.facts.insert(name, Value::Array(merged));
            }
            _ => {
                entry.facts.insert(name, value);
            }
        }
    }
}

fn watermark_now() -> String {
    let at = SystemTime::now()
        .checked_sub(WATERMARK_SKEW)
        .unwrap_or(UNIX_EPOCH);
    chrono::DateTime::<chrono::Utc>::from(at)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn newest_updated_at<'a>(values: impl IntoIterator<Item = &'a Value>) -> Option<String> {
    values
        .into_iter()
        .filter_map(|value| value.get("updated_at").and_then(Value::as_str))
        .max()
        .map(str::to_owned)
}

pub(super) async fn observe_at(
    request: ObservationRequest,
    api_base: &str,
    token: Option<&str>,
) -> Result<ProviderObservation> {
    let cache_for = github_cache_for(&request);
    let token = token
        .filter(|value| !value.trim().is_empty())
        .context(GITHUB_AUTH_REMEDY)?;
    let (owner, repository) = request
        .locator
        .split_once('/')
        .context("a GitHub repository locator needs OWNER/REPO")?;
    anyhow::ensure!(
        !owner.is_empty() && !repository.is_empty() && !repository.contains('/'),
        "a GitHub repository locator needs OWNER/REPO"
    );
    let fields = &request.fields;
    let client = github_client();
    let base = format!("{api_base}/repos/{owner}/{repository}");
    // A renamed repository answers through a redirect. Its numeric ID proves that the locator
    // still names the repository whose items were observed before.
    let repository_id = github_json(&client, base.clone(), token, cache_for)
        .await?
        .value
        .get("id")
        .and_then(Value::as_u64)
        .context("the GitHub repository response has no numeric ID")?;
    if let Some(previous_id) = request
        .previous_facts
        .as_ref()
        .and_then(|value| value.get("repository_id"))
        .and_then(Value::as_u64)
    {
        anyhow::ensure!(
            previous_id == repository_id,
            "the locator now names GitHub repository {repository_id}, not the observed repository {previous_id}"
        );
    }
    let mut cursor = RepositoryCursor::parse(request.cursor.as_deref());
    // How far the observer had read before this poll. Without one, this poll reads every open
    // item once; from then on it reads only what changed.
    let watermark = cursor.items_since.clone();
    let mut items = BTreeMap::<u64, Item>::new();
    let mut more = false;
    let mut changed_open_pull = false;
    // The open pull requests the REST reads named. GraphQL must know each one before its answer
    // stands in, or a new pull request would be recorded without the head a review needs.
    let mut listed_open_pulls = BTreeSet::new();

    if fields
        .iter()
        .any(|field| DATA_TYPES.contains(&field.as_str()))
    {
        let listing = match &watermark {
            None => {
                github_listing(
                    &client,
                    format!("{base}/issues?state=open&per_page=100"),
                    token,
                    cache_for,
                    LISTING_PAGES,
                    false,
                )
                .await?
            }
            Some(since) => github_listing(
                &client,
                format!(
                    "{base}/issues?state=all&sort=updated&direction=asc&since={since}&per_page=100"
                ),
                token,
                cache_for,
                LISTING_PAGES,
                true,
            )
            .await?,
        };
        more |= listing.truncated;
        if watermark.is_none() {
            // The open listing a first poll reads already names each open item's reactions.
            cursor
                .seen
                .insert("open-reactions".into(), listing_digest(&listing));
        }
        if !cursor.already_seen("items", listing_digest(&listing)) {
            for issue in &listing.values {
                let item = listed_item(issue, fields, watermark.as_deref());
                if item.pull && item.facts.get("state").and_then(Value::as_str) == Some("open") {
                    changed_open_pull = true;
                    listed_open_pulls.extend(item.facts.get("number").and_then(Value::as_u64));
                }
                merge(&mut items, item);
            }
        }
        if watermark.is_none() {
            // Builds before item resources kept each pull request in the repository facts. One
            // that was open there and is missing from the open listing has closed.
            for number in request
                .previous_facts
                .as_ref()
                .and_then(|facts| facts.get("pull_requests"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|legacy| {
                    legacy
                        .get("state")
                        .and_then(Value::as_str)
                        .unwrap_or("open")
                        == "open"
                })
                .filter_map(|legacy| legacy.get("number").and_then(Value::as_u64))
            {
                let listed = listing
                    .values
                    .iter()
                    .any(|issue| issue.get("number").and_then(Value::as_u64) == Some(number));
                if !listed {
                    merge(
                        &mut items,
                        Item {
                            pull: true,
                            listed: false,
                            facts: serde_json::Map::from_iter([
                                ("number".into(), Value::from(number)),
                                ("state".into(), Value::String("closed".into())),
                            ]),
                        },
                    );
                }
            }
        }
        cursor.items_since = newest_updated_at(&listing.values)
            .into_iter()
            .chain(watermark.clone())
            .max()
            .or_else(|| Some(watermark_now()));

        // A reaction changes no `updated_at`, so reactions on open items come from the open
        // listing. A first poll already read it.
        if fields.contains("reactions") && watermark.is_some() {
            let open = github_listing(
                &client,
                format!("{base}/issues?state=open&per_page=100"),
                token,
                cache_for,
                LISTING_PAGES,
                false,
            )
            .await?;
            if !cursor.already_seen("open-reactions", listing_digest(&open)) {
                for issue in &open.values {
                    let mut facts = serde_json::Map::new();
                    insert(&mut facts, "number", issue.get("number"));
                    facts.insert("reactions".into(), reactions(issue.get("reactions")));
                    merge(
                        &mut items,
                        Item {
                            pull: issue.get("pull_request").is_some(),
                            listed: false,
                            facts,
                        },
                    );
                }
            }
        }

        if fields.contains("comments") || fields.contains("mentions") {
            match cursor.comments_since.clone() {
                // The first poll starts the comment watermark now instead of reading every comment
                // the repository ever had.
                None => cursor.comments_since = Some(watermark_now()),
                Some(since) => {
                    let comments = github_listing(
                        &client,
                        format!(
                            "{base}/issues/comments?sort=updated&direction=asc&since={since}&per_page=100"
                        ),
                        token,
                        cache_for,
                        LISTING_PAGES,
                        true,
                    )
                    .await?;
                    more |= comments.truncated;
                    if !cursor.already_seen("comments", listing_digest(&comments)) {
                        for item in comments
                            .values
                            .iter()
                            .filter_map(|comment| listed_comment(comment, fields))
                        {
                            merge(&mut items, item);
                        }
                    }
                    cursor.comments_since = newest_updated_at(&comments.values)
                        .into_iter()
                        .chain(Some(since))
                        .max();
                }
            }
            // Reactions on comments come from the newest hundred comments.
            if fields.contains("reactions") && fields.contains("comments") {
                let recent = github_listing(
                    &client,
                    format!("{base}/issues/comments?sort=updated&direction=desc&per_page=100"),
                    token,
                    cache_for,
                    1,
                    true,
                )
                .await?;
                if !cursor.already_seen("recent-comments", listing_digest(&recent)) {
                    for mut item in recent
                        .values
                        .iter()
                        .filter_map(|comment| listed_comment(comment, fields))
                    {
                        item.facts.remove("mentions");
                        merge(&mut items, item);
                    }
                }
            }
        }
    }

    if fields.contains("pull_requests") {
        let open = open_pull_requests(
            &client,
            api_base,
            token,
            cache_for,
            &request.locator,
            (owner, repository),
            request.refresh,
            changed_open_pull || !cursor.seen.contains_key("pull-requests"),
            &listed_open_pulls,
        )
        .await?;
        let digest = hex::encode(Sha256::digest(
            serde_json::to_vec(open.as_slice()).unwrap_or_default(),
        ));
        if !cursor.already_seen("pull-requests", digest) {
            for facts in open.iter() {
                let mut facts = facts.clone();
                // Every observer of the repository on this host shares the answer; reviews belong
                // to `recent_comments` only for an observer that emits comments.
                if !fields.contains("comments") {
                    facts.remove("recent_comments");
                }
                mark_known(&mut facts, watermark.as_deref());
                merge(
                    &mut items,
                    Item {
                        pull: true,
                        listed: true,
                        facts,
                    },
                );
            }
        }
    }

    let mut facts =
        serde_json::Map::from_iter([("repository_id".to_owned(), Value::from(repository_id))]);
    let mut pulls = Vec::new();
    let mut issues = Vec::new();
    for (_, item) in items {
        let mut item_facts = item.facts;
        if !item.listed {
            item_facts.insert("new".into(), Value::Bool(false));
        }
        if let Some(Value::Array(mentioned)) = item_facts.get_mut("mentions") {
            mentioned.truncate(ITEM_MENTIONS);
        }
        // An observer that declared only one collection emits only that one. Comments,
        // reactions and mentions alone belong to both.
        if item.pull && (fields.contains("pull_requests") || !fields.contains("issues")) {
            pulls.push(Value::Object(item_facts));
        } else if !item.pull && (fields.contains("issues") || !fields.contains("pull_requests")) {
            issues.push(Value::Object(item_facts));
        }
    }
    if !pulls.is_empty() {
        facts.insert("pull_requests".into(), Value::Array(pulls));
    }
    if !issues.is_empty() {
        facts.insert("issues".into(), Value::Array(issues));
    }
    Ok(ProviderObservation {
        facts: Value::Object(facts),
        cursor: Some(serde_json::to_string(&cursor)?),
        // A listing longer than one read continues at once.
        next_check_unix_ms: now_ms().saturating_add(if more {
            super::PROVIDER_CONTINUE_MS
        } else {
            300_000
        }),
    })
}

/// The repository's open pull requests as GraphQL last answered on this host. GraphQL has no
/// conditional request, so it is asked again only when `changed` says the REST reads saw an open
/// pull request change, while a pull request is settling, or once the last answer is old, and
/// never sooner than a minute after the last read unless `refresh` asks or the REST reads name
/// an open pull request (`listed`) the last answer lacks: a new pull request's first record
/// carries its head, since only a head that differs from a known one asks for a review. A change
/// seen within that minute is remembered, so the first read after it asks.
#[allow(clippy::too_many_arguments)]
async fn open_pull_requests(
    client: &reqwest::Client,
    api_base: &str,
    token: &str,
    cache_for: Duration,
    locator: &str,
    (owner, repository): (&str, &str),
    refresh: bool,
    changed: bool,
    listed: &BTreeSet<u64>,
) -> Result<Arc<Vec<serde_json::Map<String, Value>>>> {
    let key = format!("{api_base} {locator}");
    let previous = {
        let mut checks = pull_request_checks()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let previous = checks.get_mut(&key);
        if let Some(previous) = previous {
            previous.changed |= changed;
            Some(previous.clone())
        } else {
            None
        }
    };
    if let Some(previous) = previous.filter(|previous| {
        !refresh
            && listed.iter().all(|number| {
                previous
                    .open
                    .iter()
                    .any(|facts| facts.get("number").and_then(Value::as_u64) == Some(*number))
            })
            && (previous.at.elapsed() < PULL_REQUEST_CHECK_INTERVAL
                || (!previous.changed
                    && !previous.settling
                    && previous.at.elapsed() < SETTLED_PULL_REQUEST_CHECK))
    }) {
        return Ok(previous.open);
    }
    let mut nodes = Vec::new();
    let mut after = Value::Null;
    for page in 0.. {
        anyhow::ensure!(
            page < PULL_REQUEST_PAGES,
            "the repository has more than {} open pull requests",
            PULL_REQUEST_PAGES * 100
        );
        let data = github_graphql(
            client,
            api_base,
            token,
            OPEN_PULL_REQUESTS,
            json!({"owner": owner, "name": repository, "after": after}),
        )
        .await?;
        let connection = data
            .pointer("/repository/pullRequests")
            .context("the GitHub GraphQL response has no pull requests")?;
        nodes.extend(
            connection
                .get("nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
        if connection.pointer("/pageInfo/hasNextPage") != Some(&Value::Bool(true)) {
            break;
        }
        after = connection
            .pointer("/pageInfo/endCursor")
            .cloned()
            .unwrap_or(Value::Null);
    }
    let mut required = BTreeMap::<String, Option<Vec<RequiredCheck>>>::new();
    for node in &nodes {
        if let Some(base) = node.get("baseRefName").and_then(Value::as_str)
            && !required.contains_key(base)
        {
            let checks = required_by_base(
                client,
                api_base,
                token,
                cache_for,
                (owner, repository),
                base,
            )
            .await?;
            required.insert(base.to_owned(), checks);
        }
    }
    let mut open = nodes
        .iter()
        .map(|node| {
            let required = node
                .get("baseRefName")
                .and_then(Value::as_str)
                .and_then(|base| required.get(base))
                .and_then(Option::as_deref);
            open_pull_request(node, required)
        })
        .collect::<Vec<_>>();
    open.sort_by_key(|facts| facts.get("number").and_then(Value::as_u64));
    let open = Arc::new(open);
    pull_request_checks()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            key,
            PullRequestCheck {
                at: Instant::now(),
                settling: open.iter().any(settling),
                changed: false,
                open: open.clone(),
            },
        );
    Ok(open)
}

/// The checks a base branch requires before merging: those its rulesets name and those its
/// classic branch protection names, read with read access and conditionally. GitHub may refuse
/// to show either (a 403 or a 404); the other still counts, and `None` says it showed neither, so
/// the caller counts every check instead. One unreadable rule must never stop the observer. A
/// rate limit, a rejected token, or a failed connection fails the read as any other read does,
/// and the observer tries again.
async fn required_by_base(
    client: &reqwest::Client,
    api_base: &str,
    token: &str,
    cache_for: Duration,
    (owner, repository): (&str, &str),
    base: &str,
) -> Result<Option<Vec<RequiredCheck>>> {
    let unreadable = |error: &anyhow::Error| {
        error.downcast_ref::<super::ProviderForbidden>().is_some()
            || error
                .downcast_ref::<reqwest::Error>()
                .and_then(reqwest::Error::status)
                == Some(reqwest::StatusCode::NOT_FOUND)
    };
    let repository_base = format!("{api_base}/repos/{owner}/{repository}");
    let mut required = Vec::new();
    let mut readable = false;
    match github_json(
        client,
        format!("{repository_base}/rules/branches/{base}"),
        token,
        cache_for,
    )
    .await
    {
        Ok(rules) => {
            readable = true;
            for rule in rules.value.as_array().into_iter().flatten().filter(|rule| {
                rule.get("type").and_then(Value::as_str) == Some("required_status_checks")
            }) {
                for check in rule
                    .pointer("/parameters/required_status_checks")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(context) = check.get("context").and_then(Value::as_str) {
                        required.push(RequiredCheck {
                            context: context.to_owned(),
                            app: check.get("integration_id").and_then(Value::as_u64),
                        });
                    }
                }
            }
        }
        Err(error) if unreadable(&error) => {}
        Err(error) => return Err(error),
    }
    match github_json(
        client,
        format!("{repository_base}/branches/{base}"),
        token,
        cache_for,
    )
    .await
    {
        Ok(branch) => {
            readable = true;
            let checks = branch.value.pointer("/protection/required_status_checks");
            let named = checks
                .and_then(|checks| checks.get("checks"))
                .and_then(Value::as_array)
                .filter(|named| !named.is_empty());
            match named {
                Some(named) => required.extend(named.iter().filter_map(|check| {
                    Some(RequiredCheck {
                        context: check.get("context")?.as_str()?.to_owned(),
                        app: check.get("app_id").and_then(Value::as_u64),
                    })
                })),
                None => required.extend(
                    checks
                        .and_then(|checks| checks.get("contexts"))
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(|context| RequiredCheck {
                            context: context.to_owned(),
                            app: None,
                        }),
                ),
            }
        }
        Err(error) if unreadable(&error) => {}
        Err(error) => return Err(error),
    }
    required.sort();
    required.dedup();
    Ok(readable.then_some(required))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{github_usage_report, spend_as};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// A GitHub that answers each path from a table, revalidates with ETags, answers GraphQL
    /// with one fixed body, and keeps every request it received.
    #[derive(Clone, Default)]
    struct FakeGithub {
        routes: Arc<Mutex<HashMap<String, (String, String)>>>,
        refused: Arc<Mutex<HashMap<String, String>>>,
        graphql: Arc<Mutex<String>>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl FakeGithub {
        async fn start() -> (Self, String) {
            let github = Self::default();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            // cargo test shares the production caches between tests. A recycled TCP port
            // must not let a new fixture reuse another fixture's REST or GraphQL state.
            let scope = format!("/fixture/{}", uuid::Uuid::now_v7());
            let base = format!("http://{}{scope}", listener.local_addr().unwrap());
            let server = github.clone();
            tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let server = server.clone();
                    let scope = scope.clone();
                    tokio::spawn(async move {
                        let mut request = Vec::new();
                        let mut chunk = [0_u8; 4096];
                        let header_end = loop {
                            let size = stream.read(&mut chunk).await.unwrap();
                            request.extend_from_slice(&chunk[..size]);
                            if let Some(end) =
                                request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                            {
                                break end + 4;
                            }
                            if size == 0 {
                                return;
                            }
                        };
                        // Route tables and request assertions use paths relative to this
                        // fixture's API base; the client/cache still sees the unique scope.
                        let head = String::from_utf8_lossy(&request[..header_end]).replacen(&scope, "", 1);
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        while request.len() < header_end + length {
                            let size = stream.read(&mut chunk).await.unwrap();
                            request.extend_from_slice(&chunk[..size]);
                        }
                        let target = head.lines().next().unwrap().to_owned();
                        server.requests.lock().unwrap().push(head.clone());
                        let respond = |status: &str, etag: Option<&str>, body: &str| {
                            let etag =
                                etag.map_or(String::new(), |etag| format!("ETag: {etag}\r\n"));
                            format!(
                                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{etag}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                        };
                        let response = if target.starts_with("POST /graphql ") {
                            respond("200 OK", None, &server.graphql.lock().unwrap())
                        } else {
                            let path = target.split(' ').nth(1).unwrap().to_owned();
                            let route = server.routes.lock().unwrap().get(&path).cloned();
                            let refused = server.refused.lock().unwrap().get(&path).cloned();
                            match route {
                                _ if refused.is_some() => respond(
                                    refused.as_deref().unwrap(),
                                    None,
                                    r#"{"message":"Resource not accessible by integration"}"#,
                                ),
                                None => respond("404 Not Found", None, r#"{"message":"Not Found"}"#),
                                Some((etag, _))
                                    if head
                                        .to_ascii_lowercase()
                                        .contains(&format!("if-none-match: {}", etag.to_ascii_lowercase())) =>
                                {
                                    "HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
                                }
                                Some((etag, body)) => respond("200 OK", Some(&etag), &body),
                            }
                        };
                        stream.write_all(response.as_bytes()).await.unwrap();
                    });
                }
            });
            (github, base)
        }

        fn route(&self, path: &str, body: Value) {
            let body = body.to_string();
            let etag = format!(
                "\"{}\"",
                &hex::encode(Sha256::digest(body.as_bytes()))[..12]
            );
            self.routes
                .lock()
                .unwrap()
                .insert(path.to_owned(), (etag, body));
        }

        /// Answer `path` with `status`, such as `403 Forbidden`.
        fn refuse(&self, path: &str, status: &str) {
            self.refused
                .lock()
                .unwrap()
                .insert(path.to_owned(), status.to_owned());
        }

        fn open_pull_requests(&self, nodes: Value) {
            *self.graphql.lock().unwrap() = json!({"data": {"repository": {"pullRequests": {
                "pageInfo": {"hasNextPage": false, "endCursor": null},
                "nodes": nodes,
            }}}})
            .to_string();
        }

        fn take_requests(&self) -> Vec<String> {
            std::mem::take(&mut *self.requests.lock().unwrap())
                .into_iter()
                .map(|request| request.lines().next().unwrap_or_default().to_owned())
                .collect()
        }
    }

    /// Make this host's last GraphQL answer for `acme/garden` at `base` older by `by`.
    fn age_pull_request_check(base: &str, by: Duration) {
        let mut checks = pull_request_checks()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let check = checks
            .get_mut(&format!("{base} acme/garden"))
            .expect("GraphQL was read");
        check.at = check.at.checked_sub(by).expect("the clock reaches back");
    }

    fn request(
        fields: &[&str],
        cursor: Option<&str>,
        previous: Option<Value>,
    ) -> ObservationRequest {
        ObservationRequest {
            provider: "github.repository".into(),
            locator: "acme/garden".into(),
            fields: fields.iter().map(|field| (*field).to_owned()).collect(),
            cursor: cursor.map(str::to_owned),
            previous_facts: previous,
            // Each poll revalidates instead of reusing a cached response.
            every_ms: Some(0),
            refresh: false,
        }
    }

    fn items<'a>(facts: &'a Value, collection: &str) -> Vec<&'a Value> {
        facts
            .get(collection)
            .and_then(Value::as_array)
            .map(|items| items.iter().collect())
            .unwrap_or_default()
    }

    fn item<'a>(facts: &'a Value, collection: &str, number: u64) -> &'a Value {
        items(facts, collection)
            .into_iter()
            .find(|item| item["number"] == number)
            .unwrap_or_else(|| panic!("{collection} #{number} is missing from {facts}"))
    }

    fn open_pull(number: u64, head: &str, checks: &str) -> Value {
        json!({
            "number": number, "title": format!("Pull {number}"),
            "url": format!("https://github.com/acme/garden/pull/{number}"),
            "isDraft": false, "headRefOid": head, "headRefName": format!("agent/pull-{number}"),
            "createdAt": "2026-09-01T00:00:00Z", "author": {"login": "orchid-bot"},
            "reviewDecision": "REVIEW_REQUIRED",
            "mergeQueueEntry": null,
            "latestReviews": {"nodes": [{
                "author": {"login": "fern"}, "state": "COMMENTED",
                "submittedAt": "2026-09-01T01:00:00Z", "commit": {"oid": head},
            }]},
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": checks, "contexts": {"nodes": [
                {"__typename": "StatusContext", "context": "garden/ci", "state": checks},
                {"__typename": "CheckRun", "name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS"},
            ]}}}}]},
        })
    }

    fn issue(number: u64, updated_at: &str, extra: Value) -> Value {
        let mut issue = json!({
            "number": number, "title": format!("Item {number}"),
            "html_url": format!("https://github.com/acme/garden/issues/{number}"),
            "state": "open", "user": {"login": "fern"},
            "created_at": "2026-09-01T00:00:00Z", "updated_at": updated_at,
            "comments": 0, "body": "",
            "reactions": {"total_count": 0, "+1": 0},
        });
        for (name, value) in extra.as_object().unwrap() {
            issue[name] = value.clone();
        }
        issue
    }

    #[test]
    fn a_body_mentions_people_but_not_addresses_or_teams() {
        assert_eq!(
            mentioned_logins(
                "@fern can you look? cc @orchid-bot, not mail@example.com or @acme/gardeners. @fern again"
            ),
            vec!["fern", "orchid-bot"]
        );
        assert!(mentioned_logins("`@fern` @-dash @").is_empty());
    }

    #[tokio::test]
    async fn one_poll_emits_each_declared_data_type_and_an_unchanged_repository_emits_nothing() {
        let (github, base) = FakeGithub::start().await;
        github.route("/repos/acme/garden", json!({"id": 7}));
        github.route(
            "/repos/acme/garden/issues?state=open&per_page=100",
            json!([
                issue(1, "2026-09-02T00:00:00Z", json!({"comments": 2, "body": "@fern please triage", "reactions": {"total_count": 1, "+1": 1}})),
                issue(2, "2026-09-03T00:00:00Z", json!({"pull_request": {"merged_at": null}, "draft": false,
                    "html_url": "https://github.com/acme/garden/pull/2"})),
                issue(3, "2026-09-04T00:00:00Z", json!({"pull_request": {"merged_at": null}, "draft": true,
                    "html_url": "https://github.com/acme/garden/pull/3"})),
            ]),
        );
        let mut draft = open_pull(3, "cccccccccccccccccccccccccccccccccccccccc", "SUCCESS");
        draft["isDraft"] = Value::Bool(true);
        github.open_pull_requests(json!([
            open_pull(2, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "PENDING"),
            draft,
        ]));
        github.route(
            "/repos/acme/garden/issues/comments?sort=updated&direction=desc&per_page=100",
            json!([]),
        );
        let fields = [
            "pull_requests",
            "issues",
            "comments",
            "reactions",
            "mentions",
        ];
        let spender = "observer/orchid-one-poll".to_owned();
        let first = spend_as(
            spender.clone(),
            observe_at(request(&fields, None, None), &base, Some("orchid-token")),
        )
        .await
        .unwrap();
        let facts = &first.facts;
        assert_eq!(facts["repository_id"], 7);
        let issue = item(facts, "issues", 1);
        assert_eq!(issue["state"], "open");
        assert_eq!(issue["comments"], 2);
        assert_eq!(issue["reactions"], json!({"total_count": 1, "+1": 1}));
        assert_eq!(issue["mentions"][0]["login"], "fern");
        assert!(
            issue.get("new").is_none(),
            "a first poll leaves newness to the store"
        );
        let pull = item(facts, "pull_requests", 2);
        assert_eq!(pull["head"], "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(pull["branch"], "agent/pull-2");
        assert_eq!(pull["checks_state"], "pending");
        assert_eq!(
            pull["checks"],
            json!([
                {"name": "garden/ci", "status": "pending", "conclusion": null},
                {"name": "lint", "status": "completed", "conclusion": "success"},
            ])
        );
        assert_eq!(pull["review_decision"], "review_required");
        assert_eq!(pull["reviews"][0]["state"], "commented");
        assert_eq!(pull["merge_queue"], Value::Null);
        assert_eq!(item(facts, "pull_requests", 3)["draft"], true);
        assert_eq!(
            items(facts, "issues").len(),
            1,
            "pull requests are not issues"
        );
        let cursor = RepositoryCursor::parse(first.cursor.as_deref());
        assert_eq!(cursor.items_since.as_deref(), Some("2026-09-04T00:00:00Z"));
        assert!(cursor.comments_since.is_some());

        // Nothing changed. Every listing revalidates for free and returns nothing to record.
        // GraphQL is asked again only because a check is still pending, and not within a minute
        // of the last read.
        github.take_requests();
        let since = "/repos/acme/garden/issues?state=all&sort=updated&direction=asc&since=2026-09-04T00:00:00Z&per_page=100";
        github.route(since, json!([]));
        let comments = format!(
            "/repos/acme/garden/issues/comments?sort=updated&direction=asc&since={}&per_page=100",
            cursor.comments_since.as_deref().unwrap()
        );
        github.route(&comments, json!([]));
        let second = spend_as(
            spender.clone(),
            observe_at(
                request(&fields, first.cursor.as_deref(), Some(first.facts.clone())),
                &base,
                Some("orchid-token"),
            ),
        )
        .await
        .unwrap();
        let requests = github.take_requests();
        assert!(
            !requests
                .iter()
                .any(|request| request.starts_with("POST /graphql")),
            "a pending check waits a minute between reads: {requests:?}"
        );
        age_pull_request_check(&base, PULL_REQUEST_CHECK_INTERVAL);
        let third = spend_as(
            spender.clone(),
            observe_at(
                request(
                    &fields,
                    second.cursor.as_deref(),
                    Some(json!({"repository_id": 7})),
                ),
                &base,
                Some("orchid-token"),
            ),
        )
        .await
        .unwrap();
        assert_eq!(third.facts, json!({"repository_id": 7}));
        assert_eq!(third.cursor, second.cursor);
        let requests = github.take_requests();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("POST /graphql"))
                .count(),
            1,
            "a pending check asks GraphQL once the minute passed: {requests:?}"
        );

        // Once every check settles, an unchanged repository does not ask GraphQL at all.
        github.open_pull_requests(json!([open_pull(
            2,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "SUCCESS"
        )]));
        age_pull_request_check(&base, PULL_REQUEST_CHECK_INTERVAL);
        let settled = observe_at(
            request(&fields, third.cursor.as_deref(), None),
            &base,
            Some("orchid-token"),
        )
        .await
        .unwrap();
        assert_eq!(
            item(&settled.facts, "pull_requests", 2)["checks_state"],
            "success"
        );
        github.take_requests();
        let quiet = observe_at(
            request(&fields, settled.cursor.as_deref(), None),
            &base,
            Some("orchid-token"),
        )
        .await
        .unwrap();
        assert_eq!(quiet.facts, json!({"repository_id": 7}));
        let requests = github.take_requests();
        assert!(
            !requests
                .iter()
                .any(|request| request.starts_with("POST /graphql")),
            "{requests:?}"
        );
        assert!(requests.iter().all(|request| request.starts_with("GET ")));
        let spent = github_usage_report()
            .spenders
            .into_iter()
            .find(|report| report.spender == spender)
            .unwrap();
        assert!(
            spent.not_modified >= 4,
            "unchanged listings revalidate for free: not_modified={}, sent={}",
            spent.not_modified,
            spent.sent
        );
    }

    #[tokio::test]
    async fn a_later_poll_reads_only_what_changed_and_names_what_was_not_new() {
        let (github, base) = FakeGithub::start().await;
        github.route("/repos/acme/garden", json!({"id": 7}));
        let mut opened = open_pull(5, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "SUCCESS");
        opened["createdAt"] = json!("2026-09-10T03:00:00Z");
        github.open_pull_requests(json!([opened]));
        let since = "2026-09-10T00:00:00Z";
        github.route(
            &format!("/repos/acme/garden/issues?state=all&sort=updated&direction=asc&since={since}&per_page=100"),
            json!([
                // An old issue closed; a merged pull request; a new pull request; a new issue.
                issue(1, "2026-09-10T01:00:00Z", json!({"state": "closed", "state_reason": "completed"})),
                issue(4, "2026-09-10T02:00:00Z", json!({"state": "closed", "pull_request": {"merged_at": "2026-09-10T02:00:00Z"},
                    "html_url": "https://github.com/acme/garden/pull/4"})),
                issue(5, "2026-09-10T03:00:00Z", json!({"created_at": "2026-09-10T03:00:00Z", "pull_request": {"merged_at": null},
                    "draft": false, "html_url": "https://github.com/acme/garden/pull/5"})),
                issue(6, "2026-09-10T04:00:00Z", json!({"created_at": "2026-09-10T04:00:00Z"})),
            ]),
        );
        let comments_since = "2026-09-10T00:00:00Z";
        github.route(
            &format!("/repos/acme/garden/issues/comments?sort=updated&direction=asc&since={comments_since}&per_page=100"),
            json!([
                {"id": 91, "issue_url": "https://api.github.com/repos/acme/garden/issues/8",
                 "html_url": "https://github.com/acme/garden/pull/8#issuecomment-91",
                 "user": {"login": "fern"}, "body": "@orchid-bot should this merge?",
                 "created_at": "2026-09-10T05:00:00Z", "updated_at": "2026-09-10T05:00:00Z"},
            ]),
        );
        let cursor = serde_json::to_string(&RepositoryCursor {
            items_since: Some(since.into()),
            comments_since: Some(comments_since.into()),
            seen: BTreeMap::new(),
        })
        .unwrap();
        let observed = observe_at(
            request(
                &["pull_requests", "issues", "comments", "mentions"],
                Some(&cursor),
                Some(json!({"repository_id": 7})),
            ),
            &base,
            Some("orchid-token"),
        )
        .await
        .unwrap();
        let facts = &observed.facts;
        let closed = item(facts, "issues", 1);
        assert_eq!(
            (closed["state"].clone(), closed["state_reason"].clone()),
            (json!("closed"), json!("completed"))
        );
        assert_eq!(
            closed["new"], false,
            "an issue created before the watermark is not new"
        );
        let merged = item(facts, "pull_requests", 4);
        assert_eq!(
            (merged["state"].clone(), merged["merged"].clone()),
            (json!("closed"), json!(true))
        );
        let opened = item(facts, "pull_requests", 5);
        assert!(opened.get("new").is_none());
        assert_eq!(
            opened["head"], "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "GraphQL ran for the changed pull request"
        );
        assert!(item(facts, "issues", 6).get("new").is_none());
        let commented = item(facts, "pull_requests", 8);
        assert_eq!(
            commented["new"], false,
            "a comment never makes its item new"
        );
        assert_eq!(commented["last_comment"]["id"], 91);
        assert!(
            commented["last_comment"]["body_digest"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
        assert!(
            commented["last_comment"].get("body").is_none(),
            "bodies stay out of the graph"
        );
        assert_eq!(commented["mentions"][0]["login"], "orchid-bot");
        let cursor = RepositoryCursor::parse(observed.cursor.as_deref());
        assert_eq!(cursor.items_since.as_deref(), Some("2026-09-10T04:00:00Z"));
        assert_eq!(
            cursor.comments_since.as_deref(),
            Some("2026-09-10T05:00:00Z")
        );
    }

    #[tokio::test]
    async fn every_comment_and_review_is_one_recent_entry_without_its_body() {
        let (github, base) = FakeGithub::start().await;
        github.route("/repos/acme/garden", json!({"id": 7}));
        let since = "2026-09-10T00:00:00Z";
        github.route(
            &format!("/repos/acme/garden/issues?state=all&sort=updated&direction=asc&since={since}&per_page=100"),
            json!([]),
        );
        let comment = |id: u64, number: u64, at: &str| {
            json!({"id": id, "issue_url": format!("https://api.github.com/repos/acme/garden/issues/{number}"),
                "html_url": format!("https://github.com/acme/garden/issues/{number}#issuecomment-{id}"),
                "user": {"login": "fern"}, "body": "a body that never enters the facts",
                "created_at": at, "updated_at": at})
        };
        github.route(
            &format!("/repos/acme/garden/issues/comments?sort=updated&direction=asc&since={since}&per_page=100"),
            json!([
                comment(92, 8, "2026-09-10T05:02:00Z"),
                comment(91, 8, "2026-09-10T05:01:00Z"),
                comment(93, 2, "2026-09-10T05:04:00Z"),
            ]),
        );
        let mut pull = open_pull(2, &"a".repeat(40), "SUCCESS");
        pull["reviews"] = json!({"nodes": [
            {"databaseId": 5001, "author": {"login": "moss"}, "state": "APPROVED",
             "submittedAt": "2026-09-10T05:03:00Z"},
            {"databaseId": 5002, "author": {"login": "orchid-bot"}, "state": "PENDING",
             "submittedAt": null},
        ]});
        github.open_pull_requests(json!([pull]));
        let cursor = serde_json::to_string(&RepositoryCursor {
            items_since: Some(since.into()),
            comments_since: Some(since.into()),
            seen: BTreeMap::new(),
        })
        .unwrap();
        let observed = observe_at(
            request(
                &["pull_requests", "issues", "comments"],
                Some(&cursor),
                Some(json!({"repository_id": 7})),
            ),
            &base,
            Some("orchid-token"),
        )
        .await
        .unwrap();
        assert_eq!(
            item(&observed.facts, "issues", 8)["recent_comments"],
            json!([
                {"kind": "comment", "id": 91, "author": "fern", "at": "2026-09-10T05:01:00Z"},
                {"kind": "comment", "id": 92, "author": "fern", "at": "2026-09-10T05:02:00Z"},
            ]),
            "two comments in one poll are two entries, and no body"
        );
        assert_eq!(
            item(&observed.facts, "pull_requests", 2)["recent_comments"],
            json!([
                {"kind": "review", "id": 5001, "author": "moss", "at": "2026-09-10T05:03:00Z",
                 "state": "approved"},
                {"kind": "comment", "id": 93, "author": "fern", "at": "2026-09-10T05:04:00Z"},
            ]),
            "a submitted review joins the comments; a pending one is a draft"
        );

        // An observer that emits no comments keeps reviews out of its facts, though it shares
        // the pull request read.
        let pulls_only = observe_at(
            request(
                &["pull_requests"],
                Some(&cursor),
                Some(json!({"repository_id": 7})),
            ),
            &base,
            Some("orchid-token"),
        )
        .await
        .unwrap();
        assert!(
            item(&pulls_only.facts, "pull_requests", 2)
                .get("recent_comments")
                .is_none()
        );
    }

    #[tokio::test]
    async fn required_checks_count_what_the_base_requires_and_unreadable_rules_count_every_check() {
        let (github, base) = FakeGithub::start().await;
        github.route("/repos/acme/garden", json!({"id": 7}));
        github.route(
            "/repos/acme/garden/issues?state=open&per_page=100",
            json!([]),
        );
        github.route(
            "/repos/acme/garden/rules/branches/main",
            json!([
                {"type": "deletion"},
                {"type": "required_status_checks", "parameters": {"required_status_checks": [
                    {"context": "build", "integration_id": 15368},
                    {"context": "test", "integration_id": 15368},
                ]}},
            ]),
        );
        github.route(
            "/repos/acme/garden/branches/main",
            json!({"name": "main", "protected": true, "protection": {"enabled": true,
                "required_status_checks": {"enforcement_level": "non_admins",
                    "contexts": ["garden/legacy"], "checks": [{"context": "garden/legacy", "app_id": null}]}}}),
        );
        // A base whose rules this token may not read.
        github.refuse("/repos/acme/garden/rules/branches/release", "403 Forbidden");
        let run = |name: &str, conclusion: Option<&str>, app: u64| {
            json!({"__typename": "CheckRun", "name": name,
                "status": if conclusion.is_some() { "COMPLETED" } else { "IN_PROGRESS" },
                "conclusion": conclusion, "checkSuite": {"app": {"databaseId": app}}})
        };
        let legacy =
            json!({"__typename": "StatusContext", "context": "garden/legacy", "state": "SUCCESS"});
        let pull = |number: u64, base: &str, contexts: Value| {
            let mut node = open_pull(number, &format!("{number:0>40}"), "PENDING");
            node["baseRefName"] = json!(base);
            node["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["nodes"] =
                contexts;
            node
        };
        github.open_pull_requests(json!([
            // An optional check failing does not count.
            pull(
                1,
                "main",
                json!([
                    run("build", Some("SUCCESS"), 15368),
                    run("test", Some("SKIPPED"), 15368),
                    run("lint", Some("FAILURE"), 15368),
                    legacy
                ])
            ),
            pull(
                2,
                "main",
                json!([
                    run("build", Some("FAILURE"), 15368),
                    run("test", None, 15368),
                    legacy
                ])
            ),
            // A required check that has not appeared is pending.
            pull(
                3,
                "main",
                json!([run("build", Some("SUCCESS"), 15368), legacy])
            ),
            // A check of the required name from another app does not count, and neither does a
            // commit status, which names no app.
            pull(
                4,
                "main",
                json!([
                    run("build", Some("SUCCESS"), 999),
                    run("test", Some("SUCCESS"), 15368),
                    legacy
                ])
            ),
            // A failed status and a running check run of one counted name: the failure decides.
            pull(
                8,
                "main",
                json!([
                    run("build", Some("SUCCESS"), 15368),
                    run("test", Some("SUCCESS"), 15368),
                    {"__typename": "StatusContext", "context": "garden/legacy", "state": "FAILURE"},
                    run("garden/legacy", None, 15368)
                ])
            ),
            pull(
                7,
                "main",
                json!([
                    {"__typename": "StatusContext", "context": "build", "state": "SUCCESS"},
                    run("test", Some("SUCCESS"), 15368),
                    legacy
                ])
            ),
            pull(
                5,
                "release",
                json!([
                    run("build", Some("SUCCESS"), 15368),
                    run("lint", Some("FAILURE"), 15368)
                ])
            ),
            pull(6, "release", json!([])),
        ]));
        let observed = observe_at(
            request(&["pull_requests"], None, None),
            &base,
            Some("orchid-token"),
        )
        .await
        .expect("an unreadable rule never fails the observation");
        let required =
            |number: u64| item(&observed.facts, "pull_requests", number)["required_checks"].clone();
        let main_checks = json!(["build", "garden/legacy", "test"]);
        assert_eq!(
            required(1),
            json!({"state": "pass", "source": "rules", "checks": main_checks, "failed": []})
        );
        assert_eq!(
            required(2),
            json!({"state": "fail", "source": "rules", "checks": main_checks, "failed": ["build"]})
        );
        assert_eq!(required(3)["state"], "pending");
        assert_eq!(required(4)["state"], "pending");
        assert_eq!(required(7)["state"], "pending");
        assert_eq!(
            required(8),
            json!({"state": "fail", "source": "rules", "checks": main_checks, "failed": ["garden/legacy"]})
        );
        assert_eq!(
            required(5),
            json!({"state": "fail", "source": "all", "checks": ["build", "lint"], "failed": ["lint"]})
        );
        assert_eq!(required(6)["state"], "none");
        assert_eq!(
            item(&observed.facts, "pull_requests", 5)["base_branch"],
            "release"
        );
    }

    /// A pull request whose required checks failed is read again each minute, so a rerun that
    /// passes on the same head is seen without a change the REST reads would show.
    #[tokio::test]
    async fn a_failed_required_check_is_read_again_until_a_rerun_passes() {
        let (github, base) = FakeGithub::start().await;
        github.route("/repos/acme/garden", json!({"id": 7}));
        github.route(
            "/repos/acme/garden/issues?state=open&per_page=100",
            json!([]),
        );
        let failed = open_pull(2, &"a".repeat(40), "FAILURE");
        github.open_pull_requests(json!([failed]));
        let first = observe_at(
            request(&["pull_requests"], None, None),
            &base,
            Some("orchid-token"),
        )
        .await
        .unwrap();
        assert_eq!(
            item(&first.facts, "pull_requests", 2)["required_checks"]["state"],
            "fail"
        );
        let since = RepositoryCursor::parse(first.cursor.as_deref())
            .items_since
            .unwrap();
        github.route(
            &format!(
                "/repos/acme/garden/issues?state=all&sort=updated&direction=asc&since={since}&per_page=100"
            ),
            json!([]),
        );
        github.open_pull_requests(json!([open_pull(2, &"a".repeat(40), "SUCCESS")]));
        age_pull_request_check(&base, PULL_REQUEST_CHECK_INTERVAL);
        let rerun = observe_at(
            request(&["pull_requests"], first.cursor.as_deref(), None),
            &base,
            Some("orchid-token"),
        )
        .await
        .unwrap();
        assert_eq!(
            item(&rerun.facts, "pull_requests", 2)["required_checks"]["state"],
            "pass"
        );
    }

    /// A pull request opened within a minute of the last GraphQL read is read at once: its first
    /// record carries the head a review needs, which a later read could not give it.
    #[tokio::test]
    async fn a_pull_request_the_last_graphql_answer_lacks_is_read_at_once() {
        let (github, base) = FakeGithub::start().await;
        github.route("/repos/acme/garden", json!({"id": 7}));
        github.route(
            "/repos/acme/garden/issues?state=open&per_page=100",
            json!([]),
        );
        github.open_pull_requests(json!([open_pull(2, &"a".repeat(40), "SUCCESS")]));
        let first = observe_at(
            request(&["pull_requests"], None, None),
            &base,
            Some("orchid-token"),
        )
        .await
        .unwrap();
        let since = RepositoryCursor::parse(first.cursor.as_deref())
            .items_since
            .unwrap();
        github.route(
            &format!(
                "/repos/acme/garden/issues?state=all&sort=updated&direction=asc&since={since}&per_page=100"
            ),
            json!([issue(
                3,
                "2026-09-20T00:00:00Z",
                json!({"pull_request": {"merged_at": null}, "draft": false,
                "html_url": "https://github.com/acme/garden/pull/3"})
            )]),
        );
        github.open_pull_requests(json!([
            open_pull(2, &"a".repeat(40), "SUCCESS"),
            open_pull(3, &"c".repeat(40), "SUCCESS")
        ]));
        github.take_requests();
        let opened = observe_at(
            request(&["pull_requests"], first.cursor.as_deref(), None),
            &base,
            Some("orchid-token"),
        )
        .await
        .unwrap();
        assert!(
            github
                .take_requests()
                .iter()
                .any(|request| request.starts_with("POST /graphql"))
        );
        assert_eq!(
            item(&opened.facts, "pull_requests", 3)["head"],
            "c".repeat(40)
        );
    }

    /// A pull request change seen within a minute of the last GraphQL read waits for the minute,
    /// and the first poll after it asks, even when nothing changed since.
    #[tokio::test]
    async fn a_change_within_the_graphql_interval_is_read_once_the_interval_passes() {
        let (github, base) = FakeGithub::start().await;
        github.route("/repos/acme/garden", json!({"id": 7}));
        github.route(
            "/repos/acme/garden/issues?state=open&per_page=100",
            json!([]),
        );
        github.open_pull_requests(json!([open_pull(2, &"a".repeat(40), "SUCCESS")]));
        let first = observe_at(
            request(&["pull_requests"], None, None),
            &base,
            Some("orchid-token"),
        )
        .await
        .unwrap();
        let since = RepositoryCursor::parse(first.cursor.as_deref())
            .items_since
            .unwrap();
        let listing = format!(
            "/repos/acme/garden/issues?state=all&sort=updated&direction=asc&since={since}&per_page=100"
        );
        // A new head: the REST listing shows the pull request changed.
        github.route(
            &listing,
            json!([issue(
                2,
                "2026-09-20T00:00:00Z",
                json!({"pull_request": {"merged_at": null}, "draft": false,
                "html_url": "https://github.com/acme/garden/pull/2"})
            )]),
        );
        github.open_pull_requests(json!([open_pull(2, &"b".repeat(40), "SUCCESS")]));
        github.take_requests();
        let within = observe_at(
            request(&["pull_requests"], first.cursor.as_deref(), None),
            &base,
            Some("orchid-token"),
        )
        .await
        .unwrap();
        assert!(
            !github
                .take_requests()
                .iter()
                .any(|request| request.starts_with("POST /graphql"))
        );
        assert!(
            item(&within.facts, "pull_requests", 2)
                .get("head")
                .is_none(),
            "the new head waits for the next read"
        );
        age_pull_request_check(&base, PULL_REQUEST_CHECK_INTERVAL);
        let after = observe_at(
            request(&["pull_requests"], within.cursor.as_deref(), None),
            &base,
            Some("orchid-token"),
        )
        .await
        .unwrap();
        assert_eq!(
            item(&after.facts, "pull_requests", 2)["head"],
            "b".repeat(40)
        );
    }

    #[tokio::test]
    async fn an_observer_emits_only_its_declared_collection() {
        let (github, base) = FakeGithub::start().await;
        github.route("/repos/acme/garden", json!({"id": 7}));
        github.route(
            "/repos/acme/garden/issues?state=open&per_page=100",
            json!([
                issue(1, "2026-09-02T00:00:00Z", json!({"comments": 3})),
                issue(
                    2,
                    "2026-09-03T00:00:00Z",
                    json!({"pull_request": {"merged_at": null}})
                ),
            ]),
        );
        let observed = observe_at(
            request(&["issues"], None, None),
            &base,
            Some("orchid-token"),
        )
        .await
        .unwrap();
        assert_eq!(items(&observed.facts, "issues").len(), 1);
        assert!(observed.facts.get("pull_requests").is_none());
        assert!(
            item(&observed.facts, "issues", 1).get("comments").is_none(),
            "comments were not declared"
        );
        assert!(
            github
                .take_requests()
                .iter()
                .all(|request| !request.contains("graphql"))
        );
    }

    #[tokio::test]
    async fn a_first_poll_after_an_older_build_closes_pull_requests_that_left_the_open_listing() {
        let (github, base) = FakeGithub::start().await;
        github.route("/repos/acme/garden", json!({"id": 7}));
        github.route(
            "/repos/acme/garden/issues?state=open&per_page=100",
            json!([]),
        );
        github.open_pull_requests(json!([]));
        let legacy = json!({"repository_id": 7, "pull_requests": [
            {"number": 4, "state": "open", "head": "aaaa"},
            {"number": 5, "state": "closed", "head": "bbbb"},
        ]});
        let observed = observe_at(
            request(&["pull_requests"], Some("a3f1c0ffee"), Some(legacy)),
            &base,
            Some("orchid-token"),
        )
        .await
        .unwrap();
        assert_eq!(
            items(&observed.facts, "pull_requests"),
            vec![&json!({"number": 4, "state": "closed", "new": false})]
        );
    }

    #[tokio::test]
    async fn a_renamed_repository_keeps_its_identity_and_another_repository_fails() {
        let (github, base) = FakeGithub::start().await;
        github.route("/repos/acme/garden", json!({"id": 8}));
        let error = observe_at(
            request(&["issues"], None, Some(json!({"repository_id": 7}))),
            &base,
            Some("orchid-token"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("repository 8"), "{error}");
    }
}
