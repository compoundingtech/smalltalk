//! The clients connected to this member now, kept in memory only: each open collection or
//! terminal stream, and each client seen in the last few minutes. A client names itself with
//! the optional `x-st3-client` header ("stui 0.1.0+a0c135e3"); st shows that as reported and
//! never treats it as identity or authority, and never refuses, slows or nags a client for it.

use super::client_v0::ClientSession;
use axum::http::HeaderMap;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

/// The header a client names itself with.
pub(super) const CLIENT_HEADER: &str = "x-st3-client";
/// How long a client that only made requests stays in the list after its last one.
const SEEN_FOR_MS: u128 = 5 * 60 * 1_000;
/// Bound the memory a stream of distinct clients can take.
const MAX_SEEN: usize = 512;
const MAX_NAME_CHARS: usize = 120;

/// Who a request or stream came from, as one row groups it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Key {
    member: String,
    actor: String,
    client: Option<String>,
    via: &'static str,
}

#[derive(Clone, Debug)]
struct Seen {
    person: String,
    first_ms: u128,
    last_ms: u128,
}

#[derive(Clone, Debug)]
struct Stream {
    key: Key,
    person: String,
    since_ms: u128,
    /// What it follows, by subscription id: a window, a conversation, a terminal.
    follows: BTreeMap<String, String>,
}

#[derive(Default)]
struct Presence {
    seen: HashMap<Key, Seen>,
    streams: BTreeMap<u64, Stream>,
    next: u64,
}

fn presence() -> &'static Mutex<Presence> {
    static PRESENCE: OnceLock<Mutex<Presence>> = OnceLock::new();
    PRESENCE.get_or_init(Mutex::default)
}

/// The client's own name for itself, bounded and printable, or none.
pub(super) fn client_name(headers: &HeaderMap) -> Option<String> {
    let name = headers.get(CLIENT_HEADER)?.to_str().ok()?.trim();
    let name: String = name
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_NAME_CHARS)
        .collect();
    (!name.is_empty()).then_some(name)
}

/// How the client reached this member: its own machine, or a paired device through the client
/// gateway (a Tailscale forward or Fabric reach the same socket, so st can say only that,
/// unless Tailscale Serve names itself).
pub(super) fn via(session: &ClientSession, headers: &HeaderMap) -> &'static str {
    if session.transport == "unix" {
        "local"
    } else if headers.contains_key("tailscale-user-login") {
        "tailscale"
    } else {
        "gateway"
    }
}

fn key(member: &str, session: &ClientSession, client: Option<String>, via: &'static str) -> Key {
    Key {
        member: member.to_owned(),
        actor: session.actor.clone(),
        client,
        via,
    }
}

/// Note one client request.
pub(super) fn note_request(
    member: &str,
    session: &ClientSession,
    headers: &HeaderMap,
    now_ms: u128,
) {
    let key = key(member, session, client_name(headers), via(session, headers));
    let Ok(mut presence) = presence().lock() else {
        return;
    };
    if presence.seen.len() >= MAX_SEEN && !presence.seen.contains_key(&key) {
        presence
            .seen
            .retain(|_, seen| now_ms.saturating_sub(seen.last_ms) < SEEN_FOR_MS);
        if presence.seen.len() >= MAX_SEEN {
            return;
        }
    }
    let seen = presence.seen.entry(key).or_insert(Seen {
        person: session.authority_actor.clone(),
        first_ms: now_ms,
        last_ms: now_ms,
    });
    seen.last_ms = now_ms;
}

/// An open stream, listed until it is dropped.
pub(super) struct StreamGuard(u64);

/// Note an open collection or terminal stream; it leaves the list when the guard drops.
pub(super) fn open_stream(
    member: &str,
    session: &ClientSession,
    headers: &HeaderMap,
    now_ms: u128,
) -> StreamGuard {
    let key = key(member, session, client_name(headers), via(session, headers));
    let Ok(mut presence) = presence().lock() else {
        return StreamGuard(u64::MAX);
    };
    presence.next += 1;
    let id = presence.next;
    presence.streams.insert(
        id,
        Stream {
            key,
            person: session.authority_actor.clone(),
            since_ms: now_ms,
            follows: BTreeMap::new(),
        },
    );
    StreamGuard(id)
}

impl StreamGuard {
    /// The stream now follows `what` under subscription `id`.
    pub(super) fn follow(&self, id: &str, what: String) {
        if let Ok(mut presence) = presence().lock()
            && let Some(stream) = presence.streams.get_mut(&self.0)
        {
            stream.follows.insert(id.to_owned(), what);
        }
    }

    pub(super) fn unfollow(&self, id: &str) {
        if let Ok(mut presence) = presence().lock()
            && let Some(stream) = presence.streams.get_mut(&self.0)
        {
            stream.follows.remove(id);
        }
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        if let Ok(mut presence) = presence().lock() {
            presence.streams.remove(&self.0);
        }
    }
}

/// The clients connected to `member` now and those seen in the last few minutes, newest first:
/// one row per client, person or agent, and way in.
pub(super) fn list(member: &str, now_ms: u128) -> Vec<Value> {
    let Ok(mut presence) = presence().lock() else {
        return Vec::new();
    };
    presence
        .seen
        .retain(|_, seen| now_ms.saturating_sub(seen.last_ms) < SEEN_FOR_MS);
    struct Row {
        person: String,
        since_ms: u128,
        last_ms: u128,
        streams: usize,
        follows: BTreeSet<String>,
    }
    let mut rows = BTreeMap::<Key, Row>::new();
    for (key, seen) in presence.seen.iter().filter(|(key, _)| key.member == member) {
        rows.insert(
            key.clone(),
            Row {
                person: seen.person.clone(),
                since_ms: seen.first_ms,
                last_ms: seen.last_ms,
                streams: 0,
                follows: BTreeSet::new(),
            },
        );
    }
    for stream in presence
        .streams
        .values()
        .filter(|stream| stream.key.member == member)
    {
        let row = rows.entry(stream.key.clone()).or_insert(Row {
            person: stream.person.clone(),
            since_ms: stream.since_ms,
            last_ms: now_ms,
            streams: 0,
            follows: BTreeSet::new(),
        });
        row.streams += 1;
        row.since_ms = row.since_ms.min(stream.since_ms);
        // An open stream is connected now.
        row.last_ms = now_ms;
        row.follows.extend(stream.follows.values().cloned());
    }
    let mut rows = rows.into_iter().collect::<Vec<_>>();
    rows.sort_by(|(left_key, left), (right_key, right)| {
        right
            .last_ms
            .cmp(&left.last_ms)
            .then(right.since_ms.cmp(&left.since_ms))
            .then(left_key.cmp(right_key))
    });
    rows.into_iter()
        .map(|(key, row)| {
            json!({
                "actor": key.actor,
                "person": row.person,
                "client": key.client,
                "member": super::client_host_id(&key.member),
                "via": key.via,
                "connected": row.streams > 0,
                "streams": row.streams,
                "since": super::client_timestamp(row.since_ms),
                "last_seen": super::client_timestamp(row.last_ms),
                "follows": row.follows,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(actor: &str, transport: &'static str) -> ClientSession {
        ClientSession::for_tests(actor, "person/avery", transport)
    }

    fn headers(client: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(client) = client {
            headers.insert(CLIENT_HEADER, client.parse().unwrap());
        }
        headers
    }

    #[test]
    fn a_stream_is_connected_while_open_and_a_request_is_seen_for_a_while() {
        let member = "presence-test-member-a";
        let phone = session("person/avery/session/0011", "fabric-loopback");
        let stream = open_stream(
            member,
            &phone,
            &headers(Some("smalltalk-ios 1.0 (42)")),
            1_000,
        );
        stream.follow("agents", "agents".into());
        stream.follow("chat", "conversation:agent/example/harbor".into());
        let rows = list(member, 2_000);
        let [row] = &rows[..] else { panic!("{rows:?}") };
        assert_eq!(row["client"], "smalltalk-ios 1.0 (42)");
        assert_eq!(row["via"], "gateway");
        assert_eq!(row["connected"], true);
        assert_eq!(row["person"], "person/avery");
        assert_eq!(
            row["follows"],
            json!(["agents", "conversation:agent/example/harbor"])
        );
        stream.unfollow("chat");
        assert_eq!(list(member, 2_500)[0]["follows"], json!(["agents"]));
        // The stream leaving takes it out; a client that never sent the header is still seen.
        drop(stream);
        assert!(list(member, 3_000).is_empty());
        let local = session("person/avery", "unix");
        note_request(member, &local, &headers(None), 4_000);
        let rows = list(member, 5_000);
        let [row] = &rows[..] else { panic!("{rows:?}") };
        assert_eq!(
            (row["via"].as_str(), row["client"].is_null()),
            (Some("local"), true)
        );
        assert_eq!(row["connected"], false);
        // Seen only for a few minutes.
        assert!(list(member, 4_000 + SEEN_FOR_MS).is_empty());
        // Another member's clients are not this member's.
        assert!(list("presence-test-member-b", 5_000).is_empty());
    }

    #[test]
    fn a_client_name_is_bounded_and_printable() {
        assert_eq!(
            client_name(&headers(Some("  stui 0.1.0+abc  "))).as_deref(),
            Some("stui 0.1.0+abc")
        );
        assert_eq!(client_name(&headers(Some("   "))), None);
        let long = "x".repeat(400);
        assert_eq!(
            client_name(&headers(Some(&long))).unwrap().len(),
            MAX_NAME_CHARS
        );
    }
}
