//! A seat's handoff: the one document it keeps for the session that comes after it, and the
//! message that gives a fresh session that document together with the graph's live state.
//!
//! The document is `doc/agents/<identity>/handoff`, versioned like any document, written by the
//! seat itself. The graph holds the rest of where a seat was (claimed steps, progress notes, mail
//! it has not read), so the handoff only carries what the graph cannot: what the seat was in the
//! middle of, the branches and pull requests it owns, what it waits on, and what to do next.

use std::fmt::Write as _;

use anyhow::Result;

use crate::model::{MessageView, StepRunView};
use crate::store::Store;

pub use crate::silent::{PREPARE_TAG_PREFIX, SESSION_START_TAG_PREFIX};

/// The `operation` of a restart request that asks for no handoff first.
pub const RESTART_NOW_OPERATION: &str = "restart-now";

/// The claim kind with which a seat says its handoff is current and a restart may go ahead.
pub const READY_KIND: &str = "custom.agent.restart-ready";

/// The subject of a seat's readiness claims.
pub fn ready_subject(agent: &str) -> String {
    use sha2::Digest as _;
    format!(
        "custom/agent/restart-ready-{}",
        hex::encode(sha2::Sha256::digest(agent.as_bytes()))
    )
}

/// The most of a handoff a session-start message carries. A longer handoff is cut here and says
/// where to read the rest.
pub const MAX_HANDOFF_BYTES: usize = 16 * 1024;

const MAX_WORK_LINES: usize = 12;
const MAX_MESSAGE_LINES: usize = 12;

/// The name of `agent`'s handoff document, or none when `agent` is not an agent identity.
pub fn document_name(agent: &str) -> Option<String> {
    let identity = agent.strip_prefix("agent/")?;
    (!identity.is_empty()).then(|| format!("doc/agents/{identity}/handoff"))
}

/// Whether `name` is in the part of the document namespace that belongs to seats. Only the seat
/// a document is named for writes it: a handoff is read as the seat's own words by the session
/// that follows, so a seat that could write another's would be writing into that seat's context.
pub fn is_seat_document(name: &str) -> bool {
    name.starts_with("doc/agents/")
}

/// Whether `writer` may bind `name`: any name outside the seat namespace, or `writer`'s own.
pub fn may_write(writer: &str, name: &str) -> bool {
    if !is_seat_document(name) {
        return true;
    }
    document_name(writer).is_some_and(|own| name == own)
}

/// The latest handoff a seat wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handoff {
    /// `doc/…@hash`, an immutable reference to exactly these bytes.
    pub reference: String,
    pub written_at_unix_ms: u128,
    pub text: String,
}

/// One unread message, by title and sender.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailLine {
    pub subject: String,
    pub from: String,
    pub title: Option<String>,
}

/// Everything a fresh session is given about where its seat was.
#[derive(Clone, Debug)]
pub struct Context {
    pub agent: String,
    pub handoff: Option<Handoff>,
    pub work: Vec<StepRunView>,
    pub mail: Vec<MailLine>,
}

/// Read a seat's handoff and live state from the graph.
pub fn gather(store: &Store, agent: &str) -> Result<Context> {
    let handoff = match document_name(agent) {
        Some(name) => latest_handoff(store, &name)?,
        None => None,
    };
    let mut work = store
        .work_for_reconcile(agent)?
        .into_iter()
        .filter(|step| {
            !matches!(
                step.status.as_str(),
                "pending" | "completed" | "failed" | "cancelled"
            ) && (step.claimant.as_deref() == Some(agent)
                || step.assigned_to.as_deref() == Some(agent))
        })
        .collect::<Vec<_>>();
    // A step whose last session ended is ready again for a new attempt, which has no progress
    // yet. The note the last session left on it is still the freshest word on where it was.
    for step in &mut work {
        if step.progress_summary.is_none()
            && let Some((note, at)) = last_progress_note(store, &step.subject)?
        {
            step.progress_summary = Some(note);
            step.progress_at_unix_ms = Some(at);
        }
    }
    // A step nobody has worked on is announced by its own ready-step message; this one is for the
    // work the last session was in the middle of.
    work.retain(|step| step.status != "ready" || step.progress_summary.is_some());
    let mail = store
        .messages(Some(agent), false)?
        .into_iter()
        .filter(|message| is_unread(message) && !is_session_start(message))
        .map(|message| MailLine {
            subject: message.subject,
            from: message.from,
            title: message.title,
        })
        .collect();
    Ok(Context {
        agent: agent.to_owned(),
        handoff,
        work,
        mail,
    })
}

/// Whether the seat has written its handoff, or said it is ready, since `since_unix_ms`. A readiness
/// claim counts only when its recorded actor is the seat itself; like other claims, the actor is what
/// the caller asserted, so this keeps out accidents and other seats, not a determined local caller.
pub fn acknowledged_since(store: &Store, agent: &str, since_unix_ms: u128) -> Result<bool> {
    // A pass that skips what nothing changed must see either acknowledgement arrive.
    smallclaims::touched::note_read(|| ready_subject(agent));
    if let Some(name) = document_name(agent) {
        smallclaims::touched::note_read(|| name);
    }
    if let Some(name) = document_name(agent)
        && store
            .list_documents(Some(&name), false, 1)?
            .into_iter()
            .any(|version| version.latest && version.created_at_unix_ms >= since_unix_ms)
    {
        return Ok(true);
    }
    Ok(store
        .claims_for(&ready_subject(agent), Some(READY_KIND))?
        .into_iter()
        .any(|claim| {
            claim.accepted_at_unix_ms >= since_unix_ms && claim.actor.as_deref() == Some(agent)
        }))
}

/// The "prepare for restart" message: why, by when, and what to do.
pub fn render_prepare(reason: &str, deadline_unix_ms: u128, now_unix_ms: u128) -> String {
    let seconds = deadline_unix_ms.saturating_sub(now_unix_ms).div_ceil(1000);
    format!(
        "st is about to restart this seat: {reason}. It restarts as soon as you acknowledge, or in \
         {seconds} s, whichever comes first, and the session after this one will not remember \
         this conversation.\n\n\
         Write your handoff now with `st agents handoff write --as \"$ST_AGENT\"` (stdin or a \
         file): what you were doing, open pull requests and branches, what you wait on, the next \
         steps, and anything running that you started. Writing it is the acknowledgement. If the \
         handoff you already have is current, run `st agents handoff ready --as \"$ST_AGENT\"` \
         instead. Take no new work in the meantime.\n"
    )
}

fn last_progress_note(store: &Store, step: &str) -> Result<Option<(String, u128)>> {
    Ok(store
        .claims_for(step, Some("work.progress"))?
        .into_iter()
        .rev()
        .find_map(|claim| {
            let fields = claim.body.get("fields").unwrap_or(&claim.body);
            if fields.get("handoff_acknowledged").is_some() {
                return None;
            }
            let note = fields.get("summary")?.as_str()?.trim();
            (!note.is_empty()).then(|| (note.to_owned(), claim.accepted_at_unix_ms))
        }))
}

fn latest_handoff(store: &Store, name: &str) -> Result<Option<Handoff>> {
    let Some(version) = store
        .list_documents(Some(name), false, 1)?
        .into_iter()
        .find(|version| version.latest)
    else {
        return Ok(None);
    };
    let Some(bytes) = store.get_document(&version.name, &version.hash)? else {
        return Ok(None);
    };
    Ok(Some(Handoff {
        reference: format!("{}@{}", version.name, version.hash),
        written_at_unix_ms: version.created_at_unix_ms,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }))
}

fn is_unread(message: &MessageView) -> bool {
    matches!(message.status.as_str(), "sent" | "staged" | "delivered")
}

fn is_session_start(message: &MessageView) -> bool {
    message
        .tags
        .iter()
        .any(|tag| tag.starts_with(SESSION_START_TAG_PREFIX))
}

/// The message a fresh session starts with, or none when the seat has nothing to carry over: no
/// handoff and no work. Unread mail reaches the session on its own, so it alone says nothing.
pub fn render(context: &Context, now_unix_ms: u128) -> Option<String> {
    if context.handoff.is_none() && context.work.is_empty() {
        return None;
    }
    let mut out = String::new();
    out.push_str(
        "This seat was restarted. If you remember the conversation before it, you already know \
         this. If you do not, this is where the session before you was: carry on from here \
         without waiting to be told.\n",
    );
    match &context.handoff {
        Some(handoff) => {
            let age = age(now_unix_ms.saturating_sub(handoff.written_at_unix_ms));
            let _ = write!(out, "\n## Handoff, written {age} ago ({})\n\n", handoff.reference);
            out.push_str(&clip(&handoff.text, MAX_HANDOFF_BYTES, &handoff.reference));
            out.push('\n');
        }
        None => out.push_str(
            "\n## Handoff\n\nThe previous session left none. Rely on the work below.\n",
        ),
    }
    if !context.work.is_empty() {
        out.push_str("\n## Your work in the graph\n");
        for step in context.work.iter().take(MAX_WORK_LINES) {
            let _ = write!(out, "\n- `{}` is {}", step.subject, step.status);
            if let Some(title) = step.title.as_deref().filter(|title| !title.is_empty()) {
                let _ = write!(out, ": {title}");
            }
            out.push('\n');
            for goal in &step.goals {
                let _ = writeln!(out, "  - goal: {}", one_line(goal));
            }
            match (&step.progress_summary, step.progress_at_unix_ms) {
                (Some(note), Some(at)) => {
                    let _ = writeln!(
                        out,
                        "  - last progress note, {} ago: {}",
                        age(now_unix_ms.saturating_sub(at)),
                        one_line(note)
                    );
                }
                (Some(note), None) => {
                    let _ = writeln!(out, "  - last progress note: {}", one_line(note));
                }
                _ => {}
            }
        }
        if context.work.len() > MAX_WORK_LINES {
            let _ = writeln!(
                out,
                "\n{} more steps: run `st work ls --as \"$ST_AGENT\"`.",
                context.work.len() - MAX_WORK_LINES
            );
        }
        out.push_str(
            "\nClaim a step that is ready with `st work claim STEP --as \"$ST_AGENT\"`; a step \
             still claimed by your last session is released when its lease runs out.\n",
        );
    }
    if !context.mail.is_empty() {
        out.push_str("\n## Unread mail\n\n");
        for line in context.mail.iter().take(MAX_MESSAGE_LINES) {
            let _ = writeln!(
                out,
                "- `{}` from {}: {}",
                line.subject,
                line.from,
                line.title.as_deref().unwrap_or("message")
            );
        }
        if context.mail.len() > MAX_MESSAGE_LINES {
            let _ = writeln!(
                out,
                "- and {} more: `st conversations ls`",
                context.mail.len() - MAX_MESSAGE_LINES
            );
        }
    }
    out.push_str(
        "\nKeep your handoff current: `st agents handoff write` replaces it with what the next session \
         needs. `st agents handoff show` prints it.\n",
    );
    Some(out)
}

fn one_line(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    clip(&text, 600, "the step")
}

fn clip(text: &str, limit: usize, source: &str) -> String {
    if text.len() <= limit {
        return text.trim_end().to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n[cut here; read all of {source}]", text[..end].trim_end())
}

fn age(ms: u128) -> String {
    let seconds = ms / 1000;
    match seconds {
        0..=59 => format!("{seconds} s"),
        60..=3599 => format!("{} min", seconds / 60),
        3600..=86_399 => format!("{} h", seconds / 3600),
        _ => format!("{} d", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(status: &str) -> StepRunView {
        serde_json::from_value(serde_json::json!({
            "subject": "step-run/abc/build",
            "run": "mission-run/example/one",
            "generation": "run-generation/abc",
            "step": "build",
            "definition_hash": "h",
            "status": status,
            "attempt": 1,
            "readiness_epoch": 1,
            "created_at_unix_ms": 0,
            "updated_at_unix_ms": 0,
            "assigned_to": "agent/garden/ash",
            "agentless": false,
            "worker_reported": false,
            "claimant": "agent/garden/ash",
            "claim_incarnation": null,
            "claim_expires_at_unix_ms": null,
            "title": "build the widget",
            "goals": ["The widget builds."],
            "progress_summary": "tests pass locally",
            "progress_at_unix_ms": 1_000,
        }))
        .expect("a step view")
    }

    fn context() -> Context {
        Context {
            agent: "agent/garden/ash".into(),
            handoff: Some(Handoff {
                reference: "doc/agents/garden/ash/handoff@abc".into(),
                written_at_unix_ms: 0,
                text: "Branch agent/widget has PR 12 open; wait for review.".into(),
            }),
            work: vec![step("claimed")],
            mail: vec![MailLine {
                subject: "message/1".into(),
                from: "agent/garden/birch".into(),
                title: Some("Question".into()),
            }],
        }
    }

    #[test]
    fn a_handoff_is_named_for_its_seat() {
        assert_eq!(
            document_name("agent/garden/ash").as_deref(),
            Some("doc/agents/garden/ash/handoff")
        );
        assert_eq!(document_name("person/ada"), None);
        assert_eq!(document_name("agent/"), None);
    }

    #[test]
    fn only_a_seat_writes_its_own_part_of_the_namespace() {
        let own = "doc/agents/garden/ash/handoff";
        assert!(may_write("agent/garden/ash", own));
        assert!(!may_write("agent/garden/birch", own));
        assert!(!may_write("agent/garden/ash", "doc/agents/garden/birch/handoff"));
        assert!(!may_write("agent/garden/ash", "doc/agents/garden/ash/other"));
        assert!(may_write("agent/garden/ash", "doc/garden/notes"));
    }

    #[test]
    fn a_fresh_session_sees_the_handoff_and_the_live_graph_state() {
        let text = render(&context(), 90_000).expect("a message");
        assert!(text.contains("Branch agent/widget has PR 12 open"), "{text}");
        assert!(text.contains("written 1 min ago (doc/agents/garden/ash/handoff@abc)"));
        assert!(text.contains("`step-run/abc/build` is claimed: build the widget"));
        assert!(text.contains("goal: The widget builds."));
        assert!(text.contains("last progress note, 1 min ago: tests pass locally"));
        assert!(text.contains("`message/1` from agent/garden/birch: Question"));
        assert!(text.contains("st agents handoff write"));
    }

    #[test]
    fn the_prepare_message_says_why_when_and_how_to_acknowledge() {
        let text = render_prepare("the declared model changed", 130_000, 10_000);
        assert!(text.contains("the declared model changed"), "{text}");
        assert!(text.contains("in 120 s"));
        assert!(text.contains("st agents handoff write"));
        assert!(text.contains("st agents handoff ready"));
    }

    #[test]
    fn a_seats_readiness_claims_have_their_own_subject() {
        assert_ne!(ready_subject("agent/garden/ash"), ready_subject("agent/garden/birch"));
        assert!(ready_subject("agent/garden/ash").starts_with("custom/agent/restart-ready-"));
    }

    #[test]
    fn a_seat_with_nothing_to_carry_over_gets_no_message() {
        let mut empty = context();
        empty.handoff = None;
        empty.work.clear();
        assert_eq!(render(&empty, 0), None);
    }

    #[test]
    fn work_alone_is_enough_and_says_there_was_no_handoff() {
        let mut work = context();
        work.handoff = None;
        let text = render(&work, 0).expect("a message");
        assert!(text.contains("The previous session left none"), "{text}");
        assert!(text.contains("`step-run/abc/build`"));
    }

    #[test]
    fn a_long_handoff_is_cut_with_its_reference() {
        let mut long = context();
        long.handoff.as_mut().unwrap().text = "é".repeat(MAX_HANDOFF_BYTES);
        let text = render(&long, 0).unwrap();
        assert!(text.contains("[cut here; read all of doc/agents/garden/ash/handoff@abc]"));
        assert!(text.len() < MAX_HANDOFF_BYTES + 4096);
    }
}
