//! The permission prompts a Claude seat waits on: its own and its subagents', oldest first.
//!
//! Claude asks through one `PermissionRequest` hook per prompt, whoever asks. The seat's top level
//! shows one prompt at a time, but a subagent (one running in the background in particular) asks
//! while the top level works, idles or asks too. Each prompt is a person's to answer, so the seat's
//! observation shows the oldest open prompt, and each prompt it shows is its own transition: the
//! hook that waits on it answers exactly that observation, never an earlier or later prompt's.
//!
//! A prompt is open from its `PermissionRequest` until the agent that asked moves on: the top
//! level's next tool, turn or session event, or the asking subagent's next tool event or its
//! `SubagentStop`. Claude reports no event when a prompt is refused in its terminal; the screen
//! says so for the top level (`native-prompt-gone`), and the refused agent's next event closes it.
//!
//! The queue is a small file beside the seat's state record, read and rewritten under its own
//! lock by each hook process. It holds tool names only: the call's input is conversation content.
//! A seat that ends without its hooks closing its prompts leaves them in the file until its next
//! session starts, which clears it; nothing reads them meanwhile.
//!
//! A subagent's prompt is shown as `subagent:<tool>`: its call is in the subagent's own
//! transcript, not in the seat's conversation where a client shows the pending call, so a client
//! never offers to allow it beside a call it could not show (st offers only deny for it).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::harness_state::{self, Activity, Ask, BlockedOn, InputBuffer, Observation};

const QUEUE_FILE: &str = "claude-prompts.json";
/// The reason prefix of a subagent's prompt.
pub const SUBAGENT_PREFIX: &str = "subagent:";
const LOCK_FILE: &str = "claude-prompts.lock";

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct Queue {
    /// Open prompts, oldest first. The first is the one the seat's observation shows.
    prompts: Vec<Prompt>,
    /// The top level's own state beneath the prompts, written again when the last one closes.
    base: Option<Base>,
    /// Each agent's latest call, from its `PreToolUse` (the top level's under ""): Claude asks
    /// about a call right after that event, and the `PermissionRequest` names no call itself.
    #[serde(default)]
    calls: BTreeMap<String, Call>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Call {
    id: String,
    tool: String,
    digest: String,
}

/// A digest of a call's tool and input, to pair a prompt with the call it asks about.
fn call_digest(tool: &str, input: &serde_json::Value) -> String {
    let digest = Sha256::digest(format!("{tool}\n{input}").as_bytes());
    digest[..16].iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Prompt {
    /// [`prompt_id`] of the `PermissionRequest` payload that opened it.
    id: String,
    /// The subagent that asks, or `None` for the seat's top level.
    agent: Option<String>,
    tool: String,
    /// The state record's (ownership, transition) sequences of the observation that shows it,
    /// once it is the oldest open prompt and that observation is written.
    shown: Option<(u64, u64)>,
    /// The id of the call it asks about (the asking agent's `PreToolUse` of the same tool and
    /// input), when Claude reported it.
    #[serde(default)]
    call: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Base {
    state: Activity,
    reason: Option<String>,
}

/// The id of the call the prompt `id` asks about, when its asker's `PreToolUse` named it.
pub fn prompt_call(agent_dir: &Path, id: &str) -> Option<String> {
    load(agent_dir)
        .prompts
        .into_iter()
        .find(|prompt| prompt.id == id)
        .and_then(|prompt| prompt.call)
}

/// Where one waiting prompt stands, as its hook reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waiting {
    /// Open, behind an older prompt the seat still shows.
    Queued,
    /// Open and shown by the observation with these (ownership, transition) sequences.
    Shown(u64, u64),
    /// Closed: answered or refused somewhere, or the session ended.
    Gone,
}

/// The identity of the prompt one `PermissionRequest` payload opens. The hook that records the
/// prompt and the same hook waiting on it compute it from the same bytes. Two identical requests
/// from one agent share it, which is safe: an agent asks one prompt at a time, and its prompt is
/// closed by its next event before it can ask the same again.
pub fn prompt_id(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    digest[..16].iter().map(|byte| format!("{byte:02x}")).collect()
}

fn queue_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(QUEUE_FILE)
}

fn load(agent_dir: &Path) -> Queue {
    std::fs::read(queue_path(agent_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Where the prompt `id` stands.
pub fn waiting(agent_dir: &Path, id: &str) -> Waiting {
    let queue = load(agent_dir);
    match queue.prompts.iter().find(|prompt| prompt.id == id) {
        None => Waiting::Gone,
        Some(Prompt {
            shown: Some((ownership, transition)),
            ..
        }) => Waiting::Shown(*ownership, *transition),
        Some(_) => Waiting::Queued,
    }
}

/// Whether an event that says nothing about top-level state still concerns the seat's prompts:
/// a subagent asking, or an event that may close a prompt while one is open.
pub(crate) fn concerns_prompts(event: &str, payload: &serde_json::Value, agent_dir: &Path) -> bool {
    event == "PermissionRequest" || (asker(payload).is_some() && !load(agent_dir).prompts.is_empty())
}

fn asker(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        .filter(|agent| !agent.is_empty())
        .map(str::to_owned)
}

fn ask_of(tool: &str) -> Ask {
    if tool == "AskUserQuestion" {
        Ask::Question
    } else {
        Ask::Permission
    }
}

/// Apply one hook event to the seat's open prompts and decide what the seat's observation says.
///
/// `top` is the event's own top-level observation ([`crate::claude_session::observe_hook_event`]),
/// `None` for a subagent's event or one that says nothing about top-level state. `write` records
/// an observation, starting a transition when told the observation shows a newly shown prompt, and
/// returns the record's (ownership, transition) sequences once written.
pub(crate) fn observe(
    agent_dir: &Path,
    event: &str,
    payload: &serde_json::Value,
    raw: &str,
    top: Option<Observation>,
    write: impl FnOnce(Observation, bool) -> Result<Option<(u64, u64)>>,
) -> Result<()> {
    let _lock = harness_state::lock_exclusive(&agent_dir.join(LOCK_FILE))?;
    let mut queue = load(agent_dir);
    let before = queue.clone();
    let agent = asker(payload);
    match event {
        "SessionStart" => queue.prompts.clear(),
        "PermissionRequest" => {
            let id = prompt_id(raw);
            let tool = payload
                .get("tool_name")
                .and_then(serde_json::Value::as_str)
                .filter(|tool| !tool.is_empty())
                .unwrap_or("permissionRequest")
                .to_owned();
            // The call this prompt asks about: the asker's latest call, of the same tool and input.
            let input = payload.get("tool_input").cloned().unwrap_or(serde_json::Value::Null);
            let call = queue
                .calls
                .get(agent.as_deref().unwrap_or_default())
                .filter(|call| call.tool == tool && call.digest == call_digest(&tool, &input))
                .map(|call| call.id.clone());
            if !queue.prompts.iter().any(|prompt| prompt.id == id) {
                queue.prompts.push(Prompt {
                    id,
                    agent: agent.clone(),
                    tool,
                    shown: None,
                    call,
                });
            }
        }
        // The asking agent moved on: its prompt was answered, wherever.
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => {
            queue.prompts.retain(|prompt| prompt.agent != agent);
            if event == "PreToolUse"
                && let (Some(id), Some(tool)) = (
                    payload.get("tool_use_id").and_then(serde_json::Value::as_str),
                    payload.get("tool_name").and_then(serde_json::Value::as_str),
                )
            {
                let input = payload.get("tool_input").cloned().unwrap_or(serde_json::Value::Null);
                queue.calls.insert(
                    agent.clone().unwrap_or_default(),
                    Call { id: id.to_owned(), tool: tool.to_owned(), digest: call_digest(tool, &input) },
                );
            }
        }
        "SubagentStop" if agent.is_some() => {
            queue.prompts.retain(|prompt| prompt.agent != agent);
            queue.calls.remove(agent.as_deref().unwrap_or_default());
        }
        "Stop" | "StopFailure" | "UserPromptSubmit" | "PreCompact" if agent.is_none() => {
            queue.prompts.retain(|prompt| prompt.agent.is_some());
        }
        _ => {}
    }
    if let Some(top) = top.as_ref().filter(|_| agent.is_none()) {
        queue.base = Some(Base {
            // A top-level prompt's own observation waits on a person; beneath it the top level
            // is active.
            state: top.state,
            reason: if top.blocked_on == BlockedOn::Human {
                None
            } else {
                top.reason.clone()
            },
        });
    }
    let observation = match queue.prompts.first() {
        Some(prompt) => {
            let mut observation =
                Observation::new(Activity::Active, BlockedOn::Human, InputBuffer::Unknown)
                    .with_ask(ask_of(&prompt.tool))
                    .with_reason(match &prompt.agent {
                        Some(_) => format!("{SUBAGENT_PREFIX}{}", prompt.tool),
                        None => prompt.tool.clone(),
                    });
            observation.provider_auth = top.as_ref().and_then(|top| top.provider_auth);
            Some((observation, prompt.shown.is_none()))
        }
        None => match top {
            Some(top) => Some((top, false)),
            // The last prompt closed on a subagent's event: the top level's own state again.
            None if !before.prompts.is_empty() => queue.base.as_ref().map(|base| {
                let observation = Observation::new(base.state, BlockedOn::None, InputBuffer::Unknown);
                (
                    match &base.reason {
                        Some(reason) => observation.with_reason(reason.clone()),
                        None => observation,
                    },
                    false,
                )
            }),
            None => None,
        },
    };
    if let Some((observation, new_prompt)) = observation
        && let Some(sequences) = write(observation, new_prompt)?
        && let Some(prompt) = queue.prompts.first_mut()
        && prompt.shown.is_none()
    {
        prompt.shown = Some(sequences);
    }
    if queue != before {
        harness_state::write_json_atomic(&queue_path(agent_dir), &queue, agent_dir, ".claude-prompts")
            .context("recording the Claude seat's open prompts")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Records what `observe` writes, and stands in for the state record's sequences: each
    /// transition is one more, a restatement the same.
    #[derive(Default)]
    struct Record {
        written: Vec<(Activity, BlockedOn, Option<String>)>,
        transition: u64,
        last: Option<(Activity, BlockedOn, Option<String>)>,
    }

    impl Record {
        fn event(&mut self, dir: &Path, event: &str, payload: serde_json::Value) {
            let raw = payload.to_string();
            let top = crate::claude_session::observe_hook_event(event, &payload);
            observe(dir, event, &payload, &raw, top, |observation, new_prompt| {
                let entry = (observation.state, observation.blocked_on, observation.reason.clone());
                if new_prompt || self.last.as_ref() != Some(&entry) {
                    self.transition += 1;
                }
                self.last = Some(entry.clone());
                self.written.push(entry);
                Ok(Some((1, self.transition)))
            })
            .unwrap();
        }

        fn shows(&self) -> (Activity, BlockedOn, Option<String>) {
            self.last.clone().unwrap()
        }
    }

    fn request(tool: &str, agent: Option<&str>, call: &str) -> serde_json::Value {
        let mut payload = json!({"hook_event_name": "PermissionRequest", "tool_name": tool,
            "tool_input": {"command": call}});
        if let Some(agent) = agent {
            payload["agent_id"] = json!(agent);
        }
        payload
    }

    fn tool_event(agent: Option<&str>) -> serde_json::Value {
        match agent {
            Some(agent) => json!({"tool_name": "Bash", "agent_id": agent}),
            None => json!({"tool_name": "Bash"}),
        }
    }

    /// A subagent's prompt waits on a person whatever the top level does, and closing it restores
    /// the top level's own state: here, idle after its turn, with the subagent in the background.
    #[test]
    fn a_subagents_prompt_waits_on_a_person_and_its_next_tool_closes_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = Record::default();
        record.event(dir.path(), "UserPromptSubmit", json!({}));
        record.event(dir.path(), "Stop", json!({}));
        assert_eq!(record.shows().0, Activity::Idle);

        let asked = request("Bash", Some("agent-a"), "date");
        let id = prompt_id(&asked.to_string());
        record.event(dir.path(), "PermissionRequest", asked);
        assert_eq!(
            record.shows(),
            (Activity::Active, BlockedOn::Human, Some("subagent:Bash".into()))
        );
        assert_eq!(waiting(dir.path(), &id), Waiting::Shown(1, record.transition));

        // Another agent's tool, or the top level's, does not answer it.
        record.event(dir.path(), "PostToolUse", tool_event(Some("agent-b")));
        assert_eq!(record.shows().1, BlockedOn::Human);
        assert!(matches!(waiting(dir.path(), &id), Waiting::Shown(..)));

        record.event(dir.path(), "PostToolUse", tool_event(Some("agent-a")));
        assert_eq!(waiting(dir.path(), &id), Waiting::Gone);
        assert_eq!(record.shows(), (Activity::Idle, BlockedOn::None, None));
    }

    /// Two prompts at once: the oldest is shown, and the next is shown as its own transition once
    /// the first closes, so the hook waiting on each answers only its own.
    #[test]
    fn prompts_are_shown_oldest_first_each_as_its_own_transition() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = Record::default();
        record.event(dir.path(), "UserPromptSubmit", json!({}));
        let top = request("Bash", None, "make");
        let sub = request("Bash", Some("agent-a"), "make");
        let (top_id, sub_id) = (prompt_id(&top.to_string()), prompt_id(&sub.to_string()));
        assert_ne!(top_id, sub_id);
        record.event(dir.path(), "PermissionRequest", top);
        let first = record.transition;
        record.event(dir.path(), "PermissionRequest", sub);
        assert_eq!(waiting(dir.path(), &top_id), Waiting::Shown(1, first));
        assert_eq!(waiting(dir.path(), &sub_id), Waiting::Queued);

        // The top level's prompt is answered: its tool runs.
        record.event(dir.path(), "PreToolUse", tool_event(None));
        assert_eq!(waiting(dir.path(), &top_id), Waiting::Gone);
        let Waiting::Shown(_, second) = waiting(dir.path(), &sub_id) else {
            panic!("the subagent's prompt is shown next");
        };
        assert!(second > first, "the second prompt is its own transition");
        assert_eq!(record.shows().1, BlockedOn::Human);

        // The subagent ends without asking again: nothing waits.
        record.event(
            dir.path(),
            "SubagentStop",
            json!({"agent_id": "agent-a"}),
        );
        assert_eq!(waiting(dir.path(), &sub_id), Waiting::Gone);
        assert_eq!(record.shows(), (Activity::Active, BlockedOn::None, None));
    }

    /// The parent's Task call is open (its own PreToolUse, no result yet) while the subagent it
    /// started asks: the seat shows the subagent's prompt as the subagent's, not as the call the
    /// seat's conversation shows unanswered.
    #[test]
    fn a_subagents_prompt_beside_its_parents_open_task_call_is_marked_as_the_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = Record::default();
        record.event(dir.path(), "UserPromptSubmit", json!({}));
        record.event(dir.path(), "PreToolUse", json!({"tool_name": "Task"}));
        record.event(dir.path(), "PermissionRequest", request("Bash", Some("agent-a"), "make"));
        assert_eq!(
            record.shows(),
            (Activity::Active, BlockedOn::Human, Some("subagent:Bash".into()))
        );
    }

    /// A prompt is bound to the exact call it asks about: the asker's own preceding call of the
    /// same tool and input. Another call, or one Claude did not name, binds nothing.
    #[test]
    fn a_prompt_names_the_call_it_asks_about() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = Record::default();
        record.event(dir.path(), "UserPromptSubmit", json!({}));
        record.event(
            dir.path(),
            "PreToolUse",
            json!({"tool_name": "Bash", "tool_use_id": "toolu_7", "tool_input": {"command": "make"}}),
        );
        let asked = request("Bash", None, "make");
        let id = prompt_id(&asked.to_string());
        record.event(dir.path(), "PermissionRequest", asked);
        assert_eq!(prompt_call(dir.path(), &id).as_deref(), Some("toolu_7"));

        // The next call's prompt is about another input: its call is not this one.
        record.event(
            dir.path(),
            "PreToolUse",
            json!({"tool_name": "Bash", "tool_use_id": "toolu_8", "tool_input": {"command": "ls"}}),
        );
        let other = request("Bash", None, "make clean");
        let other_id = prompt_id(&other.to_string());
        record.event(dir.path(), "PermissionRequest", other);
        assert_eq!(prompt_call(dir.path(), &other_id), None);
    }

    /// A parent's Task call and its subagent's prompt are open at once: the prompt names the
    /// subagent's own call, never the parent's Task call, and another subagent asking the same
    /// thing names none of theirs.
    #[test]
    fn a_subagents_prompt_beside_its_parents_task_call_names_the_subagents_call() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = Record::default();
        record.event(dir.path(), "UserPromptSubmit", json!({}));
        record.event(
            dir.path(),
            "PreToolUse",
            json!({"tool_name": "Task", "tool_use_id": "toolu_task",
                "tool_input": {"prompt": "run the build"}}),
        );
        record.event(
            dir.path(),
            "PreToolUse",
            json!({"tool_name": "Bash", "agent_id": "agent-a", "tool_use_id": "toolu_sub",
                "tool_input": {"command": "make"}}),
        );
        let asked = request("Bash", Some("agent-a"), "make");
        let id = prompt_id(&asked.to_string());
        record.event(dir.path(), "PermissionRequest", asked);
        assert_eq!(prompt_call(dir.path(), &id).as_deref(), Some("toolu_sub"));

        let other = request("Bash", Some("agent-b"), "make");
        let other_id = prompt_id(&other.to_string());
        record.event(dir.path(), "PermissionRequest", other);
        assert_eq!(prompt_call(dir.path(), &other_id), None, "agent-b made no call of its own");

        let parents = request("Bash", None, "make");
        let parents_id = prompt_id(&parents.to_string());
        record.event(dir.path(), "PermissionRequest", parents);
        assert_eq!(prompt_call(dir.path(), &parents_id), None, "the parent's call is its Task");
    }

    /// A question is its own ask, and a new session forgets every prompt.
    #[test]
    fn a_question_is_an_ask_and_a_new_session_closes_every_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = Record::default();
        let asked = request("AskUserQuestion", None, "");
        let id = prompt_id(&asked.to_string());
        let raw = asked.to_string();
        let top = crate::claude_session::observe_hook_event("PermissionRequest", &asked);
        observe(dir.path(), "PermissionRequest", &asked, &raw, top, |observation, _| {
            assert_eq!(observation.ask, Ask::Question);
            Ok(Some((1, 1)))
        })
        .unwrap();
        assert_eq!(waiting(dir.path(), &id), Waiting::Shown(1, 1));
        record.event(dir.path(), "SessionStart", json!({}));
        assert_eq!(waiting(dir.path(), &id), Waiting::Gone);
        assert_eq!(record.shows().1, BlockedOn::None);
    }
}
