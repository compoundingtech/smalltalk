//! Gates read the current graph. An expert's evidence identifies objects; it cannot
//! turn an unfinished run, unanswered request or unread message into a checked fact.

use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;
use serde_json::Value;

use crate::client::Client;
use crate::gate_kinds::GateAnswer;
use crate::model::{
    ClaimsPage, DoctorReport, DocumentListResponse, GateSpec, MissionRunView, MissionSpec,
    PersonAnswerView, StatusResponse, StepRunView,
};

pub const STEPS: &[&str] = &[
    "hello",
    "tour",
    "first-mission",
    "your-project",
    "keep-running",
    "phone",
    "second-machine",
    "github",
    "wrap-up",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    version: u32,
    run: String,
    generation: String,
    attempt: u32,
    #[serde(default)]
    sample_run: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    permissions_shown: bool,
    #[serde(default)]
    summary: Option<String>,
}

/// Missing facts are not yet; invalid context, unreadable responses and malformed
/// evidence are broken. Nothing here writes a claim or invokes gh/sudo.
pub async fn check(
    client: &Client,
    step: &str,
    run: &str,
    generation: &str,
    attempt: u32,
) -> GateAnswer {
    match check_inner(client, step, run, generation, attempt).await {
        Ok(true) => GateAnswer::Pass(format!("{step}: current onboarding facts checked")),
        Ok(false) => GateAnswer::NotYet(format!(
            "{step}: required current facts or person answer missing; read doc/st/guide"
        )),
        Err(error) if crate::client::is_not_found(&error) => GateAnswer::NotYet(format!(
            "{step}: referenced graph object does not exist yet"
        )),
        Err(error) => GateAnswer::Broken(format!("{step}: {error:#}")),
    }
}

async fn check_inner(
    client: &Client,
    name: &str,
    id: &str,
    generation: &str,
    attempt: u32,
) -> Result<bool> {
    ensure!(STEPS.contains(&name), "unknown onboarding step");
    let id = id.strip_prefix("mission-run/").unwrap_or(id);
    let run: MissionRunView = client
        .get(&format!("/v1/mission-runs/{}", urlencoding::encode(id)))
        .await?;
    ensure!(
        run.mission.trim_start_matches("mission/") == "st/onboarding",
        "not an onboarding run"
    );
    ensure!(
        run.requester.starts_with("person/"),
        "onboarding requester is not a person"
    );
    if run.generation.trim_start_matches("run-generation/")
        != generation.trim_start_matches("run-generation/")
    {
        return Ok(false);
    }
    let step = run
        .steps
        .iter()
        .find(|step| step.step == name)
        .context("step missing from run")?;
    if step.attempt != attempt
        || step.generation != run.generation
        || matches!(run.status.as_str(), "failed" | "cancelled")
    {
        return Ok(false);
    }
    if matches!(name, "phone" | "second-machine" | "github")
        && answered(&run, step, "choice", "skip")
    {
        return Ok(true);
    }
    match name {
        "hello" => {
            let report: DoctorReport = client.get("/v1/doctor").await?;
            ensure!(
                matches!(report.status.as_str(), "pass" | "warn" | "fail"),
                "unknown doctor status"
            );
            Ok(report.status != "fail")
        }
        "tour" => Ok(answered(&run, step, "choice", "tour-seen")),
        "first-mission" => {
            let Some(approved_at) = acceptance_time(&run, step) else {
                return Ok(false);
            };
            let Some(evidence) = evidence(client, &run, step).await? else {
                return Ok(false);
            };
            let sample_id = evidence
                .sample_run
                .context("first-mission evidence needs sample_run")?;
            ensure!(
                sample_id.trim_start_matches("mission-run/") != id,
                "sample cannot be the onboarding run"
            );
            let sample: MissionRunView = client
                .get(&format!(
                    "/v1/mission-runs/{}",
                    urlencoding::encode(sample_id.trim_start_matches("mission-run/"))
                ))
                .await?;
            if sample.status != "completed"
                || sample.created_at_unix_ms < run.created_at_unix_ms
                || sample.created_at_unix_ms < approved_at
            {
                return Ok(false);
            }
            let scratch = std::path::Path::new(&run.workspace).join("garden-note");
            if std::path::Path::new(&sample.workspace) != scratch {
                return Ok(false);
            }
            let spec: MissionSpec = client
                .get(&format!(
                    "/v1/missions/{}",
                    urlencoding::encode(sample.mission.trim_start_matches("mission/"))
                ))
                .await?;
            // Latest declarations cannot stand in for the revision that actually ran.
            if spec.revision != sample.revision {
                return Ok(false);
            }
            Ok(spec.steps.values().any(|definition| {
                definition.gates.iter().any(|gate| matches!(gate, GateSpec::Human { reviewer, .. } if reviewer == &run.requester))
                    && sample.steps.iter().any(|actual| actual.step == definition.id && actual.status == "completed")
            }))
        }
        "your-project" => {
            let Some(approved_at) = acceptance_time(&run, step) else {
                return Ok(false);
            };
            let Some(evidence) = evidence(client, &run, step).await? else {
                return Ok(false);
            };
            let agent = evidence
                .agent
                .context("your-project evidence needs agent")?;
            ensure!(
                agent.starts_with("agent/") && agent != crate::onboarding::EXPERT_SUBJECT,
                "name the new project seat"
            );
            let message = evidence
                .message
                .context("your-project evidence needs message")?;
            ensure!(message.starts_with("message/"), "name the first message");
            let workspace = evidence
                .workspace
                .context("your-project evidence needs workspace")?;
            let status: StatusResponse = client
                .get(&format!(
                    "/v1/status?subject={}",
                    urlencoding::encode(&agent)
                ))
                .await?;
            let Some(seat) = status.subjects.iter().find(|seat| seat.subject == agent) else {
                return Ok(false);
            };
            let Some(harness) = &seat.harness else {
                return Ok(false);
            };
            if seat.kind.as_deref() != Some("agent")
                || !harness.is_ready()
                || harness.incarnation_id.is_empty()
                || seat.projection.layer != "current"
                || !seat.projection.actionable
                || !seat.conflicts.is_empty()
            {
                return Ok(false);
            }
            let token = seat
                .desired_token
                .as_ref()
                .context("seat has no declaration token")?;
            let claim: crate::model::ClaimRecord = client
                .get(&format!("/v1/claims/by-id/{}", urlencoding::encode(token)))
                .await?;
            let desired: crate::model::DesiredSubject = serde_json::from_value(claim.body)?;
            if desired
                .member
                .as_ref()
                .map(|member| member.workspace.as_str())
                != Some(&workspace)
            {
                return Ok(false);
            }
            let delivery: Value = client
                .get(&format!(
                    "/v1/messages/delivery/{}",
                    urlencoding::encode(&message)
                ))
                .await?;
            if claim.accepted_at_unix_ms < approved_at {
                return Ok(false);
            }
            if delivery["to"].as_str() != Some(&agent) {
                return Ok(false);
            }
            // Page the one message's claims to exhaustion; no global scans or silent truncation.
            let mut after = 0;
            let mut sent = false;
            let mut read = false;
            loop {
                let page: ClaimsPage = client
                    .get(&format!(
                        "/v1/claims?subject={}&after={after}&limit=100",
                        urlencoding::encode(&message)
                    ))
                    .await?;
                sent |= page.claims.iter().any(|claim| {
                    claim.kind == "message.sent"
                        && claim.accepted_at_unix_ms >= run.created_at_unix_ms
                        && claim.accepted_at_unix_ms >= approved_at
                });
                read |= page.claims.iter().any(|claim| {
                    claim.kind == "message.read"
                        && claim.actor.as_deref() == Some(&agent)
                        && claim.accepted_at_unix_ms >= run.created_at_unix_ms
                });
                if sent && read {
                    return Ok(true);
                }
                let Some(next) = page.next_cursor else {
                    return Ok(false);
                };
                ensure!(next > after, "message claim cursor did not advance");
                after = next;
            }
        }
        "keep-running" => {
            let service = tokio::task::spawn_blocking(crate::service::status).await??;
            if !service.services.iter().any(|service| {
                matches!(
                    service.name.as_str(),
                    "st3.service" | "com.compoundingtech.st3"
                ) && service.installed
                    && service.running
            }) {
                return Ok(false);
            }
            #[cfg(target_os = "linux")]
            {
                let linger = tokio::task::spawn_blocking(|| {
                    std::process::Command::new("loginctl")
                        .args(["show-user", "--property=Linger", "--value"])
                        .output()
                })
                .await??;
                ensure!(
                    linger.status.success(),
                    "loginctl could not read current user's lingering"
                );
                Ok(String::from_utf8(linger.stdout)?.trim() == "yes")
            }
            #[cfg(target_os = "macos")]
            {
                let Some(evidence) = evidence(client, &run, step).await? else {
                    return Ok(false);
                };
                Ok(
                    evidence.permissions_shown
                        && answered(&run, step, "choice", "permissions-seen"),
                )
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                Ok(false)
            }
        }
        "phone" => {
            if !answered(&run, step, "choice", "enable") {
                return Ok(false);
            }
            // This read needs an explicit person filter over the trusted local socket.
            let reader = Client::unix_as(
                client
                    .socket_path()
                    .context("phone check needs the local socket")?,
                &run.requester,
            )?;
            let mut path = "/v1/client/devices?limit=100".to_owned();
            loop {
                let page: Value = reader.get(&path).await?;
                let items = page["items"]
                    .as_array()
                    .context("device page has no items")?;
                if items.iter().any(|device| {
                    device["person_id"] == run.requester && device["state"] == "active"
                }) {
                    return Ok(true);
                }
                if page["page"]["has_more"] == false {
                    return Ok(false);
                }
                let cursor = page["page"]["next_cursor"]
                    .as_str()
                    .context("device page has no continuation")?;
                let next = format!(
                    "/v1/client/devices?limit=100&cursor={}",
                    urlencoding::encode(cursor)
                );
                ensure!(next != path, "device cursor did not advance");
                path = next;
            }
        }
        "second-machine" => {
            if !answered(&run, step, "choice", "enable") {
                return Ok(false);
            }
            let fleet: crate::api::FleetStatus = client.get("/v1/internal/fleet/status").await?;
            Ok(fleet.removed.is_none()
                && fleet.fleet_id.is_some()
                && fleet
                    .view
                    .members
                    .iter()
                    .any(|member| member.name != fleet.node && member.state == "current"))
        }
        "github" => {
            if !answered(&run, step, "choice", "enable") {
                return Ok(false);
            }
            let report: DoctorReport = client.get("/v1/doctor").await?;
            Ok(report
                .checks
                .iter()
                .any(|check| check.name == "github-observer-auth" && check.status == "pass"))
        }
        "wrap-up" => {
            if !STEPS[..8].iter().all(|name| {
                run.steps
                    .iter()
                    .any(|step| step.step == *name && step.status == "completed")
            }) {
                return Ok(false);
            }
            let Some(evidence) = evidence(client, &run, step).await? else {
                return Ok(false);
            };
            if evidence
                .summary
                .as_deref()
                .is_none_or(|summary| summary.trim().is_empty())
            {
                return Ok(false);
            }
            let status: StatusResponse =
                client.get("/v1/status?subject=agent%2Fst%2Fexpert").await?;
            Ok(status.subjects.iter().any(|seat| {
                seat.subject == crate::onboarding::EXPERT_SUBJECT
                    && seat.kind.as_deref() == Some("agent")
                    && seat.owner_run.is_none()
            }))
        }
        _ => unreachable!(),
    }
}

fn answered(run: &MissionRunView, step: &StepRunView, kind: &str, id: &str) -> bool {
    step.person_answers
        .iter()
        .any(|answer| person_answer(answer, &run.requester, kind, id))
}

fn acceptance_time(run: &MissionRunView, step: &StepRunView) -> Option<u128> {
    step.person_answers
        .iter()
        .filter(|answer| {
            answer.status == "completed"
                && answer.acted_for.as_deref().unwrap_or(&answer.respondent) == run.requester
                && answer
                    .answer
                    .as_ref()
                    .is_some_and(|value| value["type"] == "decision")
        })
        .max_by_key(|answer| answer.answered_at_unix_ms)
        .filter(|answer| person_answer(answer, &run.requester, "decision", "accept"))
        .map(|answer| answer.answered_at_unix_ms)
}

fn person_answer(answer: &PersonAnswerView, person: &str, kind: &str, id: &str) -> bool {
    let respondent = answer.acted_for.as_deref().unwrap_or(&answer.respondent);
    answer.status == "completed"
        && respondent == person
        && answer.answer.as_ref().is_some_and(|value| {
            value["type"] == kind
                && value["id"] == id
                && (kind != "decision" || value["outcome"] == "accept")
        })
}

async fn evidence(
    client: &Client,
    run: &MissionRunView,
    step: &StepRunView,
) -> Result<Option<Evidence>> {
    let name = format!(
        "doc/st/onboarding/{}/{}",
        run.id.trim_start_matches("mission-run/"),
        step.step
    );
    let versions: DocumentListResponse = client
        .get(&format!(
            "/v1/documents?name={}&limit=1",
            urlencoding::encode(&name)
        ))
        .await?;
    let Some(version) = versions.items.first() else {
        return Ok(None);
    };
    ensure!(
        version.size <= 8192,
        "onboarding evidence exceeds 8192 bytes"
    );
    #[derive(Deserialize)]
    struct Content {
        bytes: Vec<u8>,
    }
    let content: Content = client
        .get(&format!(
            "/v1/documents/content?reference={}",
            urlencoding::encode(&format!("{name}@{}", version.hash))
        ))
        .await?;
    let evidence: Evidence =
        serde_json::from_slice(&content.bytes).context("parse onboarding evidence JSON")?;
    ensure!(evidence.version == 1, "unknown onboarding evidence version");
    if evidence.run.trim_start_matches("mission-run/") != run.id.trim_start_matches("mission-run/")
        || evidence.generation.trim_start_matches("run-generation/")
            != run.generation.trim_start_matches("run-generation/")
        || evidence.attempt != step.attempt
    {
        return Ok(None);
    }
    // Read this on every platform so typoed fields still produce a useful diagnosis.
    let _ = evidence.permissions_shown;
    Ok(Some(evidence))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    struct Fixture {
        client: Client,
        replies: Arc<Mutex<BTreeMap<String, Value>>>,
        run: MissionRunView,
        server: tokio::task::JoinHandle<()>,
    }

    impl Fixture {
        async fn new() -> Self {
            let store = crate::store::Store::open_memory("studio").unwrap();
            let source = format!(
                "version 2\nmission \"st/onboarding\" state=\"ready\" {{ goal \"Learn\"; {} }}",
                STEPS
                    .iter()
                    .map(|step| format!("step \"{step}\" {{ agentless }}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            let intent = crate::parse_intent(&source, "studio").unwrap();
            let preview = store
                .mission(
                    &intent,
                    crate::model::IntentInput {
                        kdl: source,
                        source_name: None,
                    },
                )
                .unwrap();
            store
                .apply(&intent, &preview.subject_tokens, "publish")
                .unwrap();
            let run = store
                .create_mission_run(&crate::model::MissionRunRequest {
                    mission: "st/onboarding".into(),
                    revision: None,
                    workspace: "/scratch/expert".into(),
                    requester: Some("person/ada".into()),
                    mode: None,
                    inputs: Default::default(),
                    idempotency_key: "fixture-run".into(),
                })
                .unwrap();
            let replies = Arc::new(Mutex::new(BTreeMap::from([
                (
                    format!("/v1/mission-runs/{}", urlencoding::encode(&run.id)),
                    serde_json::to_value(&run).unwrap(),
                ),
                (
                    "/v1/doctor".into(),
                    json!({"status":"pass","checks":[],"performance":{}}),
                ),
            ])));
            let data = replies.clone();
            let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
                let data = data.clone();
                async move {
                    let key = uri.path_and_query().unwrap().as_str();
                    let value = data.lock().unwrap().get(key).cloned();
                    match value {
                        Some(value) => (axum::http::StatusCode::OK, axum::Json(json!({"api_version":"st3.v1","value":value}))),
                        None => (axum::http::StatusCode::NOT_FOUND, axum::Json(json!({"api_version":"st3.v1","error":{"code":"not-found","message":"fixture object missing","details":{}}}))),
                    }
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = Client::new(crate::client::Endpoint::Http(format!(
                "http://{}",
                listener.local_addr().unwrap()
            )));
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                client,
                replies,
                run,
                server,
            }
        }

        fn update(&self, run: &MissionRunView) {
            self.replies.lock().unwrap().insert(
                format!("/v1/mission-runs/{}", urlencoding::encode(&run.id)),
                serde_json::to_value(run).unwrap(),
            );
        }

        async fn code(&self, step: &str) -> u8 {
            check(
                &self.client,
                step,
                &self.run.id,
                &self.run.generation,
                self.run.steps[0].attempt,
            )
            .await
            .exit_code()
        }

        fn document(&self, step: &str, value: Value) {
            let name = format!("doc/st/onboarding/{}/{step}", self.run.id);
            let bytes = serde_json::to_vec(&value).unwrap();
            let hash = "1".repeat(64);
            let mut data = self.replies.lock().unwrap();
            data.insert(format!("/v1/documents?name={}&limit=1", urlencoding::encode(&name)), json!({"items":[{"name":name,"hash":hash,"size":bytes.len(),"created_index":1,"latest":true,"binding_claim_id":"claim/fixture","created_at_unix_ms":1}],"has_more":false,"next_cursor":null,"limit":1,"history":false}));
            data.insert(
                format!(
                    "/v1/documents/content?reference={}",
                    urlencoding::encode(&format!("{name}@{hash}"))
                ),
                json!({"bytes":bytes}),
            );
        }

        fn evidence(&self) -> Value {
            json!({"version":1,"run":self.run.id,"generation":self.run.generation,"attempt":self.run.steps[0].attempt})
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    fn answer(kind: &str, id: &str) -> PersonAnswerView {
        PersonAnswerView {
            acted_for: None,
            ask: "step-run/fixture/ask".into(),
            status: "completed".into(),
            summary: "Fixture person answered".into(),
            respondent: "person/ada".into(),
            answered_at_unix_ms: 1,
            answer: Some(
                json!({"type":kind,"id":id,"outcome":if kind == "decision" { "accept" } else { "selected" }}),
            ),
            evidence: Vec::new(),
        }
    }

    #[tokio::test]
    async fn skips_require_the_current_person_step_generation_and_attempt() {
        let fixture = Fixture::new().await;
        assert_eq!(fixture.code("phone").await, 1);
        let mut run = fixture.run.clone();
        let phone = run
            .steps
            .iter_mut()
            .find(|step| step.step == "phone")
            .unwrap();
        phone.person_answers.push(answer("choice", "skip"));
        fixture.update(&run);
        assert_eq!(fixture.code("phone").await, 0);
        assert_eq!(
            fixture.code("github").await,
            1,
            "phone skip cannot skip GitHub"
        );
        assert_eq!(
            check(&fixture.client, "phone", &run.id, "old-generation", 1)
                .await
                .exit_code(),
            1
        );
        assert_eq!(
            check(&fixture.client, "phone", &run.id, &run.generation, 99)
                .await
                .exit_code(),
            1
        );
        let phone = run
            .steps
            .iter_mut()
            .find(|step| step.step == "phone")
            .unwrap();
        phone.person_answers[0].respondent = "agent/st/expert".into();
        fixture.update(&run);
        assert_eq!(
            fixture.code("phone").await,
            1,
            "expert prose is not a person skip"
        );
        let phone = run
            .steps
            .iter_mut()
            .find(|step| step.step == "phone")
            .unwrap();
        phone.person_answers[0].respondent = "person/ada".into();
        phone.person_answers[0].status = "cancelled".into();
        fixture.update(&run);
        assert_eq!(fixture.code("phone").await, 1);
        let tour = run
            .steps
            .iter_mut()
            .find(|step| step.step == "tour")
            .unwrap();
        tour.person_answers.push(answer("choice", "tour-seen"));
        fixture.update(&run);
        assert_eq!(fixture.code("tour").await, 0);
    }

    #[tokio::test]
    async fn sample_completion_requires_approval_and_the_revision_person_review() {
        let fixture = Fixture::new().await;
        let mut run = fixture.run.clone();
        run.steps
            .iter_mut()
            .find(|step| step.step == "first-mission")
            .unwrap()
            .person_answers
            .push(answer("decision", "accept"));
        fixture.update(&run);
        let mut evidence = fixture.evidence();
        evidence["sample_run"] = json!("garden/one");
        fixture.document("first-mission", evidence.clone());
        let source = "version 2\nmission \"garden\" state=\"ready\" { goal \"Review a note\"; step \"review-note\" { agentless; gate \"approved\" type=\"human\" { reviewer \"person/ada\" } } }";
        let mut spec = crate::parse_intent(source, "studio")
            .unwrap()
            .missions
            .remove("garden")
            .unwrap();
        let mut sample = fixture.run.clone();
        sample.id = "garden/one".into();
        sample.mission = "garden".into();
        sample.status = "completed".into();
        sample.workspace = "/scratch/expert/garden-note".into();
        sample.revision = spec.revision.clone();
        sample.steps[0].step = "review-note".into();
        sample.steps[0].status = "completed".into();
        {
            let mut data = fixture.replies.lock().unwrap();
            data.insert(
                "/v1/mission-runs/garden%2Fone".into(),
                serde_json::to_value(&sample).unwrap(),
            );
            data.insert(
                "/v1/missions/garden".into(),
                serde_json::to_value(&spec).unwrap(),
            );
        }
        assert_eq!(fixture.code("first-mission").await, 0);
        let original = run.clone();
        run.steps
            .iter_mut()
            .find(|step| step.step == "first-mission")
            .unwrap()
            .person_answers[0]
            .answered_at_unix_ms = sample.created_at_unix_ms + 1;
        fixture.update(&run);
        assert_eq!(
            fixture.code("first-mission").await,
            1,
            "approval after the sample started is insufficient"
        );
        run = original.clone();
        let mut decline = answer("decision", "decline");
        decline.answered_at_unix_ms = 2;
        decline.answer.as_mut().unwrap()["outcome"] = json!("decline");
        run.steps
            .iter_mut()
            .find(|step| step.step == "first-mission")
            .unwrap()
            .person_answers
            .push(decline);
        fixture.update(&run);
        assert_eq!(
            fixture.code("first-mission").await,
            1,
            "a later decline overrides an older approval"
        );
        run = original;
        fixture.update(&run);
        spec.steps.get_mut("review-note").unwrap().gates.clear();
        fixture.replies.lock().unwrap().insert(
            "/v1/missions/garden".into(),
            serde_json::to_value(&spec).unwrap(),
        );
        assert_eq!(
            fixture.code("first-mission").await,
            1,
            "completed sample without a person review is insufficient"
        );
        evidence["generation"] = json!("stale");
        fixture.document("first-mission", evidence.clone());
        assert_eq!(fixture.code("first-mission").await, 1);
        evidence["generation"] = json!(run.generation);
        evidence["typo"] = json!(true);
        fixture.document("first-mission", evidence);
        assert_eq!(
            fixture.code("first-mission").await,
            3,
            "malformed evidence is broken, not success"
        );
    }

    #[tokio::test]
    async fn doctor_failure_and_early_wrap_up_cannot_pass() {
        let fixture = Fixture::new().await;
        assert_eq!(fixture.code("hello").await, 0);
        fixture.replies.lock().unwrap().insert(
            "/v1/doctor".into(),
            json!({"status":"fail","checks":[],"performance":{}}),
        );
        assert_eq!(fixture.code("hello").await, 1);
        assert_eq!(fixture.code("wrap-up").await, 1);
    }

    #[tokio::test]
    async fn project_needs_a_current_seat_and_recipient_read_across_claim_pages() {
        let fixture = Fixture::new().await;
        let mut run = fixture.run.clone();
        run.steps
            .iter_mut()
            .find(|step| step.step == "your-project")
            .unwrap()
            .person_answers
            .push(answer("decision", "accept"));
        fixture.update(&run);
        let mut evidence = fixture.evidence();
        evidence["agent"] = json!("agent/garden/worker");
        evidence["message"] = json!("message/project-first");
        evidence["workspace"] = json!("/scratch/project");
        fixture.document("your-project", evidence);
        let source = "version 2\nagent \"garden/worker\" { workspace \"/scratch/project\"; harness \"codex\" {} }";
        let desired = crate::parse_intent(source, "studio")
            .unwrap()
            .subjects
            .remove("agent/garden/worker")
            .unwrap();
        let claim = |kind: &str, actor: &str, body: Value, at: u128| {
            json!({
                "id":"claim/fixture","store_index":101,"batch_id":"batch/fixture","subject":"message/project-first",
                "kind":kind,"origin":"studio","actor":actor,"body":body,"predecessors":[],"accepted_at_unix_ms":at
            })
        };
        let mut seat = json!({"subject":"agent/garden/worker","kind":"agent","desired_token":"claim/seat","desired_revision":null,"desired":null,"actual":null,
            "harness":{"state":"ready","incarnation_id":"fixture-current","claim":"claim/harness","observed_at_unix_ms":1,"since_unix_ms":1},
            "conflicts":[],"claims":[],"owner_run":null,"gap":null,"reachability":"reachable","reason":null,
            "projection":{"layer":"current","actionable":true,"reasons":[]}});
        let sent = claim(
            "message.sent",
            "agent/st/expert",
            json!({}),
            run.created_at_unix_ms + 1,
        );
        let read = claim(
            "message.read",
            "agent/garden/worker",
            json!({}),
            run.created_at_unix_ms + 2,
        );
        {
            let mut replies = fixture.replies.lock().unwrap();
            replies.insert(
                "/v1/status?subject=agent%2Fgarden%2Fworker".into(),
                json!({"store_index":1,"subjects":[seat.clone()],"pending_actions":[]}),
            );
            replies.insert(
                "/v1/claims/by-id/claim%2Fseat".into(),
                claim(
                    "declaration",
                    "person/ada",
                    serde_json::to_value(desired).unwrap(),
                    1,
                ),
            );
            replies.insert(
                "/v1/messages/delivery/message%2Fproject-first".into(),
                json!({"to":"agent/garden/worker","delivery":{"state":"read"}}),
            );
            replies.insert(
                "/v1/claims?subject=message%2Fproject-first&after=0&limit=100".into(),
                json!({"claims":[sent],"next_cursor":100}),
            );
            replies.insert(
                "/v1/claims?subject=message%2Fproject-first&after=100&limit=100".into(),
                json!({"claims":[read.clone()],"next_cursor":null}),
            );
        }
        assert_eq!(
            fixture.code("your-project").await,
            0,
            "a later receipt page must be checked"
        );
        let mut wrong_reader = read.clone();
        wrong_reader["actor"] = json!("agent/st/expert");
        fixture.replies.lock().unwrap().insert(
            "/v1/claims?subject=message%2Fproject-first&after=100&limit=100".into(),
            json!({"claims":[wrong_reader],"next_cursor":null}),
        );
        assert_eq!(
            fixture.code("your-project").await,
            1,
            "sender read does not prove recipient delivery"
        );
        fixture.replies.lock().unwrap().insert(
            "/v1/claims?subject=message%2Fproject-first&after=100&limit=100".into(),
            json!({"claims":[read],"next_cursor":null}),
        );
        seat["kind"] = json!("stop");
        seat["projection"]["layer"] = json!("history");
        seat["projection"]["actionable"] = json!(false);
        fixture.replies.lock().unwrap().insert(
            "/v1/status?subject=agent%2Fgarden%2Fworker".into(),
            json!({"store_index":1,"subjects":[seat],"pending_actions":[]}),
        );
        assert_eq!(
            fixture.code("your-project").await,
            1,
            "historical reachability must not revive a stopped seat"
        );
    }
}
