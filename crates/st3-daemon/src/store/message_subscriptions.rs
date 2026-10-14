//! Validation for public direct-message registrations on existing observer declarations.
use super::*;

pub(crate) fn validate_actor(
    intent: &NormalizedIntent,
    actor: Option<&str>,
) -> Result<(), St3Error> {
    if intent.direct_message_registrations.is_empty() {
        return Ok(());
    }
    let actor = actor.ok_or_else(|| {
        St3Error::new(
            "missing-publication-actor",
            "message subscription registration needs an actor",
        )
    })?;
    if !(actor.starts_with("person/") || actor.starts_with("agent/"))
        || st3_schema::registry().validate_subject(actor).is_err()
    {
        return Err(St3Error::new(
            "invalid-registration-actor",
            "message subscription registration needs a person or agent actor",
        ));
    }
    for subject in &intent.direct_message_registrations {
        let spec = crate::graph::subscription_spec(&intent.subjects[subject].desired).unwrap();
        if spec.to != actor {
            return Err(St3Error::new(
                "foreign-registration-recipient",
                "a public message subscription must be published by its recipient",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate(
    connection: &Connection,
    intent: &NormalizedIntent,
    index: u64,
) -> Result<(), St3Error> {
    for subject in &intent.direct_message_registrations {
        let desired = &intent.subjects[subject];
        let subscription = crate::graph::subscription_spec(&desired.desired).ok_or_else(|| {
            St3Error::new(
                "invalid-message-registration",
                "message registration needs a subscription",
            )
        })?;
        let existing = connection
            .query_row(
                "SELECT kind, body FROM desired WHERE subject=?1",
                [subject],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(internal)?;
        if let Some((kind, body)) = existing {
            let body: Value = serde_json::from_str(&body).map_err(internal)?;
            if kind != desired.kind
                || crate::graph::subscription_spec(&body).as_ref() != Some(&subscription)
            {
                return Err(St3Error::new(
                    "message-registration-conflict",
                    "an existing subscription cannot be replaced or retargeted by public registration",
                ));
            }
        }
        let declared = |subject: &str| -> Result<Option<(String, Value)>, St3Error> {
            connection.query_row(
                "SELECT desired.kind, desired.body FROM desired JOIN claims ON claims.id=desired.claim_id
                 WHERE desired.subject=?1 AND claims.store_index<=?2",
                params![subject, index], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
                .optional().map_err(internal)?
                .map(|(kind, body)| serde_json::from_str(&body).map(|body| (kind, body)).map_err(internal)).transpose()
        };
        let observer = declared(&subscription.observer)?
            .filter(|(kind, _)| kind == "observer")
            .and_then(|(_, body)| crate::graph::observer_spec(&body));
        if observer.as_ref().is_none_or(|observer| observer.stopped) {
            return Err(St3Error::new(
                "missing-registration-observer",
                format!(
                    "subscription `{subject}` needs an already-declared live observer `{}`",
                    subscription.observer
                ),
            ));
        }
        if observer
            .as_ref()
            .is_some_and(|observer| observer.provider != "github.repository")
        {
            return Err(St3Error::new(
                "invalid-registration-observer",
                "main Performance registration needs a github.repository observer",
            ));
        }
        if declared(&subscription.to)?.is_none_or(|(kind, _)| kind != "agent")
            || declaration_stopped_tx(connection, &subscription.to)
                .map_err(internal)?
                .is_some()
        {
            return Err(St3Error::new(
                "missing-registration-recipient",
                format!(
                    "subscription `{subject}` needs an already-declared agent `{}`",
                    subscription.to
                ),
            ));
        }
    }
    Ok(())
}

/// Publication and replicated projection share the original-author check. The public API
/// reparses KDL; a replicated main-field registration is classified from its durable spec.
pub(crate) fn validate_publisher(
    connection: &Connection,
    intent: &NormalizedIntent,
    actor: Option<&str>,
) -> Result<(), St3Error> {
    validate_actor(intent, actor)?;
    for subject in &intent.direct_message_registrations {
        let existing_author: Option<Option<String>> = connection.query_row(
            "SELECT claims.actor FROM desired JOIN claims ON claims.id=desired.claim_id WHERE desired.subject=?1",
            [subject], |row| row.get(0)).optional().map_err(internal)?;
        if existing_author.is_some_and(|author| author.as_deref() != actor) {
            return Err(St3Error::new(
                "message-registration-author",
                "only the original author can re-register a public subscription",
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_replicated(
    connection: &Transaction<'_>,
    claim: &ClaimRecord,
    desired: &DesiredSubject,
) -> Result<(), St3Error> {
    if desired.owner_run.is_some() || desired.kind != "subscription" {
        return Ok(());
    }
    let Some(spec) = crate::graph::subscription_spec(&desired.desired) else {
        return Ok(());
    };
    if !spec
        .fields
        .iter()
        .any(|field| field == crate::resource::github_workflows::PERFORMANCE_FAILURES_FIELD)
        || spec.watch.is_some()
    {
        return Ok(());
    }
    if spec.delivery != "message"
        || spec.fields != [crate::resource::github_workflows::PERFORMANCE_FAILURES_FIELD]
        || spec.batch_every_ms.is_some()
        || spec.condition.is_some()
        || spec.owner_message
        || spec.stopped
    {
        return Err(St3Error::new(
            "invalid-message-registration",
            "unmanaged main Performance registrations must be simple direct messages",
        ));
    }
    let intent = NormalizedIntent {
        direct_message_registrations: BTreeSet::from([desired.subject.clone()]),
        schema: "st3.v1".into(),
        source_hash: String::new(),
        subjects: BTreeMap::from([(desired.subject.clone(), desired.clone())]),
        missions: BTreeMap::new(),
        mission_provenance: BTreeMap::new(),
        mission_runs: BTreeMap::new(),
        planning_sessions: BTreeMap::new(),
        resource_refreshes: Vec::new(),
        replica_repairs: Vec::new(),
        document_refs: BTreeSet::new(),
        deprecated_syntax: BTreeSet::new(),
        normalized: Value::Null,
    };
    validate_publisher(connection, &intent, claim.actor.as_deref())?;
    validate(
        connection,
        &intent,
        current_index_tx(connection).map_err(internal)?,
    )
}

#[cfg(test)]
pub(crate) const PERFORMANCE_REGISTRATION: &str = r#"version 2
subscription "fixture/main-performance-p0" {
  observer "observer/github/compoundingtech/smalltalk"
  on "main_performance_failures"
  to "agent/fleet/fixture-project/speed"
  delivery "message"
}
"#;

#[cfg(test)]
pub(crate) const PERFORMANCE_DECLARATIONS: &str = r#"version 2
resource "github/compoundingtech/smalltalk" { kind "vcs.repository" }
observer "github/compoundingtech/smalltalk" {
  resource "resource/github/compoundingtech/smalltalk"
  provider "github.repository"
  locator "compoundingtech/smalltalk"
  field "issues"
}
agent "fleet/fixture-project/speed" { workspace "."; command "true" }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn setup(store: &Store) {
        let intent = crate::graph::parse_internal_intent(PERFORMANCE_DECLARATIONS, "node").unwrap();
        store
            .apply_internal(&intent, "fixture-declarations")
            .unwrap();
    }

    #[test]
    fn main_performance_failures_registration_previews_applies_and_is_idempotent() {
        let store = Store::open_memory("node").unwrap();
        setup(&store);
        let intent = crate::graph::parse_intent(PERFORMANCE_REGISTRATION, "node").unwrap();
        assert_eq!(
            intent
                .direct_message_registrations
                .iter()
                .collect::<Vec<_>>(),
            ["subscription/fixture/main-performance-p0"]
        );
        let before = store.desired_subjects().unwrap();
        let observer = store
            .selected_desired_revision("observer/github/compoundingtech/smalltalk")
            .unwrap();
        let plan = store
            .mission(
                &intent,
                IntentInput {
                    kdl: PERFORMANCE_REGISTRATION.into(),
                    source_name: None,
                },
            )
            .unwrap();
        assert!(plan.blockers.is_empty(), "{:?}", plan.blockers);
        assert_eq!(
            store.desired_subjects().unwrap(),
            before,
            "preview is read-only"
        );
        let first = store
            .apply_as(
                &intent,
                &plan.subject_tokens,
                "register",
                Some("agent/fleet/fixture-project/speed"),
            )
            .unwrap();
        let again = store
            .apply_as(
                &intent,
                &plan.subject_tokens,
                "register",
                Some("agent/fleet/fixture-project/speed"),
            )
            .unwrap();
        assert_eq!(first.claim_ids, again.claim_ids);
        let next = store
            .mission(
                &intent,
                IntentInput {
                    kdl: PERFORMANCE_REGISTRATION.into(),
                    source_name: None,
                },
            )
            .unwrap();
        assert!(
            !store
                .apply_as(
                    &intent,
                    &next.subject_tokens,
                    "reregister",
                    Some("agent/fleet/fixture-project/speed")
                )
                .unwrap()
                .changed
        );
        assert_eq!(
            store
                .selected_desired_revision("observer/github/compoundingtech/smalltalk")
                .unwrap(),
            observer
        );
        assert_eq!(store.desired_subjects().unwrap().len(), before.len() + 1);
        assert!(store.active_mission_runs().unwrap().is_empty());
        let desired = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|item| item.subject == "subscription/fixture/main-performance-p0")
            .unwrap();
        let spec = crate::graph::subscription_spec(&desired.desired).unwrap();
        assert_eq!(spec.fields, ["main_performance_failures"]);
        assert_eq!(spec.observer, "observer/github/compoundingtech/smalltalk");
        assert_eq!(spec.to, "agent/fleet/fixture-project/speed");
    }

    #[test]
    fn main_performance_failures_registration_requires_existing_live_declarations_and_actor() {
        let intent = crate::graph::parse_intent(PERFORMANCE_REGISTRATION, "node").unwrap();
        let store = Store::open_memory("node").unwrap();
        let plan = store
            .mission(
                &intent,
                IntentInput {
                    kdl: PERFORMANCE_REGISTRATION.into(),
                    source_name: None,
                },
            )
            .unwrap();
        assert!(
            plan.blockers
                .iter()
                .any(|reason| reason.contains("missing-registration-observer"))
        );
        let refused = store
            .apply_as(
                &intent,
                &plan.subject_tokens,
                "no-observer",
                Some("agent/fleet/fixture-project/speed"),
            )
            .unwrap_err();
        assert_eq!(refused.code, "missing-registration-observer");
        assert!(store.desired_subjects().unwrap().is_empty());
        setup(&store);
        let plan = store
            .mission(
                &intent,
                IntentInput {
                    kdl: PERFORMANCE_REGISTRATION.into(),
                    source_name: None,
                },
            )
            .unwrap();
        let refused = store
            .apply_as(
                &intent,
                &plan.subject_tokens,
                "bad-actor",
                Some("daemon/forged"),
            )
            .unwrap_err();
        assert_eq!(refused.code, "invalid-registration-actor");
        let stop = crate::graph::parse_internal_intent(
            "version 2\nobserver \"github/compoundingtech/smalltalk\" { stop }",
            "node",
        )
        .unwrap();
        store.apply_internal(&stop, "stop-observer").unwrap();
        let refused = store
            .apply_as(
                &intent,
                &plan.subject_tokens,
                "stopped-observer",
                Some("agent/fleet/fixture-project/speed"),
            )
            .unwrap_err();
        assert_eq!(refused.code, "missing-registration-observer");
        assert!(
            !store
                .desired_subjects()
                .unwrap()
                .iter()
                .any(|item| item.kind == "subscription")
        );
    }

    #[test]
    fn main_performance_failures_registration_rejects_injection_overwrite_and_another_author() {
        let store = Store::open_memory("node").unwrap();
        setup(&store);
        let intent = crate::graph::parse_intent(PERFORMANCE_REGISTRATION, "node").unwrap();
        let plan = store
            .mission(
                &intent,
                IntentInput {
                    kdl: PERFORMANCE_REGISTRATION.into(),
                    source_name: None,
                },
            )
            .unwrap();
        let error = store
            .apply_as(
                &intent,
                &plan.subject_tokens,
                "foreign-recipient",
                Some("agent/other"),
            )
            .unwrap_err();
        assert_eq!(error.code, "foreign-registration-recipient");
        assert!(
            !store
                .desired_subjects()
                .unwrap()
                .iter()
                .any(|d| d.kind == "subscription")
        );
        let existing =
            crate::graph::parse_internal_intent(PERFORMANCE_REGISTRATION, "node").unwrap();
        store
            .apply_as(
                &existing,
                &plan.subject_tokens,
                "prior-author",
                Some("person/operator"),
            )
            .unwrap();
        let same = store
            .mission(
                &intent,
                IntentInput {
                    kdl: PERFORMANCE_REGISTRATION.into(),
                    source_name: None,
                },
            )
            .unwrap();
        let error = store
            .apply_as(
                &intent,
                &same.subject_tokens,
                "different-author",
                Some("agent/fleet/fixture-project/speed"),
            )
            .unwrap_err();
        assert_eq!(error.code, "message-registration-author");
        let different = PERFORMANCE_REGISTRATION.replace(
            "observer/github/compoundingtech/smalltalk",
            "observer/elsewhere",
        );
        let different = crate::graph::parse_intent(&different, "node").unwrap();
        assert!(
            store
                .mission(
                    &different,
                    IntentInput {
                        kdl: String::new(),
                        source_name: None
                    }
                )
                .unwrap()
                .blockers
                .iter()
                .any(|b| b.contains("message-registration-conflict"))
        );
        let stop = crate::graph::parse_internal_intent(
            "version 2\nstop \"agent/fleet/fixture-project/speed\"",
            "node",
        )
        .unwrap();
        store.apply_internal(&stop, "stop-target").unwrap();
        assert!(
            store
                .mission(
                    &intent,
                    IntentInput {
                        kdl: PERFORMANCE_REGISTRATION.into(),
                        source_name: None
                    }
                )
                .unwrap()
                .blockers
                .iter()
                .any(|b| b.contains("missing-registration-recipient"))
        );
    }

    #[test]
    fn main_performance_failures_replicated_registration_is_checked_without_a_client_marker() {
        let store = Store::open_memory("node").unwrap();
        setup(&store);
        let intent = crate::graph::parse_intent(PERFORMANCE_REGISTRATION, "node").unwrap();
        let desired = intent.subjects.values().next().unwrap().clone();
        store
            .connection
            .batched(|transaction| -> Result<(), St3Error> {
                let claim = append_claim_tx(
                    transaction,
                    "peer",
                    &desired.subject,
                    "intent.desired",
                    Some("agent/foreign"),
                    &serde_json::to_value(&desired).unwrap(),
                    &[],
                    None,
                )
                .map_err(claim_append_error)?;
                project_claim_isolated_tx(transaction, "projection:base", &claim, || {
                    select_replicated_desired(transaction, &claim, &desired)
                })?;
                let code: String = transaction
                    .query_row(
                        "SELECT error_code FROM projection_health WHERE aggregate=?1",
                        [format!("projection:base:{}", claim.id)],
                        |row| row.get(0),
                    )
                    .map_err(internal)?;
                assert_eq!(code, "foreign-registration-recipient");
                assert!(
                    current_desired_row_tx(transaction, &desired.subject)
                        .map_err(internal)?
                        .is_none()
                );
                Ok(())
            })
            .unwrap()
            .unwrap();
        let plan = store
            .mission(
                &intent,
                IntentInput {
                    kdl: PERFORMANCE_REGISTRATION.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply_as(
                &intent,
                &plan.subject_tokens,
                "legal-self-registration",
                Some("agent/fleet/fixture-project/speed"),
            )
            .unwrap();
        let claim = store
            .claims_for(&desired.subject, Some("intent.desired"))
            .unwrap()
            .into_iter()
            .find(|claim| claim.actor.as_deref() == Some("agent/fleet/fixture-project/speed"))
            .unwrap();
        store
            .connection
            .batched(|transaction| -> Result<(), St3Error> {
                validate_replicated(transaction, &claim, &desired)?;
                let source = PERFORMANCE_REGISTRATION.replace(
                    "on \"main_performance_failures\"",
                    "on \"main_performance_failures\"; on \"issues\"",
                );
                let broad = crate::graph::parse_internal_intent(&source, "node").unwrap();
                let broad = broad.subjects.values().next().unwrap();
                assert_eq!(
                    validate_replicated(transaction, &claim, broad)
                        .unwrap_err()
                        .code,
                    "invalid-message-registration"
                );
                Ok(())
            })
            .unwrap()
            .unwrap();
    }

    #[test]
    fn main_performance_failures_registration_does_not_allow_other_top_level_runtime() {
        for source in [
            "version 2\nobserver \"other\" { resource \"resource/repo\"; provider \"github.repository\"; locator \"owner/repo\"; field \"issues\" }",
            "version 2\nexec \"other\" { command \"true\" }",
            "version 2\nlane \"other\" { entries \"example\" }",
            "version 2\nsubscription \"other\" { observer \"observer/existing\"; delivery \"mission\" { mission \"example\"; resource \"source\"; workspace \"/tmp\" } }",
            "version 2\nsubscription \"other\" { observer \"observer/existing\"; to \"agent/existing\"; on \"issues\"; delivery \"message\" { every \"30m\" } }",
        ] {
            assert_eq!(
                crate::graph::parse_intent(source, "node").unwrap_err().code,
                "runtime-outside-mission"
            );
        }
        // Names belong in the deployment fixture, not a parser allowlist.
        assert!(
            crate::graph::parse_intent(
                &PERFORMANCE_REGISTRATION.replace(
                    "on \"main_performance_failures\"",
                    "on \"main_performance_failures\"; on \"issues\""
                ),
                "node"
            )
            .is_err()
        );
        for field in ["issues", "pull_requests"] {
            assert!(
                crate::graph::parse_intent(
                    &PERFORMANCE_REGISTRATION.replace("main_performance_failures", field),
                    "node"
                )
                .is_err()
            );
        }
        let generic = PERFORMANCE_REGISTRATION
            .replace("fixture/main-performance-p0", "another/registration")
            .replace(
                "observer/github/compoundingtech/smalltalk",
                "observer/already-declared",
            )
            .replace(
                "agent/fleet/fixture-project/speed",
                "agent/another/recipient",
            );
        assert!(crate::graph::parse_intent(&generic, "node").is_ok());
    }
}
