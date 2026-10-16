//! Documents for clients: one version's text, the list of documents, and the documents a
//! message, mission or step names. Every client reads documents through these routes; see
//! `docs/clients/ui-contract.md` (Documents).

use super::client_blobs::person_reads_message;
use super::client_v0::{ClientSession, require_scope};
use super::*;
use st3_ui_model::documents::{document_links, parse_reference};

#[derive(Deserialize)]
pub(super) struct ClientDocumentQuery {
    name: String,
}

fn invalid_reference() -> ApiError {
    ApiError::bad(St3Error::new(
        "invalid-document-reference",
        "a document is named `doc/NAME@HASH`, or `doc/NAME` for its latest version",
    ))
}

/// `GET /v1/client/documents/content`: one version of a document, `doc/NAME@HASH`, or the
/// name's latest version for `doc/NAME`. A pinned version this host does not hold answers
/// `document-not-synced`; an unknown name answers `not-found`.
pub(super) async fn document_get(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<ClientDocumentQuery>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let (name, hash) = match query.name.rsplit_once('@') {
        Some((name, hash)) if !name.is_empty() && !hash.is_empty() => {
            (name.to_owned(), Some(hash.to_owned()))
        }
        Some(_) => return Err(invalid_reference()),
        None if query.name.starts_with("doc/") && query.name.len() > 4 => (query.name.clone(), None),
        None => return Err(invalid_reference()),
    };
    let store = state.store.clone();
    let lookup = (name.clone(), hash.clone());
    let found = blocking_store(move || {
        let Some(version) = store.document_version(&lookup.0, lookup.1.as_deref())? else {
            return Ok(None);
        };
        let bytes = store.get_document(&version.name, &version.hash)?;
        Ok(bytes.map(|bytes| (version, bytes)))
    })
    .await?;
    let Some((version, bytes)) = found else {
        return Err(match hash {
            Some(_) => ApiError {
                status: StatusCode::NOT_FOUND,
                code: "document-not-synced".into(),
                message: format!("document `{}` has not reached this host yet", query.name),
                details: Box::default(),
            },
            None => ApiError::not_found(format!("document `{}` is not stored", query.name)),
        });
    };
    let mut value = document_item(&version);
    value["bytes"] = json!(bytes);
    Ok(Json(value))
}

/// A version without its bytes; `owner` only when st knows it.
fn document_item(version: &smallclaims::claim::DocumentVersion) -> Value {
    let mut value = json!({
        "reference": format!("{}@{}", version.name, version.hash),
        "name": version.name,
        "version": version.hash,
        "size": version.size,
        "changed_at_unix_ms": version.created_at_unix_ms as u64,
        "latest": version.latest,
    });
    if let Some(owner) = &version.owner {
        value["owner"] = json!(owner);
    }
    value
}

/// `GET /v1/client/documents`: the documents in name order, a page at a time, with the same
/// filters and cursor as the daemon's own `/v1/documents`.
pub(super) async fn documents_list(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    query: Query<DocumentQuery>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let Json(page) = list_documents(State(state), query).await?;
    Ok(Json(json!({
        "items": page.items.iter().map(document_item).collect::<Vec<_>>(),
        "has_more": page.has_more,
        "limit": page.limit,
        "history": page.history,
        "next_cursor": page.next_cursor,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DocumentReferencesQuery {
    subject: String,
}

fn reference_value(reference: &str, source: &str, step: Option<&str>) -> Option<Value> {
    let (name, version) = parse_reference(reference)?;
    Some(json!({
        "reference": reference,
        "name": name,
        "version": version,
        "source": source,
        "step": step,
    }))
}

/// The pinned documents of `mission`'s steps, in display order, each once, with the step that
/// pins it; `only` keeps one step's.
fn step_references(mission: &crate::model::MissionSpec, only: Option<&str>, references: &mut Vec<Value>) {
    for id in &mission.display_order {
        let Some(step) = mission.steps.get(id) else {
            continue;
        };
        if only.is_none_or(|only| only == step.path) {
            for document in &step.documents {
                if references.iter().all(|seen| seen["reference"] != document.as_str())
                    && let Some(value) = reference_value(document, "step", Some(&step.path))
                {
                    references.push(value);
                }
            }
        }
        if let Some(nested) = &step.nested_mission {
            step_references(nested, only, references);
        }
    }
}

/// `GET /v1/client/document-references?subject=`: the documents a message, mission, mission
/// run or step names. A message names the document that is its content, then each `doc/…`
/// link in its whole text; a step names its pinned documents; a mission or run names its
/// steps'.
pub(super) async fn document_references(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Query(query): Query<DocumentReferencesQuery>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.projections")?;
    let subject = query.subject;
    let mut references = Vec::new();
    let mut complete = true;
    if subject.starts_with("message/") {
        let lookup = state.clone();
        let wanted = subject.clone();
        let message = blocking_store(move || lookup.store.message(&wanted))
            .await?
            .ok_or_else(|| ApiError::not_found(format!("message `{subject}` does not exist")))?;
        let actor = session.authority_actor.as_str();
        if actor.starts_with("person/")
            && actor.matches('/').count() == 1
            && !person_reads_message(&state.store, actor, &message)
        {
            return Err(ApiError {
                status: StatusCode::FORBIDDEN,
                code: "forbidden".into(),
                message: "this message is not in a conversation the session may read".into(),
                details: Box::default(),
            });
        }
        let text = match super::message_bodies::whole_text(&state, &session, &message).await {
            Ok(text) => text,
            Err(_) => {
                complete = false;
                message.content.clone()
            }
        };
        if let Some(value) = reference_value(message.content.trim(), "content", None) {
            references.push(value);
        }
        for link in document_links(&text) {
            if references.iter().all(|seen| seen["reference"] != link.reference.as_str())
                && let Some(value) = reference_value(&link.reference, "body", None)
            {
                references.push(value);
            }
        }
    } else if subject.starts_with("mission/")
        || subject.starts_with("mission-run/")
        || subject.starts_with("step-run/")
    {
        let store = state.store.clone();
        let wanted = subject.clone();
        let found = blocking_store(move || {
            let (mission, revision, step) = if let Some(mission) = wanted.strip_prefix("mission/") {
                (mission.to_owned(), None, None)
            } else {
                let (run, step) = if wanted.starts_with("step-run/") {
                    let Some(step) = store.step_run(&wanted)? else {
                        return Ok(None);
                    };
                    (step.run, Some(step.step))
                } else {
                    (wanted.clone(), None)
                };
                let Some(run) = store.mission_run(&run)? else {
                    return Ok(None);
                };
                let mission = run.mission.trim_start_matches("mission/").to_owned();
                (mission, Some(run.revision), step)
            };
            let spec = store.mission_spec(&mission, revision.as_deref())?;
            Ok(spec.map(|spec| (spec, step)))
        })
        .await?;
        let (mission, step) =
            found.ok_or_else(|| ApiError::not_found(format!("`{subject}` does not exist")))?;
        step_references(&mission, step.as_deref(), &mut references);
    } else {
        return Err(ApiError::bad(St3Error::new(
            "validation-failed",
            "document references are read for a message/, mission/, mission-run/ or step-run/ subject",
        )));
    }
    Ok(Json(json!({ "subject": subject, "references": references, "complete": complete })))
}
