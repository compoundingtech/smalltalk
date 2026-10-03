use std::collections::VecDeque;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use st3_client::{
    Agent, Attention, Client, ClientError, Device, Envelope, ErrorCode, Machine, Mission, Page,
    Resource, Session, Snapshot, SyncNotice, TimelineEntry,
};

/// Each collection is deliberately capped. The UI shows a truncation marker when a cap is hit.
const PAGE_SIZE: usize = 50;
pub const MAX_PAGES: usize = 4;

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Collection {
    pub items: Vec<Resource>,
    pub snapshot: Option<Snapshot>,
    pub truncated: bool,
    /// Present while the host was catching up with a peer when it served this collection.
    #[serde(default)]
    pub sync: Option<SyncNotice>,
}

impl Collection {}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Model {
    #[serde(default)]
    pub last_connected: Option<String>,
    pub now: Collection,
    pub messages: Collection,
    pub launches: Collection,
    pub missions: Collection,
    pub work: Collection,
    pub agents: Collection,
    pub sessions: Collection,
    pub runtimes: Collection,
    pub machines: Collection,
    pub devices: Collection,
    pub tree: crate::tree::MissionsTree,
    pub timeline: Vec<TimelineEntry>,
    pub timeline_truncated: bool,
    pub event_cursor: String,
    pub actor: String,
    pub status: String,
    recent_events: VecDeque<String>,
    /// Token spend over the Usage tab's period, read while something shows it, or why not.
    #[serde(skip)]
    pub usage: Option<std::result::Result<st3_client::UsagePeriod, String>>,
}

impl Model {
    /// The sync notice from the most recently served collection. An older collection that has
    /// not reloaded since the host caught up must not keep the notice alive.
    pub fn sync_notice(&self) -> Option<&SyncNotice> {
        // Now comes last so it wins a tie: it is the collection re-read while syncing.
        [
            &self.messages,
            &self.launches,
            &self.missions,
            &self.work,
            &self.agents,
            &self.sessions,
            &self.runtimes,
            &self.machines,
            &self.devices,
            &self.now,
        ]
        .into_iter()
        .filter_map(|collection| Some((collection.snapshot.as_ref()?.store_index, collection)))
        .max_by_key(|(store_index, _)| *store_index)
        .and_then(|(_, collection)| collection.sync.as_ref())
    }

    pub fn attention(&self) -> impl Iterator<Item = &Attention> + Clone {
        self.now.items.iter().filter_map(|item| match item {
            Resource::Attention(v) if v.state != "resolved" && v.person_id == self.actor => Some(v),
            _ => None,
        })
    }
    pub fn agents(&self) -> impl Iterator<Item = &Agent> + Clone {
        self.agents.items.iter().filter_map(|item| match item {
            Resource::Agent(v) => Some(v),
            _ => None,
        })
    }
    pub fn undeclared_sessions(&self) -> impl Iterator<Item = &Session> {
        self.sessions.items.iter().filter_map(|item| match item {
            Resource::Session(v)
                if v.state == "running"
                    && v.extra.get("managed").and_then(serde_json::Value::as_bool)
                        == Some(false) =>
            {
                Some(v)
            }
            _ => None,
        })
    }
    pub fn missions(&self) -> impl Iterator<Item = &Mission> + Clone {
        self.missions.items.iter().filter_map(|item| match item {
            Resource::Mission(v) => Some(v),
            _ => None,
        })
    }
    pub fn machines(&self) -> impl Iterator<Item = &Machine> {
        self.machines.items.iter().filter_map(|item| match item {
            Resource::Machine(v) => Some(v),
            _ => None,
        })
    }
    pub fn devices(&self) -> impl Iterator<Item = &Device> {
        self.devices.items.iter().filter_map(|item| match item {
            Resource::Device(v) => Some(v),
            _ => None,
        })
    }
}

/// What the screens read page by page when a tab opens.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Kind {
    NativeSessions,
    Machines,
    Devices,
}

/// Harness sessions on this machine that st did not start. Nothing announces them, so the
/// screens read them when the Agents tab opens.
pub async fn read_native_sessions(client: &Client) -> Result<Collection> {
    read_pages(client, Kind::NativeSessions).await
}

/// The Fleet tab's machines, read when it opens.
pub async fn read_machines(client: &Client) -> Result<Collection> {
    read_pages(client, Kind::Machines).await
}

/// The Fleet tab's paired devices, read when it opens.
pub async fn read_devices(client: &Client) -> Result<Collection> {
    read_pages(client, Kind::Devices).await
}

async fn read_pages(client: &Client, kind: Kind) -> Result<Collection> {
    for attempt in 0..3 {
        match read_pages_once(client, kind).await {
            Ok(collection) => return Ok(collection),
            Err(error)
                if attempt < 2
                    && error.downcast_ref::<ClientError>().is_some_and(|error| {
                        matches!(error, ClientError::Api(ErrorCode::PageCursorExpired, _, _))
                    }) =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1))).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("bounded page retry returns from every attempt")
}

async fn read_pages_once(client: &Client, kind: Kind) -> Result<Collection> {
    let mut result = Collection::default();
    let mut cursor = None;
    for page_index in 0..MAX_PAGES {
        let Envelope {
            snapshot, value, ..
        }: Envelope<Page> = match kind {
            Kind::NativeSessions => {
                client
                    .sessions_list_native(cursor.as_deref(), Some(PAGE_SIZE), false)
                    .await?
            }
            Kind::Machines => {
                client
                    .machines_list(cursor.as_deref(), Some(PAGE_SIZE), false)
                    .await?
            }
            Kind::Devices => {
                client
                    .devices_list(cursor.as_deref(), Some(PAGE_SIZE), false)
                    .await?
            }
        };
        result.snapshot = Some(snapshot);
        result.sync = value.sync;
        result.items.extend(value.items);
        if !value.page.has_more {
            return Ok(result);
        }
        if page_index + 1 == MAX_PAGES {
            result.truncated = true;
            return Ok(result);
        }
        cursor = value.page.next_cursor;
        if cursor.is_none() {
            anyhow::bail!("resource page omitted continuation cursor");
        }
    }
    Ok(result)
}

pub use st3_conversation_ui::clean_message_text;
#[cfg(test)]
mod tests {
    use super::*;

    fn work_resource(id: &str, state: &str, layer: &str) -> Resource {
        serde_json::from_value(serde_json::json!({
            "kind":"work", "id":id, "revision":"one", "updated_at":"2026-09-25T08:00:00Z",
            "operational":{"layer":layer,"actionable":layer=="current","reasons":[]},
            "mission_run_id":"mission-run/test", "generation_id":"run-generation/test",
            "definition_id":"definition/test", "path":"step", "state":state,
            "attempt":1, "readiness_epoch":1, "claimant":null, "claim_incarnation":null,
            "blocked_reason":null, "blockers":[], "goals":[], "constraints":[]
        }))
        .unwrap()
    }

    #[test]
    fn a_message_that_names_a_hidden_tag_is_not_cut_short() {
        let text = "Hooks wrap context in `<system-reminder>` blocks, and a <thinking> mention \
                    stays too (agent/postel-transcripts). I checked both harnesses.";
        assert_eq!(clean_message_text(text), text);
        assert_eq!(
            clean_message_text("Done.\n<thinking>\nprivate, never closed"),
            "Done."
        );
        assert_eq!(
            clean_message_text("Before <thinking>private</thinking>after"),
            "Before after"
        );
        assert_eq!(
            clean_message_text("A long report\n[st truncated this native timeline value]"),
            "A long report\n\n… st kept only the start of this"
        );
    }

    #[test]
    fn regression_notification_text_is_cleaned() {
        assert_eq!(
            clean_message_text("[PING] ? hello [id:message/abc]"),
            "hello"
        );
        assert_eq!(clean_message_text("[PING] ?"), "");
    }

    #[test]
    fn fixture_only_routes_person_attention_to_now() {
        let resources: Vec<Resource> = serde_json::from_str(include_str!(
            "../../../docs/st3/client-v0/fixtures/resources.json"
        ))
        .unwrap();
        let model = Model {
            actor: "person/alex".into(),
            now: Collection {
                items: resources,
                ..Collection::default()
            },
            ..Model::default()
        };
        let attention: Vec<_> = model.attention().collect();
        assert_eq!(attention.len(), 1);
        assert_eq!(attention[0].header.id, "attention/release-review");
    }

    #[test]
    fn running_undeclared_sessions_are_separate_from_managed_and_history() {
        let resources: Vec<Resource> = serde_json::from_str(
            r#"[
                {"kind":"session","id":"session/external","revision":"one","updated_at":"2026-09-24T09:00:00Z","owner_id":"external-session/codex/one","state":"running","started_at":"2026-09-24T08:00:00Z","ended_at":null,"timeline_cursor":"cursor/one","managed":false,"driver":"codex","native_session_id":"one"},
                {"kind":"session","id":"session/unresolved","revision":"two","updated_at":"2026-09-24T09:00:00Z","owner_id":"external-process/claude/42","state":"running","started_at":"2026-09-24T08:00:00Z","ended_at":null,"timeline_cursor":"cursor/two","managed":false,"driver":"claude","native_session_id":null},
                {"kind":"session","id":"session/managed","revision":"three","updated_at":"2026-09-24T09:00:00Z","owner_id":"agent/one","state":"running","started_at":"2026-09-24T08:00:00Z","ended_at":null,"timeline_cursor":"cursor/three"},
                {"kind":"session","id":"session/old","revision":"four","updated_at":"2026-09-24T09:00:00Z","owner_id":"external-session/codex/old","state":"completed","started_at":"2026-09-23T08:00:00Z","ended_at":"2026-09-23T09:00:00Z","timeline_cursor":"cursor/four","managed":false}
            ]"#,
        )
        .unwrap();
        let model = Model {
            sessions: Collection {
                items: resources,
                ..Collection::default()
            },
            ..Model::default()
        };
        let ids = model
            .undeclared_sessions()
            .map(|session| session.header.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["session/external", "session/unresolved"]);
    }
}
