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

pub const EXPERT_SUBJECT: &str = "agent/st/expert";
const MISSION: &str = "st/onboarding";
const EXPERT: &str = include_str!("builtins/expert.kdl");
const ONBOARDING: &str = include_str!("builtins/onboarding.kdl");
const GUIDE: &str = include_str!("../../../docs/st3/onboarding-guide.md");

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
    let existing = expert_status(&client).await?;
    if !rerun
        && existing.as_ref().is_some_and(|subject| {
            subject.desired_token.is_some() && subject.kind.as_deref() != Some("agent")
        })
    {
        println!(
            "The built-in expert is stopped; setup leaves it stopped. Use st setup --onboarding to begin again explicitly."
        );
        return Ok(None);
    }
    let versions: DocumentListResponse = client
        .get("/v1/documents?name=doc%2Fst%2Fguide&limit=1")
        .await?;
    let hash = hex::encode(Sha256::digest(GUIDE.as_bytes()));
    if !versions.items.iter().any(|version| version.hash == hash) {
        let _: crate::model::DocumentVersion = client
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
    let mission_source = ONBOARDING.replace("@GUIDE@", &format!("doc/st/guide@{hash}"));
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
    let claude_mode =
        crate::permission_mode::Effective::resolve(config.claude_permission_mode).mode;
    let source = declarations(
        harness,
        &workspace,
        &run_id,
        revision,
        config.person.as_deref().unwrap(),
        claude_mode,
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
        if definition_preview.subject_tokens.get(EXPERT_SUBJECT) != Some(&expected) {
            return declaration_changed(rerun);
        }
        definition_preview
            .subject_tokens
            .insert(EXPERT_SUBJECT.into(), expected);
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
    let current = expert_status(&client)
        .await?
        .context("built-in expert disappeared during publication")?;
    if current.kind.as_deref() != Some("agent") {
        println!("The built-in expert was stopped; setup leaves it stopped.");
        return Ok(None);
    }
    anyhow::ensure!(
        current.conflicts.is_empty(),
        "the built-in expert has conflicting declarations"
    );
    let token = current
        .desired_token
        .context("the built-in expert has no declaration")?;
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
    if preview.subject_tokens.get(EXPERT_SUBJECT) != Some(&expected) {
        return declaration_changed(rerun);
    }
    preview
        .subject_tokens
        .insert(EXPERT_SUBJECT.into(), expected);
    let result = apply(&client, preview, config).await;
    if let Err(error) = result {
        // A concurrent first setup may have won the same named run's publication fence.
        if !rerun && crate::client::http_status(&error) == Some(409) {
            return declaration_changed(false);
        }
        return Err(error);
    }
    println!(
        "Started mission/st/onboarding with agent/st/expert. The expert stays available after onboarding."
    );
    Ok(Some(EXPERT_SUBJECT.into()))
}

fn declaration_changed(rerun: bool) -> Result<Option<String>> {
    anyhow::ensure!(
        !rerun,
        "the built-in expert changed during setup; retry setup to inspect its new state"
    );
    println!(
        "The onboarding declarations changed concurrently; ordinary setup preserves their current state. Run st setup again if onboarding has not started."
    );
    Ok(None)
}

async fn expert_status(client: &Client) -> Result<Option<crate::model::SubjectStatus>> {
    let status: StatusResponse = client.get("/v1/status?subject=agent%2Fst%2Fexpert").await?;
    Ok(status
        .subjects
        .into_iter()
        .find(|subject| subject.subject == EXPERT_SUBJECT))
}

fn workspace() -> Result<PathBuf> {
    if let Some(instance) = crate::instance::current() {
        return Ok(instance.agents_dir().join("st-expert"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .context("HOME is not an absolute directory")?;
    Ok(home.join("st/agents/st-expert"))
}

fn declarations(
    harness: &str,
    workspace: &std::path::Path,
    run_id: &str,
    revision: &str,
    person: &str,
    claude_mode: crate::permission_mode::PermissionMode,
) -> Result<String> {
    let template: KdlDocument = EXPERT.parse()?;
    let description = template
        .get("agent")
        .and_then(KdlNode::children)
        .and_then(|body| body.get("description"))
        .and_then(|node| node.get(0))
        .and_then(|value| value.as_string())
        .context("built-in expert description is missing")?;
    let args = st3_client::AgentCreateParameters {
        name: "st/expert".into(),
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
    let mut document: KdlDocument = crate::creation::agent_document(
        &args,
        &workspace.to_string_lossy(),
        false,
        None,
        claude_mode,
    )
    .parse()?;
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
    fn expert_uses_shared_harness_defaults_and_pins_the_guide() {
        assert!(
            super::GUIDE.len() <= 16_384,
            "keep the bundled guide bounded"
        );
        let guide = hex::encode(sha2::Sha256::digest(super::GUIDE.as_bytes()));
        let mission = crate::parse_intent(
            &super::ONBOARDING.replace("@GUIDE@", &format!("doc/st/guide@{guide}")),
            "studio",
        )
        .unwrap();
        assert!(
            mission
                .document_refs
                .contains(&format!("doc/st/guide@{guide}"))
        );
        let revision = &mission.missions[super::MISSION].revision;
        for harness in crate::environment::HARNESSES {
            let source = super::declarations(
                harness,
                std::path::Path::new("/work/st-expert"),
                super::MISSION,
                revision,
                "person/ada",
                crate::permission_mode::PermissionMode::Auto,
            )
            .unwrap();
            let intent = crate::parse_intent(&source, "studio").unwrap();
            assert!(intent.subjects.contains_key(super::EXPERT_SUBJECT));
            assert!(!source.contains("model "));
            assert!(source.contains(&format!("mission/st/onboarding@{revision}")));
            assert!(
                mission.missions[super::MISSION]
                    .constraints
                    .iter()
                    .any(|text| text.contains(&guide))
            );
            match *harness {
                "claude" => {
                    assert!(source.contains("--permission-mode"));
                    assert!(!source.contains("--dangerously-skip-permissions"));
                }
                "codex" => {
                    assert!(source.contains("--approve-for-me"));
                    assert!(!source.contains("--dangerously-bypass"));
                }
                _ => {}
            }
        }
    }
    use sha2::Digest as _;
}
