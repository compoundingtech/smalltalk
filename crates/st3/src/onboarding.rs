//! Built-in declarations use ordinary person publication and replicated run history.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use kdl::{KdlDocument, KdlEntry, KdlNode};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::client::Client;
use crate::config::Config;
use crate::model::{
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
/// The reusable example missions the Assistant offers in act three: each is stored as a document
/// the mission pins by hash, and the Assistant applies a copy when the person says yes.
const CANONICAL: [(&str, &str, &str); 3] = [
    (
        "weekly-session-review",
        "@WEEKLY@",
        include_str!("../../../examples/st3/canonical/weekly-session-review.kdl"),
    ),
    (
        "review-pull-request",
        "@PULL_REQUEST@",
        include_str!("../../../examples/st3/canonical/review-pull-request.kdl"),
    ),
    (
        "weekly-schedule",
        "@SCHEDULE@",
        include_str!("../../../examples/st3/canonical/weekly-schedule.kdl"),
    ),
];

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
        Err(error) if crate::client::is_not_found(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Explicit reruns are the only setup path that can replace a stopped built-in seat.
pub async fn start(config: &Config, harness: &str, rerun: bool) -> Result<Option<String>> {
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
        crate::setup::said(
            "The built-in Smalltalk Assistant is stopped; setup leaves it stopped. Use st setup --onboarding to begin again explicitly."
        );
        return Ok(None);
    }
    let hash = store_document(&client, "doc/st/guide", GUIDE).await?;
    let mut mission_source = ONBOARDING.replace("@GUIDE@", &format!("doc/st/guide@{hash}"));
    for (name, placeholder, text) in CANONICAL {
        let hash = store_document(&client, &format!("doc/st/canonical/{name}"), text).await?;
        mission_source = mission_source.replace(placeholder, &format!("doc/st/canonical/{name}@{hash}"));
    }
    let parsed = crate::parse_intent(&mission_source, &config.node)?;
    let revision = &parsed
        .missions
        .get(MISSION)
        .context("built-in onboarding mission is missing")?
        .revision;
    let workspace = workspace()?;
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
        if !rerun && crate::client::http_status(&error) == Some(409) {
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
        crate::setup::said("The built-in Smalltalk Assistant was stopped; setup leaves it stopped.");
        return Ok(None);
    }
    anyhow::ensure!(
        current.conflicts.is_empty(),
        "the built-in Smalltalk Assistant has conflicting declarations"
    );
    let token = current
        .desired_token
        .context("the built-in Smalltalk Assistant has no declaration")?;
    let claim: crate::model::ClaimRecord = client.get(&format!("/v1/claims/by-id/{token}")).await?;
    let desired: crate::model::DesiredSubject = serde_json::from_value(claim.body)?;
    let mut agent = crate::graph::render_desired_node(&desired.desired)?;
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
        if !rerun && crate::client::http_status(&error) == Some(409) {
            return declaration_changed(false);
        }
        return Err(error);
    }
    crate::setup::said(
        "Started mission/st/onboarding with agent/st/assistant. The Assistant stays available after onboarding."
    );
    Ok(Some(ASSISTANT_SUBJECT.into()))
}

/// Store `text` under `name` unless its newest version already holds it; return its hash.
async fn store_document(client: &Client, name: &str, text: &str) -> Result<String> {
    let versions: DocumentListResponse = client
        .get(&format!("/v1/documents?name={}&limit=1", urlencoding::encode(name)))
        .await?;
    let hash = hex::encode(Sha256::digest(text.as_bytes()));
    if !versions.items.iter().any(|version| version.hash == hash) {
        let _: crate::model::DocumentVersion = client
            .post(
                "/v1/documents",
                &DocumentPutRequest {
                    name: name.into(),
                    bytes: text.as_bytes().to_vec(),
                    expected_document: versions
                        .items
                        .first()
                        .map(|version| version.binding_claim_id.clone()),
                    idempotency_key: format!("onboarding-document:{name}:{hash}"),
                },
            )
            .await?;
    }
    Ok(hash)
}

fn declaration_changed(rerun: bool) -> Result<Option<String>> {
    anyhow::ensure!(
        !rerun,
        "the built-in Smalltalk Assistant changed during setup; retry setup to inspect its new state"
    );
    crate::setup::said(
        "The onboarding declarations changed concurrently; ordinary setup preserves their current state. Run st setup again if onboarding has not started."
    );
    Ok(None)
}

async fn assistant_status(client: &Client) -> Result<Option<crate::model::SubjectStatus>> {
    let status: StatusResponse = client.get("/v1/status?subject=agent%2Fst%2Fassistant").await?;
    Ok(status
        .subjects
        .into_iter()
        .find(|subject| subject.subject == ASSISTANT_SUBJECT))
}

fn workspace() -> Result<PathBuf> {
    if let Some(instance) = crate::instance::current() {
        return Ok(instance.agents_dir().join("st-expert"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .context("HOME is not an absolute directory")?;
    Ok(home.join("st/agents/st-assistant"))
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
        crate::creation::agent_document(&args, &workspace.to_string_lossy(), false, None)
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
    /// The onboarding mission with every pinned document filled in, as setup publishes it.
    fn resolved_mission() -> String {
        let mut source = super::ONBOARDING.replace(
            "@GUIDE@",
            &format!("doc/st/guide@{}", hex::encode(sha2::Sha256::digest(super::GUIDE.as_bytes()))),
        );
        for (name, placeholder, text) in super::CANONICAL {
            let hash = hex::encode(sha2::Sha256::digest(text.as_bytes()));
            source = source.replace(placeholder, &format!("doc/st/canonical/{name}@{hash}"));
        }
        source
    }

    #[test]
    fn assistant_uses_shared_harness_defaults_and_pins_the_guide() {
        assert!(
            super::GUIDE.len() <= 16_384,
            "keep the bundled guide bounded"
        );
        let guide = hex::encode(sha2::Sha256::digest(super::GUIDE.as_bytes()));
        let mission = crate::parse_intent(&resolved_mission(), "studio").unwrap();
        assert!(
            mission
                .document_refs
                .contains(&format!("doc/st/guide@{guide}"))
        );
        for (name, _, text) in super::CANONICAL {
            let hash = hex::encode(sha2::Sha256::digest(text.as_bytes()));
            assert!(
                mission
                    .document_refs
                    .contains(&format!("doc/st/canonical/{name}@{hash}")),
                "the mission must pin the canonical {name}"
            );
        }
        let revision = &mission.missions[super::MISSION].revision;
        for harness in crate::environment::HARNESSES {
            let source = super::declarations(
                harness,
                std::path::Path::new("/work/st-assistant"),
                super::MISSION,
                revision,
                "person/ada",
            )
            .unwrap();
            let intent = crate::parse_intent(&source, "studio").unwrap();
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
            match *harness {
                "claude" => assert!(source.contains("--dangerously-skip-permissions")),
                "codex" => {
                    assert!(source.contains("--approve-for-me"));
                    assert!(!source.contains("--dangerously-bypass"));
                }
                _ => {}
            }
        }
    }
    #[test]
    fn no_onboarding_text_calls_the_first_agent_an_expert() {
        for text in [super::ASSISTANT, super::ONBOARDING, super::GUIDE] {
            assert!(!text.to_lowercase().contains("expert"));
        }
    }
    #[test]
    fn the_play_is_three_acts_spoken_to_you() {
        let mission = crate::parse_intent(&resolved_mission(), "studio").unwrap();
        assert!(mission.missions.contains_key(super::MISSION));
        for act in ["hello", "act-one", "act-two", "act-three"] {
            assert!(
                super::ONBOARDING.contains(&format!("step \"{act}\"")),
                "the play lost {act}"
            );
        }
        // Goals describe what the person experiences, addressed to them. Nothing the person
        // reads may send them to Home or an attention item, or name a sample person.
        let goals: Vec<&str> = super::ONBOARDING
            .lines()
            .filter(|line| line.trim_start().starts_with("goal \""))
            .collect();
        assert!(goals.len() >= 5);
        for text in goals.iter().copied().chain([super::ASSISTANT]) {
            let lowered = text.to_lowercase();
            if text.contains("agent \"st/assistant\"") {
                continue;
            }
            for banned in ["the person", " ada", "attention item to", "open home", "go to home"] {
                assert!(!lowered.contains(banned), "{banned:?} in {text}");
            }
        }
        assert!(super::GUIDE.contains("conversations wait"), "the guide must pace with wait");
        assert!(super::GUIDE.contains("setup --onboarding"), "skip must say how to return");
        assert!(!super::GUIDE.to_lowercase().contains("ada"));
        // The demo mission the guide hands the Assistant must be publishable as written.
        let block = super::GUIDE
            .split("```kdl\n")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("the guide carries the demo mission");
        let demo = crate::parse_intent(block, "studio").unwrap();
        assert!(demo.missions.contains_key("st/onboarding-demo"));
    }
    use sha2::Digest as _;
}
