//! A live native permission invocation. Its socket dies with the invocation; a saved prompt
//! cannot authorize a successor. The daemon authorizes person decisions; the private native
//! socket shares the existing trusted operating-system user boundary.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prompt {
    pub kind: String,
    pub choices: Vec<Choice>,
    pub next_action: Option<String>,
    pub how: Option<String>,
    pub at_ms: Option<u64>,
    pub by: Option<String>,
    pub harness: String,
    pub incarnation: String,
    pub runtime_incarnation: String,
    pub session_id: String,
    pub prompt_id: String,
    pub episode: String,
    pub content: String,
    pub reason: String,
    pub expires_at_ms: u64,
    pub endpoint: Option<String>,
    pub capability: String,
    pub state: String,
    pub disposition: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub episode: String,
    pub prompt_id: String,
    pub runtime_incarnation: String,
    pub answer_id: String,
}

impl Prompt {
    /// Only a qualified adapter may offer structured answers. An unfamiliar capture
    /// keeps its real content but must never inherit this adapter's approve/deny UI.
    pub fn can_respond(&self) -> bool {
        self.state == "open"
            && self.kind == "permission"
            && self.harness == "claude"
            && self.capability == "claude-permission-hook"
            && self.endpoint.is_some()
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(
                self.state.as_str(),
                "open" | "answered" | "cancelled" | "timed_out" | "ended" | "unavailable"
            ),
            "invalid prompt state"
        );
        for identity in [
            &self.kind,
            &self.harness,
            &self.incarnation,
            &self.runtime_incarnation,
            &self.session_id,
            &self.prompt_id,
            &self.episode,
            &self.capability,
        ] {
            ensure!(
                !identity.is_empty()
                    && identity.len() <= 256
                    && !identity.chars().any(char::is_control),
                "invalid prompt identity"
            );
        }
        ensure!(
            self.content.len() <= 65_536 && self.reason.len() <= 4096 && self.choices.len() <= 32,
            "prompt exceeds capture bounds"
        );
        let mut ids = std::collections::BTreeSet::new();
        for choice in &self.choices {
            ensure!(
                !choice.id.is_empty() && choice.id.len() <= 256 && ids.insert(&choice.id),
                "invalid prompt choice identity"
            );
            ensure!(
                choice.label.len() <= 4096 && choice.consequence.len() <= 4096,
                "prompt choice exceeds bounds"
            );
        }
        ensure!(
            self.state == "open" || self.endpoint.is_none(),
            "closed prompt has a response endpoint"
        );
        Ok(())
    }
}

/// Reading one bounded line leaves the connection open for a real native result.
fn read_line(stream: &UnixStream) -> Result<Value> {
    use std::io::Read as _;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut bytes = Vec::new();
    BufReader::new(stream.take(16_385)).read_until(b'\n', &mut bytes)?;
    ensure!(
        bytes.len() <= 16_384 && bytes.last() == Some(&b'\n'),
        "invalid permission frame"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn send_answer(endpoint: &str, answer: &Answer) -> Result<Value> {
    let mut stream = UnixStream::connect(endpoint)?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    serde_json::to_writer(&mut stream, answer)?;
    stream.write_all(b"\n")?;
    read_line(&stream)
}

pub struct Invocation {
    pub prompt: Prompt,
    listener: UnixListener,
    _directory: tempfile::TempDir,
    agent_dir: PathBuf,
    answered: bool,
}

impl Invocation {
    pub fn open(agent_dir: &Path, mut prompt: Prompt) -> Result<Self> {
        prompt.validate()?;
        // A short private path also works when the seat's directory exceeds sun_path.
        let directory = tempfile::Builder::new()
            .prefix("st-permission-")
            .tempdir_in("/tmp")?;
        let path = directory.path().join("reply");
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        prompt.endpoint = Some(path.to_string_lossy().into_owned());
        prompt.state = "open".into();
        let invocation = Self {
            prompt,
            listener,
            _directory: directory,
            agent_dir: agent_dir.into(),
            answered: false,
        };
        invocation.publish()?;
        Ok(invocation)
    }

    pub fn publish(&self) -> Result<()> {
        crate::harness_events::write_prompt(&self.agent_dir, &serde_json::to_value(&self.prompt)?)
    }

    pub fn finish(&mut self, disposition: &str) -> Result<()> {
        self.prompt.state = match disposition {
            "timed_out" => "timed_out",
            "cancelled" => "cancelled",
            "ended" => "ended",
            "unavailable" => "unavailable",
            _ => "answered",
        }
        .into();
        self.prompt.at_ms = Some(crate::message::now_ms());
        self.prompt.disposition = Some(disposition.into());
        self.prompt.endpoint = None;
        self.publish()
    }

    /// The provider loop owns this call and commits the decision through its native protocol.
    /// There is no separate writer thread that could race provider cancellation.
    pub fn receive(&mut self) -> Result<Option<(Answer, UnixStream)>> {
        let (mut stream, _) = match self.listener.accept() {
            Ok(peer) => peer,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let value = match read_line(&stream) {
            Ok(value) => value,
            Err(_) => {
                let _ = Self::result(&mut stream, false, "invalid-permission-answer");
                return Ok(None);
            }
        };
        // A successful connect is prompt visibility evidence; a probe never authorizes an action.
        if value["type"] == "observe" {
            return Ok(None);
        }
        let answer: Answer = match serde_json::from_value(value) {
            Ok(answer) => answer,
            Err(_) => {
                let _ = Self::result(&mut stream, false, "invalid-permission-answer");
                return Ok(None);
            }
        };
        let code = if answer.episode != self.prompt.episode
            || answer.prompt_id != self.prompt.prompt_id
            || answer.runtime_incarnation != self.prompt.runtime_incarnation
        {
            Some("stale-permission-prompt")
        } else if self.answered || self.prompt.state != "open" {
            Some("permission-already-answered")
        } else if crate::message::now_ms() >= self.prompt.expires_at_ms {
            Some("permission-expired")
        } else if !matches!(answer.answer_id.as_str(), "approve" | "deny") {
            Some("invalid-permission-answer")
        } else {
            None
        };
        if let Some(code) = code {
            let _ = Self::result(&mut stream, false, code);
            return Ok(None);
        }
        self.answered = true;
        Ok(Some((answer, stream)))
    }

    pub fn result(stream: &mut UnixStream, accepted: bool, code: &str) -> Result<()> {
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        serde_json::to_writer(&mut *stream, &json!({"accepted":accepted,"code":code}))?;
        stream.write_all(b"\n")?;
        Ok(())
    }
}

pub fn visible(endpoint: &str) -> bool {
    let Ok(mut stream) = UnixStream::connect(endpoint) else {
        return false;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
    stream.write_all(b"{\"type\":\"observe\"}\n").is_ok()
}

/// Claude provides no PermissionRequest tool ID: the invocation itself owns the response.
/// Never use a turn's prompt_id as a unique tool ID, or hash identical commands into one request.
pub fn claude_prompt(
    agent_dir: &Path,
    incarnation: &str,
    payload: &Value,
    ttl: Duration,
) -> Result<Prompt> {
    let session = payload["session_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("permission needs native session"))?;
    let tool = payload["tool_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("permission needs tool"))?;
    ensure!(
        !matches!(tool, "AskUserQuestion" | "EnterPlanMode" | "ExitPlanMode"),
        "questions and plans need their own native answer mapping"
    );
    let input = payload
        .get("tool_input")
        .ok_or_else(|| anyhow::anyhow!("permission needs exact tool input"))?;
    let episode = crate::harness_state::session_token();
    Ok(Prompt {
        kind: "permission".into(),
        choices: vec![
            Choice {
                id: "approve".into(),
                label: "Approve".into(),
                consequence: "Allow this action once through the Claude permission hook.".into(),
            },
            Choice {
                id: "deny".into(),
                label: "Deny".into(),
                consequence: "Deny this action through the Claude permission hook.".into(),
            },
        ],
        next_action: None,
        how: None,
        at_ms: None,
        by: None,
        harness: "claude".into(),
        incarnation: incarnation.into(),
        runtime_incarnation: crate::harness_events::prompt_runtime(agent_dir, incarnation)?,
        session_id: session.into(),
        prompt_id: format!("hook:{episode}"),
        episode,
        content: input["command"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or(serde_json::to_string(input)?),
        reason: payload["reason"].as_str().unwrap_or(tool).into(),
        expires_at_ms: crate::message::now_ms().saturating_add(ttl.as_millis() as u64),
        endpoint: None,
        capability: "claude-permission-hook".into(),
        state: "open".into(),
        disposition: None,
    })
}

pub fn claude_output(answer_id: &str) -> Result<Value> {
    ensure!(
        matches!(answer_id, "approve" | "deny"),
        "invalid permission decision"
    );
    let decision = if answer_id == "approve" {
        json!({"behavior":"allow"})
    } else {
        json!({"behavior":"deny","message":"The owning person denied this request."})
    };
    Ok(json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":decision}}))
}

/// Claude reads exactly this hook's stdout. A successful reply means the native output was
/// written; Claude's hook protocol has no separate provider acknowledgement. Nothing modifies
/// tool input, installs an allow rule or treats a bypass setting as a person answer.
pub fn run_claude(
    agent_dir: &Path,
    incarnation: &str,
    payload: &Value,
    ttl: Duration,
    output: &mut dyn Write,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    use std::os::fd::AsRawFd as _;
    let prompt = claude_prompt(agent_dir, incarnation, payload, ttl)?;
    let mut invocation = Invocation::open(agent_dir, prompt)?;
    loop {
        if cancelled()
            || crate::harness_events::prompt_runtime(agent_dir, incarnation)
                .ok()
                .as_deref()
                != Some(invocation.prompt.runtime_incarnation.as_str())
        {
            invocation.finish("unavailable")?;
            return Ok(());
        }
        if crate::message::now_ms() >= invocation.prompt.expires_at_ms {
            // This is an observed adapter deadline, not a person's answer. Deny the tool
            // through the supported hook rather than delegating expiry to provider defaults.
            let result = (|| -> Result<()> {
                serde_json::to_writer(&mut *output, &json!({"hookSpecificOutput":{
                    "hookEventName":"PermissionRequest","decision":{
                        "behavior":"deny",
                        "message":"The response deadline elapsed without an owning-person decision."
                    }
                }}))?;
                output.write_all(b"\n")?;
                output.flush()?;
                Ok(())
            })();
            if result.is_err() {
                invocation.finish("unavailable")?;
                return result;
            }
            invocation.prompt.how =
                Some("The native hook response deadline elapsed; a system denial was sent through the hook.".into());
            invocation.finish("timed_out")?;
            return Ok(());
        }
        if let Some((answer, mut stream)) = invocation.receive()? {
            let result = (|| -> Result<()> {
                serde_json::to_writer(&mut *output, &claude_output(&answer.answer_id)?)?;
                output.write_all(b"\n")?;
                output.flush()?;
                Ok(())
            })();
            if result.is_err() {
                invocation.finish("unavailable")?;
                let _ = Invocation::result(&mut stream, false, "permission-output-failed");
                return result;
            }
            invocation.prompt.how = Some(format!(
                "{} sent through the Claude PermissionRequest hook",
                answer.answer_id
            ));
            invocation.finish(&answer.answer_id)?;
            Invocation::result(&mut stream, true, "native-output-written")?;
            return Ok(());
        }
        let mut descriptor = libc::pollfd {
            fd: invocation.listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor lives for the bounded poll; the listener retains its fd.
        unsafe {
            libc::poll(&mut descriptor, 1, 100);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub id: String,
    pub label: String,
    pub consequence: String,
}

impl Drop for Invocation {
    fn drop(&mut self) {
        if self.prompt.state == "open" {
            let _ = self.finish("unavailable");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Instant;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        crate::harness_events::enable(dir.path(), "runtime-a").unwrap();
        crate::harness_events::write_snapshot(
            dir.path(),
            "harness-state",
            &serde_json::to_vec(&json!({"incarnation":"provider-a","harness":"claude"})).unwrap(),
        )
        .unwrap();
        dir
    }
    fn payload() -> Value {
        json!({"session_id":"session-a","tool_name":"Bash","tool_input":{"command":"printf fixture"}})
    }
    fn live(dir: &Path) -> Prompt {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(prompt) = crate::harness_events::live_prompts(dir, "runtime-a")
                .unwrap()
                .into_iter()
                .next()
            {
                return prompt;
            }
            assert!(
                Instant::now() < deadline,
                "native invocation was not published"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn answer(p: &Prompt, id: &str) -> Answer {
        Answer {
            episode: p.episode.clone(),
            prompt_id: p.prompt_id.clone(),
            runtime_incarnation: p.runtime_incarnation.clone(),
            answer_id: id.into(),
        }
    }
    fn final_prompt(dir: &Path) -> Prompt {
        let events = crate::harness_events::pending(dir, 32).unwrap();
        let mut value = events
            .into_iter()
            .rev()
            .find(|e| e.kind == "harness-prompt")
            .unwrap()
            .payload;
        value.as_object_mut().unwrap().remove("account_ref");
        serde_json::from_value(value).unwrap()
    }
    #[test]
    fn native_hook_answers_once_and_refuses_stale_identity() {
        for id in ["approve", "deny"] {
            let dir = fixture();
            let path = dir.path().to_owned();
            let worker = std::thread::spawn(move || {
                let mut output = Vec::new();
                run_claude(
                    &path,
                    "provider-a",
                    &payload(),
                    Duration::from_secs(10),
                    &mut output,
                    &|| false,
                )
                .unwrap();
                output
            });
            let p = live(dir.path());
            assert_eq!(p.content, "printf fixture");
            let endpoint = p.endpoint.as_deref().unwrap();
            assert!(visible(endpoint));
            let mut stale = answer(&p, id);
            stale.runtime_incarnation = "runtime-old".into();
            assert_eq!(
                send_answer(endpoint, &stale).unwrap()["code"],
                "stale-permission-prompt"
            );
            assert_eq!(
                send_answer(endpoint, &answer(&p, id)).unwrap()["accepted"],
                true
            );
            let output: Value = serde_json::from_slice(&worker.join().unwrap()).unwrap();
            assert_eq!(output, claude_output(id).unwrap());
            assert!(send_answer(endpoint, &answer(&p, id)).is_err());
            assert!(
                crate::harness_events::live_prompts(dir.path(), "runtime-a")
                    .unwrap()
                    .is_empty()
            );
            let closed = final_prompt(dir.path());
            assert_eq!(closed.state, "answered");
            assert_eq!(closed.disposition.as_deref(), Some(id));
        }
    }
    #[test]
    fn deadline_denies_without_a_person_answer_and_disappearance_sends_no_decision() {
        let dir = fixture();
        let mut output = Vec::new();
        run_claude(
            dir.path(),
            "provider-a",
            &payload(),
            Duration::ZERO,
            &mut output,
            &|| false,
        )
        .unwrap();
        let expired_output: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(expired_output["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert!(expired_output["hookSpecificOutput"]["decision"]["message"]
            .as_str().unwrap().contains("without an owning-person decision"));
        let expired = final_prompt(dir.path());
        assert_eq!(expired.state, "timed_out");
        assert_eq!(expired.disposition.as_deref(), Some("timed_out"));
        assert!(expired.by.is_none());
        // A failed output is uncertain delivery, never a fabricated denial or timeout receipt.
        struct BrokenOutput;
        impl Write for BrokenOutput {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        assert!(run_claude(dir.path(), "provider-a", &payload(), Duration::ZERO,
            &mut BrokenOutput, &|| false).is_err());
        assert_eq!(final_prompt(dir.path()).state, "unavailable");
        let cancelled = Arc::new(AtomicBool::new(false));
        let native_cancelled = cancelled.clone();
        let path = dir.path().to_owned();
        let worker = std::thread::spawn(move || {
            let mut output = Vec::new();
            run_claude(
                &path,
                "provider-a",
                &payload(),
                Duration::from_secs(10),
                &mut output,
                &|| native_cancelled.load(Ordering::SeqCst),
            )
            .unwrap();
            output
        });
        let p = live(dir.path());
        cancelled.store(true, Ordering::SeqCst);
        assert!(worker.join().unwrap().is_empty());
        assert!(!visible(p.endpoint.as_deref().unwrap()));
        let closed = final_prompt(dir.path());
        assert_eq!(closed.state, "unavailable");
        assert!(closed.how.is_none());
    }
    #[test]
    fn identical_commands_have_independent_invocations_and_questions_are_not_permissions() {
        let dir = fixture();
        let a =
            claude_prompt(dir.path(), "provider-a", &payload(), Duration::from_secs(1)).unwrap();
        let b =
            claude_prompt(dir.path(), "provider-a", &payload(), Duration::from_secs(1)).unwrap();
        assert_ne!(a.episode, b.episode);
        assert_ne!(a.prompt_id, b.prompt_id);
        assert!(
            claude_prompt(
                dir.path(),
                "provider-old",
                &payload(),
                Duration::from_secs(1)
            )
            .is_err()
        );
        assert!(
            claude_prompt(
                dir.path(),
                "provider-a",
                &json!({"session_id":"session-a","tool_name":"AskUserQuestion","tool_input":{}}),
                Duration::from_secs(1)
            )
            .is_err()
        );
    }

    #[test]
    fn current_driver_retires_prompt_after_provider_replacement_but_old_runtime_cannot() {
        let dir = fixture();
        let mut invocation = Invocation::open(
            dir.path(),
            claude_prompt(
                dir.path(),
                "provider-a",
                &payload(),
                Duration::from_secs(10),
            )
            .unwrap(),
        )
        .unwrap();
        crate::harness_events::write_snapshot(
            dir.path(),
            "harness-state",
            &serde_json::to_vec(&json!({"incarnation":"provider-b","harness":"claude"})).unwrap(),
        )
        .unwrap();
        assert!(invocation.publish().is_err());
        invocation.prompt.state = "unavailable".into();
        invocation.prompt.endpoint = None;
        invocation.prompt.disposition = Some("unavailable".into());
        crate::harness_events::write_prompt_unavailable(dir.path(), &invocation.prompt).unwrap();
        assert!(
            crate::harness_events::live_prompts(dir.path(), "runtime-a")
                .unwrap()
                .is_empty()
        );
        assert_eq!(final_prompt(dir.path()).state, "unavailable");
        crate::harness_events::enable(dir.path(), "runtime-b").unwrap();
        assert!(
            crate::harness_events::write_prompt_unavailable(dir.path(), &invocation.prompt)
                .is_err()
        );
    }
}
