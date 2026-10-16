//! Built-in declarations use ordinary person publication and replicated run history.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use kdl::{KdlDocument, KdlEntry, KdlNode};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use st3::client::Client;
use st3::config::Config;
use st3::model::{
    ApplyRequest, ApplyResponse, DocumentListResponse, DocumentPutRequest, IntentInput,
    MissionRequest, MissionResponse, MissionRunView, StatusResponse,
};

pub const ASSISTANT_SUBJECT: &str = "agent/st/assistant";
/// The seat's subject is durable; people see this name.
const ASSISTANT_NAME: &str = "Smalltalk Assistant";
const MISSION: &str = "st/onboarding";
const ASSISTANT: &str = include_str!("builtins/assistant.kdl");
const ONBOARDING: &str = include_str!("builtins/onboarding.kdl");
const GUIDE: &str = include_str!("../../../docs/st3/onboarding-guide.md");

type Sink = Box<dyn FnMut(String)>;

thread_local! {
    /// Where this thread's setup and onboarding output goes instead of the terminal, while the
    /// setup screen owns it. The caller prints what was said once the screen has gone.
    static SINK: std::cell::RefCell<Option<Sink>> = const { std::cell::RefCell::new(None) };
}

/// Say one line to the person: on the terminal, or into the sink when one is set.
pub(crate) fn said(line: impl Into<String>) {
    let line = line.into();
    let unsent = SINK.with(|sink| match sink.borrow_mut().as_mut() {
        Some(send) => {
            send(line);
            None
        }
        None => Some(line),
    });
    if let Some(line) = unsent {
        println!("{line}");
    }
}

/// Set (or clear) this thread's sink; returns the one it replaced.
pub(crate) fn capture(sink: Option<Sink>) -> Option<Sink> {
    SINK.with(|current| std::mem::replace(&mut *current.borrow_mut(), sink))
}

fn client(config: &Config) -> Result<Client> {
    Client::unix_as(
        config.client_socket(),
        config
            .person
            .as_deref()
            .context("onboarding needs a configured person")?,
    )
}

/// Count every historical run, including completed and cancelled runs on another member.
// Not called in the binary; it was a public library function.
#[allow(dead_code)]
pub async fn has_run(config: &Config) -> Result<bool> {
    has_run_on(&client(config)?).await
}

async fn has_run_on(client: &Client) -> Result<bool> {
    match client
        .get::<Value>("/v1/mission-overview?mission=st%2Fonboarding")
        .await
    {
        Ok(overview) => Ok(overview["total_runs"]
            .as_u64()
            .context("onboarding history has no run count")?
            > 0),
        // The named mission has no overview yet: nothing has run.
        Err(error) if st3::client::is_not_found(&error) => Ok(false),
        // An older daemon without the overview route cannot say, and guessing "never" would
        // start a second onboarding on an install that already finished one.
        Err(error) if st3::client::is_missing_route(&error) => Err(error.context(
            "the running daemon is older than this st and cannot say whether onboarding already ran; restart it, or run st setup --onboarding to begin explicitly",
        )),
        Err(error) => Err(error),
    }
}

/// The harnesses the mission tells the Assistant are ready: the one it runs on, then any other
/// that setup found, selected and set up.
fn ready_phrase(harness: &str, ready: &[String]) -> String {
    let mut names: Vec<&str> = vec![harness];
    for name in ready {
        if !names.contains(&name.as_str()) {
            names.push(name);
        }
    }
    match names.as_slice() {
        [only] => format!("{only} (you run on it)"),
        [first, rest @ ..] => {
            let (last, middle) = rest.split_last().expect("two or more names");
            let mut text = (*first).to_string();
            for name in middle {
                text.push_str(", ");
                text.push_str(name);
            }
            format!("{text} and {last} (you run on {first})")
        }
        [] => unreachable!("the Assistant's own harness is always listed"),
    }
}

/// The onboarding mission with its pinned guide and ready harnesses filled in.
fn mission_source(guide_hash: &str, harness: &str, ready: &[String]) -> String {
    ONBOARDING
        .replace("@GUIDE@", &format!("doc/st/guide@{guide_hash}"))
        .replace("@HARNESSES@", &ready_phrase(harness, ready))
}

/// Explicit reruns are the only setup path that can replace a stopped built-in seat. `ready` names
/// every harness setup selected and set up, so the Assistant can run its demo on two.
pub async fn start(
    config: &Config,
    harness: &str,
    ready: &[String],
    rerun: bool,
) -> Result<Option<String>> {
    let client = client(config)?;
    if !rerun && has_run_on(&client).await? {
        return Ok(None);
    }
    if rerun {
        let active: Vec<MissionRunView> = client
            .get("/v1/mission-runs?mission=st%2Fonboarding")
            .await?;
        anyhow::ensure!(
            active.is_empty(),
            "onboarding is already active; finish or cancel its run before using --onboarding"
        );
    }
    let existing = assistant_status(&client).await?;
    if !rerun
        && existing.as_ref().is_some_and(|subject| {
            subject.desired_token.is_some() && subject.kind.as_deref() != Some("agent")
        })
    {
        said(
            "The built-in Smalltalk Assistant is stopped; setup leaves it stopped. Use st setup --onboarding to begin again explicitly."
        );
        return Ok(None);
    }
    let versions: DocumentListResponse = client
        .get("/v1/documents?name=doc%2Fst%2Fguide&limit=1")
        .await?;
    let hash = hex::encode(Sha256::digest(GUIDE.as_bytes()));
    if !versions.items.iter().any(|version| version.hash == hash) {
        let _: st3::model::DocumentVersion = client
            .post(
                "/v1/documents",
                &DocumentPutRequest {
                    name: "doc/st/guide".into(),
                    bytes: GUIDE.as_bytes().to_vec(),
                    expected_document: versions
                        .items
                        .first()
                        .map(|version| version.binding_claim_id.clone()),
                    idempotency_key: format!("onboarding-guide:{hash}"),
                },
            )
            .await?;
    }
    let mission_source = mission_source(&hash, harness, ready);
    let parsed = st3::parse_intent(&mission_source, &config.node)?;
    let revision = &parsed
        .missions
        .get(MISSION)
        .context("built-in onboarding mission is missing")?
        .revision;
    let workspace = workspace(config)?;
    std::fs::create_dir_all(&workspace)?;
    let run_id = if rerun {
        format!("{MISSION}/{}", uuid::Uuid::now_v7())
    } else {
        MISSION.into()
    };
    let source = declarations(
        harness,
        &workspace,
        &run_id,
        revision,
        config.person.as_deref().unwrap(),
    )?;
    // A mission needs its eligible seat, and a run needs a published mission revision.
    // Publish the definitions together, then the deterministically named initial run.
    let mut definitions: KdlDocument = source.parse()?;
    definitions
        .nodes_mut()
        .retain(|node| node.name().value() != "mission-run");
    if !rerun
        && existing
            .as_ref()
            .is_some_and(|subject| subject.desired_token.is_some())
    {
        definitions
            .nodes_mut()
            .retain(|node| node.name().value() != "agent");
    }
    let mission_document: KdlDocument = mission_source.parse()?;
    definitions.nodes_mut().extend(
        mission_document
            .nodes()
            .iter()
            .filter(|node| node.name().value() != "version")
            .cloned(),
    );
    definitions.autoformat();
    let mut definition_preview = preview(&client, &definitions.to_string()).await?;
    if definitions.get("agent").is_some() {
        let expected = existing
            .as_ref()
            .and_then(|subject| subject.desired_token.clone())
            .into_iter()
            .collect::<Vec<_>>();
        if definition_preview.subject_tokens.get(ASSISTANT_SUBJECT) != Some(&expected) {
            return declaration_changed(rerun);
        }
        definition_preview
            .subject_tokens
            .insert(ASSISTANT_SUBJECT.into(), expected);
    }
    if let Err(error) = apply(&client, definition_preview, config).await {
        if !rerun && st3::client::http_status(&error) == Some(409) {
            return declaration_changed(false);
        }
        return Err(error);
    }
    if !rerun && has_run_on(&client).await? {
        return Ok(None);
    }
    let current = assistant_status(&client)
        .await?
        .context("built-in Smalltalk Assistant disappeared during publication")?;
    if current.kind.as_deref() != Some("agent") {
        said("The built-in Smalltalk Assistant was stopped; setup leaves it stopped.");
        return Ok(None);
    }
    anyhow::ensure!(
        current.conflicts.is_empty(),
        "the built-in Smalltalk Assistant has conflicting declarations"
    );
    let token = current
        .desired_token
        .context("the built-in Smalltalk Assistant has no declaration")?;
    let claim: st3::model::ClaimRecord = client.get(&format!("/v1/claims/by-id/{token}")).await?;
    let desired: st3::model::DesiredSubject = serde_json::from_value(claim.body)?;
    let mut agent = st3::graph::render_desired_node(&desired.desired)?;
    // Typed harnesses require a body even when the provider has no default flags.
    if let Some(body) = agent.children_mut()
        && let Some(harness) = body.get_mut("harness")
        && harness.children().is_none()
    {
        harness.set_children(KdlDocument::new());
    }
    let mut run: KdlDocument = source.parse()?;
    run.nodes_mut()
        .retain(|node| node.name().value() != "agent");
    // Including the unchanged declaration makes the ordinary publication fence cover
    // the seat as well as the run, while preserving any person-authored customization.
    run.nodes_mut().push(agent);
    run.autoformat();
    let mut preview = preview(&client, &run.to_string()).await?;
    let expected = vec![token];
    if preview.subject_tokens.get(ASSISTANT_SUBJECT) != Some(&expected) {
        return declaration_changed(rerun);
    }
    preview
        .subject_tokens
        .insert(ASSISTANT_SUBJECT.into(), expected);
    let result = apply(&client, preview, config).await;
    if let Err(error) = result {
        // A concurrent first setup may have won the same named run's publication fence.
        if !rerun && st3::client::http_status(&error) == Some(409) {
            return declaration_changed(false);
        }
        return Err(error);
    }
    said(
        "Started mission/st/onboarding with agent/st/assistant. The Assistant stays available after onboarding."
    );
    Ok(Some(ASSISTANT_SUBJECT.into()))
}

fn declaration_changed(rerun: bool) -> Result<Option<String>> {
    anyhow::ensure!(
        !rerun,
        "the built-in Smalltalk Assistant changed during setup; retry setup to inspect its new state"
    );
    said(
        "The onboarding declarations changed concurrently; ordinary setup preserves their current state. Run st setup again if onboarding has not started."
    );
    Ok(None)
}

async fn assistant_status(client: &Client) -> Result<Option<st3::model::SubjectStatus>> {
    let status: StatusResponse = client.get("/v1/status?subject=agent%2Fst%2Fassistant").await?;
    Ok(status
        .subjects
        .into_iter()
        .find(|subject| subject.subject == ASSISTANT_SUBJECT))
}

pub(crate) fn workspace(config: &Config) -> Result<PathBuf> {
    Ok(config.work_root()?.join("agents/st-assistant"))
}

fn declarations(
    harness: &str,
    workspace: &std::path::Path,
    run_id: &str,
    revision: &str,
    person: &str,
) -> Result<String> {
    let template: KdlDocument = ASSISTANT.parse()?;
    let description = template
        .get("agent")
        .and_then(KdlNode::children)
        .and_then(|body| body.get("description"))
        .and_then(|node| node.get(0))
        .and_then(|value| value.as_string())
        .context("built-in Smalltalk Assistant description is missing")?;
    let args = st3_client::AgentCreateParameters {
        name: "st/assistant".into(),
        harness: harness.into(),
        description: Some(description.into()),
        host: None,
        model: None,
        effort: None,
        workspace: None,
        message: None,
        repo: None,
        base: None,
        branch: None,
        remove_at_run_end: None,
    };
    let mut document: KdlDocument =
        st3::creation::agent_document(&args, &workspace.to_string_lossy(), false, None)
            .parse()?;
    let mut display_name = KdlNode::new("name");
    display_name.entries_mut().push(KdlEntry::new(ASSISTANT_NAME));
    document
        .nodes_mut()
        .iter_mut()
        .find(|node| node.name().value() == "agent")
        .and_then(|node| node.children_mut().as_mut())
        .context("built-in Smalltalk Assistant declaration has no body")?
        .nodes_mut()
        .insert(0, display_name);
    let mut run = KdlNode::new("mission-run");
    run.entries_mut().push(KdlEntry::new(run_id));
    let mut body = KdlDocument::new();
    for (key, value) in [
        ("mission", format!("mission/{MISSION}@{revision}")),
        ("workspace", workspace.to_string_lossy().into_owned()),
        ("requester", person.into()),
    ] {
        let mut node = KdlNode::new(key);
        node.entries_mut().push(KdlEntry::new(value));
        body.nodes_mut().push(node);
    }
    run.set_children(body);
    document.nodes_mut().push(run);
    document.autoformat();
    Ok(document.to_string())
}

async fn preview(client: &Client, source: &str) -> Result<MissionResponse> {
    let response: MissionResponse = client
        .post(
            "/v1/intent/mission",
            &MissionRequest {
                intent: IntentInput {
                    kdl: source.into(),
                    source_name: Some("st setup built-ins".into()),
                },
                at_index: None,
            },
        )
        .await?;
    anyhow::ensure!(
        response.blockers.is_empty(),
        "{}",
        response.blockers.join("; ")
    );
    Ok(response)
}

async fn apply(
    client: &Client,
    preview: MissionResponse,
    config: &Config,
) -> Result<ApplyResponse> {
    let mut hash = Sha256::new();
    hash.update(preview.resolved_intent.kdl.as_bytes());
    hash.update(serde_json::to_vec(&preview.subject_tokens)?);
    client
        .post(
            "/v1/intent/apply",
            &ApplyRequest {
                intent: preview.resolved_intent,
                expected_subjects: preview.subject_tokens,
                idempotency_key: hex::encode(hash.finalize()),
                actor: config.person.clone(),
            },
        )
        .await
}

#[cfg(test)]
mod tests {
    #[test]
    fn assistant_uses_shared_harness_defaults_and_pins_the_guide() {
        assert!(
            super::GUIDE.len() <= 16_384,
            "keep the bundled guide bounded"
        );
        let guide = hex::encode(sha2::Sha256::digest(super::GUIDE.as_bytes()));
        let ready = ["claude".to_string(), "codex".to_string()];
        let mission = st3::parse_intent(&super::mission_source(&guide, "claude", &ready), "studio")
            .unwrap();
        assert!(
            mission
                .document_refs
                .contains(&format!("doc/st/guide@{guide}"))
        );
        let revision = &mission.missions[super::MISSION].revision;
        for harness in st3::environment::HARNESSES {
            let source = super::declarations(
                harness,
                std::path::Path::new("/work/st-assistant"),
                super::MISSION,
                revision,
                "person/ada",
            )
            .unwrap();
            let intent = st3::parse_intent(&source, "studio").unwrap();
            assert!(intent.subjects.contains_key(super::ASSISTANT_SUBJECT));
            assert!(!source.contains("model "));
            assert!(source.contains("name \"Smalltalk Assistant\""));
            assert!(source.contains(&format!("mission/st/onboarding@{revision}")));
            assert!(
                mission.missions[super::MISSION]
                    .constraints
                    .iter()
                    .any(|text| text.contains(&guide))
            );
            // The install access mode, not the declaration, decides the permission flags.
            assert!(!source.contains("--dangerously-skip-permissions"), "{source}");
            assert!(!source.contains("--dangerously-bypass-approvals-and-sandbox"), "{source}");
        }
    }
    #[test]
    fn no_onboarding_text_calls_the_first_agent_an_expert() {
        for text in [super::ASSISTANT, super::ONBOARDING, super::GUIDE] {
            assert!(!text.to_lowercase().contains("expert"));
        }
    }
    #[test]
    fn the_mission_names_the_ready_harnesses_so_act_one_can_use_both() {
        let both = ["claude".to_string(), "codex".to_string()];
        assert_eq!(
            super::ready_phrase("claude", &both),
            "claude and codex (you run on claude)"
        );
        // The Assistant's own harness leads, whatever order setup finished them in.
        assert_eq!(
            super::ready_phrase("codex", &both),
            "codex and claude (you run on codex)"
        );
        assert_eq!(super::ready_phrase("codex", &[]), "codex (you run on it)");
        assert_eq!(
            super::ready_phrase("claude", &["claude".to_string()]),
            "claude (you run on it)"
        );
        let source = super::mission_source("abc", "claude", &both);
        assert!(
            !source.contains("@GUIDE@") && !source.contains("@HARNESSES@"),
            "an unfilled placeholder is left: {source}"
        );
        assert!(source.contains("claude and codex (you run on claude)"));
    }
    #[test]
    fn the_play_is_three_acts_spoken_to_you_with_an_alert_in_act_three() {
        let guide = hex::encode(sha2::Sha256::digest(super::GUIDE.as_bytes()));
        let mission =
            st3::parse_intent(&super::mission_source(&guide, "claude", &[]), "studio").unwrap();
        assert!(mission.missions.contains_key(super::MISSION));
        for act in ["hello", "act-one", "act-two", "act-three"] {
            assert!(
                super::ONBOARDING.contains(&format!("step \"{act}\"")),
                "the play lost {act}"
            );
        }
        // Act three raises the alert before the interview, and the interview ends onboarding.
        let alert = super::GUIDE.find("Raise one alert").expect("act three raises an alert");
        let interview = super::GUIDE
            .find("What do you want to accomplish in Smalltalk today?")
            .expect("act three ends in the interview");
        assert!(alert < interview, "the alert comes before the interview");
        assert!(super::GUIDE.contains("above the message input"));
        assert!(super::GUIDE.contains("conversations wait"), "the guide must pace with wait");
        assert!(super::GUIDE.contains("setup --onboarding"), "skip must say how to return");
        // The play has none of what Nathan dropped.
        for dropped in ["canonical", "permission mode", "macOS permission"] {
            assert!(
                !super::GUIDE.to_lowercase().contains(dropped)
                    && !super::ONBOARDING.to_lowercase().contains(dropped),
                "{dropped:?} is not part of the play"
            );
        }
        // The demo mission the guide hands the Assistant must be publishable as written.
        let block = super::GUIDE
            .split("```kdl\n")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("the guide carries the demo mission");
        let demo = st3::parse_intent(block, "studio").unwrap();
        assert!(demo.missions.contains_key("st/onboarding-demo"));
    }
    /// The words the 2026-10-09 eval found between tool calls. The instructions that tell the
    /// Assistant how to speak are the only place they may appear, and only as things not to say.
    const NARRATION: [&str; 8] = [
        "the person",
        "mission step",
        "work step",
        "the user",
        "i need the",
        "let me read",
        "still working through",
        "attention item",
    ];
    #[test]
    fn what_the_person_experiences_is_written_to_them_and_leaks_no_narration() {
        // Goals are what they live through, so each speaks to "you" or about the Assistant.
        let goals: Vec<&str> = super::ONBOARDING
            .lines()
            .filter(|line| line.trim_start().starts_with("goal \""))
            .collect();
        assert!(goals.len() >= 6, "{goals:?}");
        for goal in &goals {
            let lowered = goal.to_lowercase();
            for banned in NARRATION.iter().chain(&["home", "gate"]) {
                assert!(!lowered.contains(banned), "{banned:?} in {goal}");
            }
        }
        // The seat text addresses the Assistant as "you" and says what never to write.
        let seat = super::ASSISTANT.to_lowercase();
        for required in [
            "speak to them as you",
            "write nothing between tool calls that is not addressed to them",
            "never refer to them in the third person",
            "never send them to home",
        ] {
            assert!(seat.contains(required), "the seat text lost: {required}");
        }
        assert!(!seat.contains("the person"), "the seat text speaks of them in the third person");
        // The guide carries the same rule and names no sample person.
        let guide = super::GUIDE.to_lowercase();
        assert!(guide.contains("write nothing between tool calls that is not addressed to them"));
        assert!(!guide.contains("hi avery"), "the guide greets nobody by an example name");
    }
    use sha2::Digest as _;
}
