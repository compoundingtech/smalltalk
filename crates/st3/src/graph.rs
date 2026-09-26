use std::collections::{BTreeMap, BTreeSet, HashSet};

use kdl::{KdlDocument, KdlNode, KdlValue};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use crate::model::{
    DesiredSubject, GateSpec, LaunchSpec, MemberKind, MemberLifecycle, MemberSpec,
    MissionRevisionOperation, MissionRunCreation, MissionRunDeclaration, NamedCancellation,
    NormalizedIntent, ObserverSpec, PlannerSpec, PlanningFeedbackOperation,
    PlanningSessionCreation, PlanningSessionDeclaration, QuantifiedFieldSpec,
    ReplicaRepairDeclaration, ResourceRefreshOperation, RestartIntensity, RestartType,
    RuntimeResetOperation, ScheduleSpec, St3Error, SubscriptionConditionSpec, SubscriptionSpec,
};

const ROOT_NODES: &[&str] = &[
    "account",
    "agent",
    "exec",
    "pty",
    "host",
    "doc",
    "resource",
    "observer",
    "subscription",
    "person",
    "mission",
    "mission-run",
    "planning-session",
    "message",
    "repair",
    "schedule",
    "stop",
];

pub(crate) fn is_mission_declaration(name: &str) -> bool {
    matches!(
        name,
        "agent"
            | "exec"
            | "pty"
            | "host"
            | "doc"
            | "resource"
            | "observer"
            | "subscription"
            | "person"
            | "message"
            | "schedule"
            | "stop"
    )
}

struct ParseContext {
    default_host: String,
    subjects: BTreeMap<String, DesiredSubject>,
    document_refs: BTreeSet<String>,
    owner_run: Option<String>,
    allow_execution_root: bool,
    mission_runs: BTreeMap<String, MissionRunDeclaration>,
    planning_sessions: BTreeMap<String, PlanningSessionDeclaration>,
    resource_refreshes: Vec<ResourceRefreshOperation>,
    replica_repairs: Vec<ReplicaRepairDeclaration>,
}

pub fn parse_intent(source: &str, default_host: &str) -> Result<NormalizedIntent, St3Error> {
    let intent = parse_intent_with_owner(source, default_host, None, false)?;
    validate_mission_runtimes(&intent, default_host)?;
    Ok(intent)
}

pub(crate) fn parse_internal_intent(
    source: &str,
    default_host: &str,
) -> Result<NormalizedIntent, St3Error> {
    parse_intent_with_owner(source, default_host, None, true)
}

#[cfg(test)]
pub(crate) fn parse_test_intent(
    source: &str,
    default_host: &str,
) -> Result<NormalizedIntent, St3Error> {
    parse_internal_intent(source, default_host)
}

pub(crate) fn parse_execution_intent(
    source: &str,
    default_host: &str,
    run_id: &str,
) -> Result<NormalizedIntent, St3Error> {
    let owner = format!(
        "mission-run/{}",
        run_id.strip_prefix("mission-run/").unwrap_or(run_id)
    );
    parse_intent_with_owner(source, default_host, Some(&owner), true)
}

pub fn validate_mission_runtimes(
    intent: &NormalizedIntent,
    default_host: &str,
) -> Result<BTreeSet<String>, St3Error> {
    fn validate_mission(
        mission: &crate::model::MissionSpec,
        default_host: &str,
        subjects: &mut BTreeSet<String>,
    ) -> Result<(), St3Error> {
        fn validate_source(
            source: &str,
            variables: &BTreeMap<String, String>,
            default_host: &str,
            subjects: &mut BTreeSet<String>,
        ) -> Result<(), St3Error> {
            let source = crate::mission::interpolate_kdl(source, variables)?;
            let runtime = parse_execution_intent(&source, default_host, "migration-proof")?;
            subjects.extend(runtime.subjects.keys().cloned());
            Ok(())
        }

        let mut variables = BTreeMap::from([
            ("ST_MISSION".into(), mission.id.clone()),
            ("ST_MISSION_REVISION".into(), mission.revision.clone()),
            ("ST_MISSION_RUN".into(), "migration-proof".into()),
            ("ST_RUN_GENERATION".into(), "migration-generation".into()),
            ("ST_ROOT_MISSION_RUN".into(), "migration-proof".into()),
            ("ST_ROOT_MISSION_RUN_ID".into(), "migration-proof".into()),
            ("ST_WORKSPACE".into(), "/tmp/st3-migration-workspace".into()),
            ("ST_REQUESTER".into(), "person/migration-reviewer".into()),
            ("ST_STEP".into(), "migration-step".into()),
            (
                "ST_STEP_RUN".into(),
                "step-run/migration-generation/migration-step".into(),
            ),
            ("ST_ATTEMPT".into(), "1".into()),
            ("ST_ASSIGNEE".into(), "agent/migration-proof/worker".into()),
            ("ST_PARENT_STEP_RUN".into(), String::new()),
            ("ST_GATE".into(), "migration-gate".into()),
            ("ST_AGENT".into(), "agent/migration-proof/worker".into()),
            ("ST_LOOP_ROUND".into(), "1".into()),
            ("ST_LOOP_FEEDBACK".into(), String::new()),
            ("ST_LOOP_ITEM_ID".into(), "migration-item".into()),
            ("ST_CANDIDATE_INDEX".into(), "1".into()),
            ("loop.round".into(), "1".into()),
            ("loop.feedback".into(), String::new()),
            ("loop.item.id".into(), "migration-item".into()),
            ("loop.item.*".into(), "migration-value".into()),
            ("candidate.index".into(), "1".into()),
            ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
        ]);
        variables.extend(mission.inputs.iter().map(|(name, input)| {
            let value = match input.kind {
                crate::model::MissionInputKind::Text => format!("migration-{name}"),
                crate::model::MissionInputKind::Resource => {
                    format!("resource/migration-{name}")
                }
            };
            (format!("input.{name}"), value)
        }));
        if let Some(source) = &mission.declarations_kdl {
            validate_source(source, &variables, default_host, subjects)?;
        }
        for step in mission.steps.values() {
            if let Some(source) = &step.declarations_kdl {
                validate_source(source, &variables, default_host, subjects)?;
            }
            if let Some(nested) = &step.nested_mission {
                validate_mission(nested, default_host, subjects)?;
            }
        }
        Ok(())
    }

    let mut subjects = BTreeSet::new();
    for mission in intent.missions.values() {
        validate_mission(mission, default_host, &mut subjects)?;
    }
    Ok(subjects)
}

fn owner_run_id(owner: &str) -> &str {
    owner.strip_prefix("mission-run/").unwrap_or(owner)
}

fn parse_intent_with_owner(
    source: &str,
    default_host: &str,
    owner_run: Option<&str>,
    allow_execution_root: bool,
) -> Result<NormalizedIntent, St3Error> {
    if source.len() > 16 * 1024 * 1024 {
        return Err(St3Error::new(
            "intent-too-large",
            "an intent cannot exceed 16 MiB",
        ));
    }
    let mut document: KdlDocument = source
        .parse::<KdlDocument>()
        .map_err(|error| St3Error::new("invalid-kdl", error.to_string()))?;
    st2::kdl_version::ensure_st3_version(&document)
        .map_err(|error| St3Error::new("unsupported-kdl-version", error.to_string()))?;
    let mut deprecated_syntax = BTreeSet::new();
    canonicalize_terminal_declarations(&mut document, &mut deprecated_syntax);
    let canonical_document = document.clone();
    lower_terminal_declarations(&mut document);
    let declarations = document
        .nodes()
        .iter()
        .filter(|node| node.name().value() != "version")
        .collect::<Vec<_>>();
    if declarations.is_empty() {
        return Err(St3Error::new(
            "invalid-root",
            "an st3 publication must contain at least one declaration after `version 2`",
        ));
    }
    if declarations
        .iter()
        .any(|node| node.name().value() == "subgraph")
    {
        return Err(St3Error::new(
            "removed-subgraph",
            "`subgraph` is not part of st3 KDL; publish declarations directly after `version 2`",
        ));
    }
    let missions = crate::mission::parse_missions(&document, default_host)?;
    let mut context = ParseContext {
        default_host: default_host.to_owned(),
        subjects: BTreeMap::new(),
        document_refs: BTreeSet::new(),
        owner_run: owner_run.map(str::to_owned),
        allow_execution_root,
        mission_runs: BTreeMap::new(),
        planning_sessions: BTreeMap::new(),
        resource_refreshes: Vec::new(),
        replica_repairs: Vec::new(),
    };
    for node in &declarations {
        parse_desired_node(node, None, &mut context)?;
    }
    for node in document.nodes() {
        collect_document_refs(node, &mut context.document_refs)?;
    }
    if let Some(run) = owner_run {
        rewrite_owned_references(&mut context.subjects, owner_run_id(run));
    }
    let normalized_nodes = canonical_document
        .nodes()
        .iter()
        .filter(|node| node.name().value() != "version")
        .map(canonical_node)
        .collect::<Result<Vec<_>, _>>()?;
    let normalized = json!({ "version": 2, "declarations": normalized_nodes });
    let source_hash = hash_json(&normalized);
    Ok(NormalizedIntent {
        schema: "st3.v1".into(),
        source_hash,
        subjects: context.subjects,
        missions,
        mission_runs: context.mission_runs,
        planning_sessions: context.planning_sessions,
        resource_refreshes: context.resource_refreshes,
        replica_repairs: context.replica_repairs,
        document_refs: context.document_refs,
        deprecated_syntax,
        normalized,
    })
}

fn visit_nodes_mut(document: &mut KdlDocument, visit: &mut impl FnMut(&mut KdlNode)) {
    for node in document.nodes_mut() {
        visit(node);
        if let Some(children) = node.children_mut() {
            visit_nodes_mut(children, visit);
        }
    }
}

fn canonicalize_terminal_declarations(
    document: &mut KdlDocument,
    deprecated_syntax: &mut BTreeSet<String>,
) {
    visit_nodes_mut(document, &mut |node| {
        if node.name().value() == "pty" {
            deprecated_syntax.insert("pty".into());
            node.set_name("terminal");
        }
    });
}

fn lower_terminal_declarations(document: &mut KdlDocument) {
    visit_nodes_mut(document, &mut |node| {
        if node.name().value() == "terminal" {
            node.set_name("pty");
        }
    });
}

pub fn resolve_document_references(
    source: &str,
    bindings: &BTreeMap<String, String>,
) -> Result<String, St3Error> {
    let mut document = source
        .parse::<KdlDocument>()
        .map_err(|error| St3Error::new("invalid-kdl", error.to_string()))?;
    for node in document.nodes_mut() {
        resolve_node_documents(node, bindings);
    }
    document.autoformat();
    Ok(document.to_string())
}

fn resolve_node_documents(node: &mut KdlNode, bindings: &BTreeMap<String, String>) {
    for entry in node.entries_mut() {
        let replacement = match entry.value() {
            KdlValue::String(value) if value.starts_with("doc/") && !value.contains('@') => {
                bindings.get(value).map(|hash| format!("{value}@{hash}"))
            }
            _ => None,
        };
        if let Some(replacement) = replacement {
            entry.set_value(replacement);
        }
    }
    if let Some(children) = node.children_mut() {
        for child in children.nodes_mut() {
            resolve_node_documents(child, bindings);
        }
    }
}

fn parse_desired_node(
    node: &KdlNode,
    enclosing_host: Option<&str>,
    context: &mut ParseContext,
) -> Result<(), St3Error> {
    reject_type(node)?;
    let kind = node.name().value();
    if !ROOT_NODES.contains(&kind) {
        return Err(St3Error::new(
            "unknown-node",
            format!("unknown desired-state node `{kind}`"),
        ));
    }
    if !context.allow_execution_root
        && matches!(
            kind,
            "exec" | "pty" | "observer" | "subscription" | "schedule"
        )
    {
        return Err(St3Error::new(
            "runtime-outside-mission",
            format!("`{kind}` must be inside a mission or step"),
        ));
    }
    if kind == "account" && context.owner_run.is_some() {
        return Err(St3Error::new(
            "account-inside-mission",
            "an account declaration must be at the root",
        ));
    }
    match kind {
        "host" => parse_host(node, context),
        "agent" => parse_agent(node, enclosing_host, context),
        "exec" | "pty" => parse_standalone_member(node, kind, enclosing_host, context),
        "mission" => Ok(()),
        "mission-run" => parse_mission_run_declaration(node, context),
        "planning-session" => parse_planning_session_declaration(node, context),
        "resource" => parse_resource_declaration(node, context),
        "repair" => parse_replica_repair(node, context),
        "stop" => parse_stop(node, context),
        _ => parse_structure(node, kind, context),
    }
}

fn parse_mission_run_declaration(
    node: &KdlNode,
    context: &mut ParseContext,
) -> Result<(), St3Error> {
    ensure_no_properties(node)?;
    let id = one_string_with_children(node)?;
    let subject = namespaced("mission-run", &id);
    validate_full_subject(&subject)?;
    let body = node.children().ok_or_else(|| {
        St3Error::new(
            "empty-mission-run",
            "a mission-run declaration needs a body",
        )
    })?;
    reject_unknown_children(
        body,
        &[
            "mission",
            "workspace",
            "requester",
            "mode",
            "input",
            "revision",
            "reset",
            "cancellation",
        ],
        "mission-run",
        &subject,
    )?;
    let has_creation = body.nodes().iter().any(|child| {
        matches!(
            child.name().value(),
            "mission" | "workspace" | "requester" | "mode" | "input"
        )
    });
    let creation = if has_creation {
        let mission_ref = required_child_string(body, "mission", &subject)?;
        let (mission, revision) = exact_mission_revision(&mission_ref)?;
        let workspace = required_child_string(body, "workspace", &subject)?;
        if !workspace.starts_with('/') {
            return Err(St3Error::new(
                "relative-mission-run-workspace",
                "a published mission-run workspace must be absolute",
            ));
        }
        let requester = required_child_string(body, "requester", &subject)?;
        validate_full_subject(&requester)?;
        if !matches!(requester.split('/').next(), Some("person" | "agent")) {
            return Err(St3Error::new(
                "invalid-mission-run-requester",
                "a mission-run requester must be a person or agent subject",
            ));
        }
        let mode = child_string(body, "mode")?.unwrap_or_else(|| "run".into());
        if !matches!(mode.as_str(), "run" | "eval") {
            return Err(St3Error::new(
                "invalid-run-mode",
                format!("run mode `{mode}` is not registered"),
            ));
        }
        let mut inputs = BTreeMap::new();
        for input in body
            .nodes()
            .iter()
            .filter(|child| child.name().value() == "input")
        {
            ensure_no_properties(input)?;
            ensure_no_children(input)?;
            let values = positional_values(input);
            if values.len() != 2 {
                return Err(St3Error::new(
                    "invalid-mission-run-input",
                    "a mission-run input needs a name and a value",
                ));
            }
            let name = value_string(values[0])?;
            let value = value_string(values[1])?;
            if inputs.insert(name.clone(), value).is_some() {
                return Err(St3Error::new(
                    "duplicate-mission-run-input",
                    format!("mission run `{subject}` repeats input `{name}`"),
                ));
            }
        }
        Some(MissionRunCreation {
            mission,
            revision,
            workspace,
            requester,
            inputs,
            mode,
        })
    } else {
        None
    };
    let mut incoming = MissionRunDeclaration {
        subject: subject.clone(),
        creation,
        ..MissionRunDeclaration::default()
    };
    for child in body.nodes() {
        match child.name().value() {
            "revision" => {
                let operation = parse_mission_revision(child)?;
                insert_named_operation(&mut incoming.revisions, operation.id.clone(), operation)?;
            }
            "reset" => {
                let operation = parse_runtime_reset(child)?;
                insert_named_operation(&mut incoming.resets, operation.id.clone(), operation)?;
            }
            "cancellation" => {
                let operation = parse_named_cancellation(child)?;
                insert_named_operation(
                    &mut incoming.cancellations,
                    operation.id.clone(),
                    operation,
                )?;
            }
            _ => {}
        }
    }
    merge_mission_run_declaration(context, incoming)
}

fn exact_mission_revision(value: &str) -> Result<(String, String), St3Error> {
    let (mission, revision) = value.rsplit_once('@').ok_or_else(|| {
        St3Error::new(
            "unpinned-mission-run",
            "a mission-run must name an exact mission revision as `mission/ID@REVISION`",
        )
    })?;
    let mission = mission.strip_prefix("mission/").unwrap_or(mission);
    validate_name(mission, false)?;
    if revision.len() != 64 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(St3Error::new(
            "invalid-mission-revision",
            "a mission revision must be a 64-character SHA-256 hash",
        ));
    }
    Ok((mission.into(), revision.to_ascii_lowercase()))
}

fn parse_mission_revision(node: &KdlNode) -> Result<MissionRevisionOperation, St3Error> {
    ensure_no_properties(node)?;
    let id = one_string_with_children(node)?;
    validate_name(&id, false)?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("empty-revision", format!("revision `{id}` needs a body")))?;
    reject_unknown_children(
        body,
        &["mission", "from", "reason", "cancellation"],
        "revision",
        &id,
    )?;
    let (mission, revision) =
        exact_mission_revision(&required_child_string(body, "mission", &id)?)?;
    let from_generation = namespaced("run-generation", &required_child_string(body, "from", &id)?);
    validate_full_subject(&from_generation)?;
    let cancellation = unique_child(body, "cancellation")?
        .map(parse_named_cancellation)
        .transpose()?;
    Ok(MissionRevisionOperation {
        id: id.clone(),
        mission,
        revision,
        from_generation,
        reason: required_nonempty_child(body, "reason", &id)?,
        cancellation,
    })
}

fn parse_runtime_reset(node: &KdlNode) -> Result<RuntimeResetOperation, St3Error> {
    ensure_no_properties(node)?;
    let id = one_string_with_children(node)?;
    validate_name(&id, false)?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("empty-reset", format!("reset `{id}` needs a body")))?;
    reject_unknown_children(body, &["runtime", "from", "reason"], "reset", &id)?;
    let from_generation = namespaced("run-generation", &required_child_string(body, "from", &id)?);
    validate_full_subject(&from_generation)?;
    Ok(RuntimeResetOperation {
        id: id.clone(),
        runtime: required_child_string(body, "runtime", &id)?,
        from_generation,
        reason: required_nonempty_child(body, "reason", &id)?,
    })
}

fn parse_named_cancellation(node: &KdlNode) -> Result<NamedCancellation, St3Error> {
    ensure_no_properties(node)?;
    let id = one_string_with_children(node)?;
    validate_name(&id, false)?;
    let body = node.children().ok_or_else(|| {
        St3Error::new(
            "empty-cancellation",
            format!("cancellation `{id}` needs a body"),
        )
    })?;
    reject_unknown_children(body, &["reason"], "cancellation", &id)?;
    Ok(NamedCancellation {
        id: id.clone(),
        reason: required_nonempty_child(body, "reason", &id)?,
    })
}

fn required_nonempty_child(
    body: &KdlDocument,
    name: &str,
    owner: &str,
) -> Result<String, St3Error> {
    let value = required_child_string(body, name, owner)?;
    if value.trim().is_empty() {
        return Err(St3Error::new(
            "empty-operation-value",
            format!("`{owner}` needs a non-empty `{name}`"),
        ));
    }
    Ok(value)
}

fn insert_named_operation<T: Eq>(
    target: &mut BTreeMap<String, T>,
    id: String,
    operation: T,
) -> Result<(), St3Error> {
    if let Some(current) = target.get(&id) {
        if current == &operation {
            return Ok(());
        }
        return Err(St3Error::new(
            "immutable-operation-id",
            format!("operation `{id}` repeats with different content"),
        ));
    }
    target.insert(id, operation);
    Ok(())
}

fn merge_mission_run_declaration(
    context: &mut ParseContext,
    incoming: MissionRunDeclaration,
) -> Result<(), St3Error> {
    let subject = incoming.subject.clone();
    let current = context
        .mission_runs
        .entry(subject.clone())
        .or_insert_with(|| MissionRunDeclaration {
            subject,
            ..MissionRunDeclaration::default()
        });
    if let Some(creation) = incoming.creation {
        if current
            .creation
            .as_ref()
            .is_some_and(|value| value != &creation)
        {
            return Err(St3Error::new(
                "immutable-mission-run",
                format!(
                    "mission run `{}` repeats with different creation fields",
                    current.subject
                ),
            ));
        }
        current.creation.get_or_insert(creation);
    }
    for (id, operation) in incoming.revisions {
        insert_named_operation(&mut current.revisions, id, operation)?;
    }
    for (id, operation) in incoming.resets {
        insert_named_operation(&mut current.resets, id, operation)?;
    }
    for (id, operation) in incoming.cancellations {
        insert_named_operation(&mut current.cancellations, id, operation)?;
    }
    Ok(())
}

pub(crate) fn planning_planner_subject(session: &str) -> String {
    let digest = hex::encode(Sha256::digest(session.as_bytes()));
    format!("agent/planner.{}", &digest[..20])
}

fn parse_resource_declaration(node: &KdlNode, context: &mut ParseContext) -> Result<(), St3Error> {
    ensure_no_properties(node)?;
    let name = one_string_with_children(node)?;
    validate_name(&name, false)?;
    let resource = namespaced("resource", &name);
    let body = node.children().ok_or_else(|| {
        St3Error::new(
            "missing-resource-body",
            "a resource needs a kind or a refresh operation",
        )
    })?;
    let mut refresh_count = 0;
    for refresh in body
        .nodes()
        .iter()
        .filter(|child| child.name().value() == "refresh")
    {
        refresh_count += 1;
        let operation = parse_resource_refresh(&resource, refresh)?;
        if let Some(current) = context
            .resource_refreshes
            .iter()
            .find(|current| current.resource == resource && current.id == operation.id)
        {
            if current != &operation {
                return Err(St3Error::new(
                    "immutable-operation-id",
                    format!(
                        "refresh `{}` for `{resource}` repeats with different content",
                        operation.id
                    ),
                ));
            }
        } else {
            context.resource_refreshes.push(operation);
        }
    }
    let has_kind = body
        .nodes()
        .iter()
        .any(|child| child.name().value() == "kind");
    if has_kind {
        let mut desired = node.clone();
        desired
            .children_mut()
            .as_mut()
            .expect("the resource body exists")
            .nodes_mut()
            .retain(|child| child.name().value() != "refresh");
        parse_structure(&desired, "resource", context)?;
    } else if body
        .nodes()
        .iter()
        .any(|child| child.name().value() != "refresh")
    {
        return Err(St3Error::new(
            "invalid-resource-operation",
            format!("resource `{resource}` has a field but no kind"),
        ));
    }
    if !has_kind && refresh_count == 0 {
        return Err(St3Error::new(
            "empty-resource-operation",
            format!("resource `{resource}` has no declaration or operation"),
        ));
    }
    Ok(())
}

fn parse_resource_refresh(
    resource: &str,
    node: &KdlNode,
) -> Result<ResourceRefreshOperation, St3Error> {
    ensure_no_properties(node)?;
    let id = one_string_with_children(node)?;
    validate_name(&id, false)?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("empty-refresh", format!("refresh `{id}` needs a body")))?;
    reject_unknown_children(body, &["timeout"], "refresh", &id)?;
    let timeout_ms = child_string(body, "timeout")?
        .map(|value| parse_duration(&value, true))
        .transpose()?
        .unwrap_or(30_000);
    Ok(ResourceRefreshOperation {
        resource: resource.into(),
        id,
        timeout_ms,
    })
}

fn parse_replica_repair(node: &KdlNode, context: &mut ParseContext) -> Result<(), St3Error> {
    ensure_no_properties(node)?;
    let record_ref = one_string_with_children(node)?;
    let record_id = record_ref.strip_prefix("record/").ok_or_else(|| {
        St3Error::new(
            "invalid-replica-record",
            "a repair target must use the `record/HASH` form",
        )
    })?;
    if record_id.len() != 64 || !record_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(St3Error::new(
            "invalid-replica-record",
            "a repair target must contain one 64-character hexadecimal hash",
        ));
    }
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("empty-repair", "a repair needs a replacement and reason"))?;
    reject_unknown_children(body, &["replacement", "reason"], "repair", &record_ref)?;
    let replacement_claim_id = required_child_string(body, "replacement", &record_ref)?;
    let reason = required_child_string(body, "reason", &record_ref)?;
    if replacement_claim_id.trim().is_empty() || reason.trim().is_empty() {
        return Err(St3Error::new(
            "empty-repair-field",
            "a repair replacement and reason cannot be empty",
        ));
    }
    if replacement_claim_id.len() != 64
        || !replacement_claim_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(St3Error::new(
            "invalid-replacement-claim",
            "a repair replacement must be one 64-character claim ID",
        ));
    }
    let repair = ReplicaRepairDeclaration {
        record_ref: record_ref.clone(),
        replacement_claim_id,
        reason,
    };
    if let Some(existing) = context
        .replica_repairs
        .iter()
        .find(|existing| existing.record_ref == record_ref)
    {
        if existing != &repair {
            return Err(St3Error::new(
                "conflicting-repair",
                format!("repair `{record_ref}` repeats with different content"),
            ));
        }
    } else {
        context.replica_repairs.push(repair);
    }
    Ok(())
}

fn parse_planning_session_declaration(
    node: &KdlNode,
    context: &mut ParseContext,
) -> Result<(), St3Error> {
    ensure_no_properties(node)?;
    let id = one_string_with_children(node)?;
    let subject = namespaced("planning-session", &id);
    validate_full_subject(&subject)?;
    let body = node.children().ok_or_else(|| {
        St3Error::new(
            "empty-planning-session",
            "a planning-session declaration needs a body",
        )
    })?;
    reject_unknown_children(
        body,
        &[
            "mission",
            "request",
            "workspace",
            "requester",
            "planner",
            "target-run",
            "target-generation",
            "feedback",
            "cancellation",
        ],
        "planning-session",
        &subject,
    )?;
    let has_creation = body.nodes().iter().any(|child| {
        matches!(
            child.name().value(),
            "mission"
                | "request"
                | "workspace"
                | "requester"
                | "planner"
                | "target-run"
                | "target-generation"
        )
    });
    let creation = if has_creation {
        let mission = required_child_string(body, "mission", &subject)?;
        let mission = mission
            .strip_prefix("mission/")
            .unwrap_or(&mission)
            .to_owned();
        validate_name(&mission, false)?;
        let request = required_child_string(body, "request", &subject)?;
        validate_document_ref(&request)?;
        if !request.starts_with("doc/") || !request.contains('@') {
            return Err(St3Error::new(
                "unpinned-planning-request",
                "a planning request must name an exact document version",
            ));
        }
        let workspace = required_child_string(body, "workspace", &subject)?;
        if !workspace.starts_with('/') {
            return Err(St3Error::new(
                "relative-planning-workspace",
                "a published planning-session workspace must be absolute",
            ));
        }
        let requester = required_child_string(body, "requester", &subject)?;
        if !requester.starts_with("person/") {
            return Err(St3Error::new(
                "invalid-planning-requester",
                "a planning requester must be a person subject",
            ));
        }
        validate_full_subject(&requester)?;
        let planner_node = unique_child(body, "planner")?.ok_or_else(|| {
            St3Error::new("missing-planner", "a planning-session needs a planner")
        })?;
        ensure_no_properties(planner_node)?;
        let provider = one_string_with_children(planner_node)?;
        if !matches!(
            provider.as_str(),
            "codex" | "claude" | "pi" | "omp" | "opencode"
        ) {
            return Err(St3Error::new(
                "unsupported-planner",
                "the planner must use an eligible typed harness driver",
            ));
        }
        let planner_body = planner_node
            .children()
            .ok_or_else(|| St3Error::new("empty-planner", "a planner needs a body"))?;
        reject_unknown_children(planner_body, &["model", "effort"], "planner", &provider)?;
        let target_run =
            child_string(body, "target-run")?.map(|value| namespaced("mission-run", &value));
        let target_generation = child_string(body, "target-generation")?
            .map(|value| namespaced("run-generation", &value));
        if target_run.is_some() != target_generation.is_some() {
            return Err(St3Error::new(
                "incomplete-planning-target",
                "a targeted launch needs both target-run and target-generation",
            ));
        }
        if let Some(target) = &target_run {
            validate_full_subject(target)?;
        }
        if let Some(target) = &target_generation {
            validate_full_subject(target)?;
        }
        Some(PlanningSessionCreation {
            mission,
            request,
            workspace,
            requester,
            planner: PlannerSpec {
                model: child_string(planner_body, "model")?
                    .or_else(|| (provider == "codex").then(|| "gpt-6-sol".to_owned())),
                effort: child_string(planner_body, "effort")?
                    .or_else(|| (provider == "codex").then(|| "medium".to_owned())),
                provider,
            },
            target_run,
            target_generation,
        })
    } else {
        None
    };
    let mut incoming = PlanningSessionDeclaration {
        subject: subject.clone(),
        creation,
        ..PlanningSessionDeclaration::default()
    };
    for child in body.nodes() {
        match child.name().value() {
            "feedback" => {
                ensure_no_properties(child)?;
                let operation_id = one_string_with_children(child)?;
                validate_name(&operation_id, false)?;
                let feedback_body = child.children().ok_or_else(|| {
                    St3Error::new(
                        "empty-planning-feedback",
                        format!("feedback `{operation_id}` needs a body"),
                    )
                })?;
                reject_unknown_children(
                    feedback_body,
                    &["document", "variant"],
                    "feedback",
                    &operation_id,
                )?;
                let document = required_child_string(feedback_body, "document", &operation_id)?;
                validate_document_ref(&document)?;
                if !document.starts_with("doc/") || !document.contains('@') {
                    return Err(St3Error::new(
                        "unpinned-planning-feedback",
                        "planning feedback must name an exact document version",
                    ));
                }
                let operation = PlanningFeedbackOperation {
                    id: operation_id.clone(),
                    document,
                    variant: child_string(feedback_body, "variant")?
                        .unwrap_or_else(|| "default".into()),
                };
                insert_named_operation(&mut incoming.feedback, operation_id, operation)?;
            }
            "cancellation" => {
                let operation = parse_named_cancellation(child)?;
                insert_named_operation(
                    &mut incoming.cancellations,
                    operation.id.clone(),
                    operation,
                )?;
            }
            _ => {}
        }
    }
    let planner_creation = incoming.creation.clone();
    let has_cancellation = !incoming.cancellations.is_empty();
    let current = context
        .planning_sessions
        .entry(subject.clone())
        .or_insert_with(|| PlanningSessionDeclaration {
            subject: subject.clone(),
            ..PlanningSessionDeclaration::default()
        });
    if let Some(creation) = incoming.creation {
        if current
            .creation
            .as_ref()
            .is_some_and(|value| value != &creation)
        {
            return Err(St3Error::new(
                "immutable-planning-session",
                format!(
                    "launch `{}` repeats with different creation fields",
                    current.subject
                ),
            ));
        }
        current.creation.get_or_insert(creation);
    }
    for (id, operation) in incoming.feedback {
        insert_named_operation(&mut current.feedback, id, operation)?;
    }
    for (id, operation) in incoming.cancellations {
        insert_named_operation(&mut current.cancellations, id, operation)?;
    }
    if let Some(creation) = planner_creation {
        let planner = planning_planner_subject(&subject);
        if !context.subjects.contains_key(&planner) {
            let name = planner.strip_prefix("agent/").unwrap_or(&planner);
            let mut agent = KdlNode::new("agent");
            agent.entries_mut().push(kdl::KdlEntry::new(name));
            let mut agent_body = KdlDocument::new();
            agent_body
                .nodes_mut()
                .push(string_node("workspace", &creation.workspace));
            let mut harness = KdlNode::new("harness");
            harness
                .entries_mut()
                .push(kdl::KdlEntry::new(creation.planner.provider.as_str()));
            let mut harness_body = KdlDocument::new();
            if let Some(model) = &creation.planner.model {
                harness_body.nodes_mut().push(string_node("model", model));
            }
            if let Some(effort) = &creation.planner.effort {
                harness_body.nodes_mut().push(string_node("effort", effort));
            }
            let target_context = creation.target_run.as_ref().map_or_else(String::new, |run| {
                format!(
                    " Inspect the current target with `st3 --json mission show {run}` before you revise it. The target generation is `{}`.",
                    creation.target_generation.as_deref().unwrap_or_default()
                )
            });
            harness_body.nodes_mut().push(string_node(
                "prompt",
                &format!(
                    "You are the durable {} planner for launch `{id}`. Read `{}` with `st3 documents get`.{target_context} Write one Markdown mission and one complete version 2 KDL mission. The KDL mission ID must be `{}` and its state must be ready. Submit it with `st3 launch submit {id} --variant default --markdown MARKDOWN_FILE --kdl KDL_FILE`. Use temporary files outside the workspace, and remove them after submission. Do not change the workspace. Do not publish or run the mission. Stay ready for feedback until approval or cancellation.",
                    creation.planner.provider,
                    creation.request, creation.mission
                ),
            ));
            let arguments: &[&str] = match creation.planner.provider.as_str() {
                "codex" => &[
                    "--dangerously-bypass-approvals-and-sandbox",
                    "--dangerously-bypass-hook-trust",
                ],
                "claude" => &["--permission-mode", "bypassPermissions"],
                _ => &[],
            };
            if !arguments.is_empty() {
                let mut args = KdlNode::new("args");
                for argument in arguments {
                    args.entries_mut().push(kdl::KdlEntry::new(*argument));
                }
                harness_body.nodes_mut().push(args);
            }
            harness.set_children(harness_body);
            agent_body.nodes_mut().push(harness);
            agent.set_children(agent_body);
            parse_agent(&agent, None, context)?;
        }
    }
    if has_cancellation {
        let mut stop = KdlNode::new("stop");
        stop.entries_mut()
            .push(kdl::KdlEntry::new(planning_planner_subject(&subject)));
        parse_stop(&stop, context)?;
    }
    Ok(())
}

fn string_node(name: &str, value: &str) -> KdlNode {
    let mut node = KdlNode::new(name);
    node.entries_mut().push(kdl::KdlEntry::new(value));
    node
}

fn parse_host(node: &KdlNode, context: &mut ParseContext) -> Result<(), St3Error> {
    ensure_no_properties(node)?;
    let name = placement_host(one_string_with_children(node)?, &context.default_host);
    validate_name(&name, false)?;
    let subject = format!("host/{name}");
    let mut normalized = node.clone();
    normalized.entries_mut()[0].set_value(name.clone());
    insert_subject(
        context,
        DesiredSubject {
            subject,
            kind: "host".into(),
            desired: canonical_node(&normalized)?,
            member: None,
            owner_run: context.owner_run.clone(),
            owner_generation: None,
            owner_step: None,
        },
    )?;
    if let Some(children) = node.children() {
        for child in children.nodes() {
            if child.name().value() == "document" {
                ensure_no_properties(child)?;
                ensure_no_children(child)?;
                let reference = one_string(child)?;
                validate_document_ref(&reference)?;
                if !reference.starts_with("doc/") || !reference.contains('@') {
                    return Err(St3Error::new(
                        "unpinned-host-document",
                        "a host document must name an exact doc/NAME@HASH version",
                    ));
                }
                continue;
            }
            if matches!(child.name().value(), "agent" | "exec" | "pty") {
                parse_desired_node(child, Some(&name), context)?;
            } else {
                return Err(St3Error::new(
                    "invalid-host-child",
                    format!("host `{name}` cannot contain `{}`", child.name().value()),
                ));
            }
        }
    }
    Ok(())
}

fn parse_agent(
    node: &KdlNode,
    enclosing_host: Option<&str>,
    context: &mut ParseContext,
) -> Result<(), St3Error> {
    ensure_no_properties(node)?;
    let node_name = one_string_with_children(node)?;
    let children = node.children().ok_or_else(|| {
        St3Error::new(
            "missing-agent-body",
            format!("agent `{node_name}` has no body"),
        )
    })?;
    validate_agent_body(children, &node_name)?;
    let identity = child_string(children, "identity")?.unwrap_or(node_name);
    let host = child_string(children, "host")?
        .or_else(|| enclosing_host.map(str::to_owned))
        .unwrap_or_else(|| context.default_host.clone());
    let host = placement_host(host, &context.default_host);
    // A slash-qualified top-level identity is already the stable fleet seat path. Placement is a
    // property of the seat, not part of its identity. Retain the historical host prefix for simple
    // names so existing publications keep their subjects.
    let bus_id = if identity.contains('.') || identity.contains('/') {
        identity.clone()
    } else {
        format!("{host}.{identity}")
    };
    validate_name(&bus_id, false)?;
    let subject = context.owner_run.as_ref().map_or_else(
        || format!("agent/{bus_id}"),
        |run| format!("agent/{}/{identity}", owner_run_id(run)),
    );
    let runtime_id = context.owner_run.as_ref().map_or_else(
        || bounded_runtime_id(&bus_id.replace('/', ".")),
        |run| {
            bounded_runtime_id(&format!(
                "{}.{}",
                owner_run_id(run).replace('/', "."),
                identity.replace('/', ".")
            ))
        },
    );
    let (workspace, workspace_create) =
        parse_workspace(children)?.unwrap_or_else(|| (".".into(), false));
    let environment = parse_map_child(children, "env")?;
    let display_name = child_string(children, "name")?;
    let lifecycle = MemberLifecycle::Service;
    let restart = parse_restart_type(restart_type_value(children)?)?;
    let restart_intensity = parse_restart_intensity(children)?;
    let shutdown_timeout_ms = child_string(children, "shutdown-timeout")?
        .map(|value| parse_duration(&value, true))
        .transpose()?
        .unwrap_or(5_000);

    let driver_nodes = children
        .nodes()
        .iter()
        .filter(|child| child.name().value() == "harness")
        .collect::<Vec<_>>();
    let command = child_string(children, "command")?;
    let argv = child_strings(children, "argv")?;
    if driver_nodes.len() > 1 || command.is_some() as usize + argv.is_some() as usize > 1 {
        return Err(St3Error::new(
            "multiple-agent-launches",
            format!("agent `{bus_id}` has multiple compact launches or drivers"),
        ));
    }
    if !driver_nodes.is_empty() && (command.is_some() || argv.is_some()) {
        return Err(St3Error::new(
            "multiple-agent-launches",
            format!("agent `{bus_id}` mixes a driver and a compact launch"),
        ));
    }

    let mut primary = None;
    if let Some(driver) = driver_nodes.first() {
        primary = Some(driver_member(
            driver,
            &subject,
            &runtime_id,
            &host,
            &workspace,
            workspace_create,
            &environment,
            display_name.clone(),
            lifecycle.clone(),
            restart.clone(),
            restart_intensity.clone(),
            shutdown_timeout_ms,
        )?);
    } else if let Some(launch) = compact_launch(command, argv)? {
        primary = Some(MemberSpec {
            kind: MemberKind::Agent,
            host: host.clone(),
            runtime_id: runtime_id.clone(),
            workspace: workspace.clone(),
            workspace_create,
            cwd: workspace.clone(),
            terminal: true,
            launch,
            environment: environment.clone(),
            tags: BTreeMap::new(),
            display_name: display_name.clone(),
            lifecycle: lifecycle.clone(),
            restart: restart.clone(),
            restart_intensity: restart_intensity.clone(),
            shutdown_timeout_ms,
            driver: None,
        });
    }

    if let Some(member) = primary.as_mut() {
        member.tags.insert("st3.subject".into(), subject.clone());
    }

    let mut desired = canonical_node(node)?;
    normalize_agent_under(&mut desired, &host, context.owner_run.as_deref());
    insert_subject(
        context,
        DesiredSubject {
            subject: subject.clone(),
            kind: "agent".into(),
            desired,
            member: primary,
            owner_run: context.owner_run.clone(),
            owner_generation: None,
            owner_step: None,
        },
    )?;

    let mut task_names = HashSet::new();
    for child in children.nodes() {
        if !matches!(child.name().value(), "pty" | "exec") {
            continue;
        }
        let task_name = first_string(child)?;
        if !task_names.insert(task_name.clone()) {
            return Err(St3Error::new(
                "duplicate-task",
                format!("agent `{bus_id}` repeats task `{task_name}`"),
            ));
        }
        let task_subject = context.owner_run.as_ref().map_or_else(
            || format!("{}/{bus_id}/{task_name}", child.name().value()),
            |run| {
                format!(
                    "{}/{}/{identity}/{task_name}",
                    child.name().value(),
                    owner_run_id(run)
                )
            },
        );
        let mut member = task_member(
            child,
            &task_subject,
            child.name().value() == "pty",
            &host,
            &workspace,
            workspace_create,
            &environment,
            lifecycle.clone(),
            restart.clone(),
            restart_intensity.clone(),
            shutdown_timeout_ms,
            false,
        )?;
        member.tags.insert("st3.agent".into(), subject.clone());
        member
            .tags
            .insert("st3.subject".into(), task_subject.clone());
        insert_subject(
            context,
            DesiredSubject {
                subject: task_subject,
                kind: child.name().value().into(),
                desired: canonical_node(child)?,
                member: Some(member),
                owner_run: context.owner_run.clone(),
                owner_generation: None,
                owner_step: None,
            },
        )?;
    }
    if context
        .subjects
        .get(&subject)
        .is_some_and(|subject| subject.member.is_none())
        && task_names.is_empty()
    {
        return Err(St3Error::new(
            "missing-agent-launch",
            format!("agent `{bus_id}` has no launch"),
        ));
    }
    Ok(())
}

fn parse_standalone_member(
    node: &KdlNode,
    kind: &str,
    enclosing_host: Option<&str>,
    context: &mut ParseContext,
) -> Result<(), St3Error> {
    ensure_no_properties(node)?;
    let name = one_string_with_children(node)?;
    validate_name(&name, false)?;
    let subject = context.owner_run.as_ref().map_or_else(
        || namespaced(kind, &name),
        |run| format!("{kind}/{}/{name}", owner_run_id(run)),
    );
    let children = node.children().ok_or_else(|| {
        St3Error::new(
            "missing-member-body",
            format!("{kind} `{name}` has no body"),
        )
    })?;
    let host = child_string(children, "host")?
        .or_else(|| enclosing_host.map(str::to_owned))
        .unwrap_or_else(|| context.default_host.clone());
    let host = placement_host(host, &context.default_host);
    let (workspace, workspace_create) =
        parse_workspace(children)?.unwrap_or_else(|| (".".into(), false));
    let environment = parse_map_child(children, "env")?;
    let lifecycle = MemberLifecycle::Service;
    let restart = parse_restart_type(restart_type_value(children)?)?;
    let restart_intensity = parse_restart_intensity(children)?;
    let shutdown_timeout_ms = child_string(children, "shutdown-timeout")?
        .map(|value| parse_duration(&value, true))
        .transpose()?
        .unwrap_or(5_000);
    let mut member = task_member(
        node,
        &subject,
        kind == "pty",
        &host,
        &workspace,
        workspace_create,
        &environment,
        lifecycle,
        restart,
        restart_intensity,
        shutdown_timeout_ms,
        true,
    )?;
    member.tags.insert("st3.subject".into(), subject.clone());
    insert_subject(
        context,
        DesiredSubject {
            subject,
            kind: kind.into(),
            desired: canonical_node(node)?,
            member: Some(member),
            owner_run: context.owner_run.clone(),
            owner_generation: None,
            owner_step: None,
        },
    )
}

fn parse_structure(node: &KdlNode, kind: &str, context: &mut ParseContext) -> Result<(), St3Error> {
    let name = first_string(node)?;
    validate_name(&name, false)?;
    match kind {
        "account" => validate_account(node)?,
        "doc" => validate_doc(node)?,
        "resource" => validate_resource(node)?,
        "observer" => validate_observer(node)?,
        "subscription" => validate_subscription(node)?,
        "person" => {
            ensure_no_properties(node)?;
            one_string(node)?;
        }
        "message" => validate_message(node)?,
        "schedule" => validate_schedule(node)?,
        _ => unreachable!("the desired-state registry controls structure kinds"),
    }
    let subject = match kind {
        "doc" => format!("doc/{name}"),
        "observer" | "subscription" | "schedule" if context.owner_run.is_some() => {
            format!(
                "{kind}/{}/{}",
                owner_run_id(context.owner_run.as_deref().unwrap_or_default()),
                name
            )
        }
        _ => namespaced(kind, &name),
    };
    insert_subject(
        context,
        DesiredSubject {
            subject,
            kind: kind.into(),
            desired: canonical_node(node)?,
            member: None,
            owner_run: context.owner_run.clone(),
            owner_generation: None,
            owner_step: None,
        },
    )?;
    if kind == "doc" {
        let hash = required_child_string(
            node.children().expect("a validated doc has a body"),
            "hash",
            &name,
        )?;
        context.document_refs.insert(format!("doc/{name}@{hash}"));
    }
    Ok(())
}

fn validate_account(node: &KdlNode) -> Result<(), St3Error> {
    ensure_no_properties(node)?;
    one_string_with_children(node)?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("missing-account-body", "an account needs a body"))?;
    reject_unknown_children(
        body,
        &["provider", "external-account", "auth-type"],
        "account",
        "account",
    )?;
    required_child_string(body, "provider", "account")?;
    required_child_string(body, "external-account", "account")?;
    let auth = required_child_string(body, "auth-type", "account")?;
    if !matches!(auth.as_str(), "subscription" | "api-key") {
        return Err(St3Error::new(
            "invalid-auth-type",
            format!("invalid account auth type `{auth}`"),
        ));
    }
    Ok(())
}

fn parse_stop(node: &KdlNode, context: &mut ParseContext) -> Result<(), St3Error> {
    ensure_no_properties(node)?;
    ensure_no_children(node)?;
    let subject = one_string(node)?;
    if !matches!(subject.split('/').next(), Some("agent" | "exec" | "pty")) {
        return Err(St3Error::new(
            "invalid-stop-subject",
            format!("stop requires a full agent, exec, or PTY subject; got `{subject}`"),
        ));
    }
    let kind = subject.split('/').next().unwrap_or_default();
    let local = subject
        .strip_prefix(&format!("{kind}/"))
        .unwrap_or(&subject);
    let subject = context.owner_run.as_ref().map_or(subject.clone(), |run| {
        let run = owner_run_id(run);
        if local.starts_with(&format!("{run}/")) {
            subject.clone()
        } else {
            format!("{kind}/{run}/{local}")
        }
    });
    insert_subject(
        context,
        DesiredSubject {
            subject: subject.clone(),
            kind: "stop".into(),
            desired: json!({ "stop": subject }),
            member: None,
            owner_run: context.owner_run.clone(),
            owner_generation: None,
            owner_step: None,
        },
    )
}

fn rewrite_owned_references(subjects: &mut BTreeMap<String, DesiredSubject>, run: &str) {
    let mut aliases = BTreeMap::new();
    for subject in subjects.keys() {
        let Some((kind, rest)) = subject.split_once('/') else {
            continue;
        };
        let Some(local) = rest.strip_prefix(&format!("{run}/")) else {
            continue;
        };
        if matches!(
            kind,
            "agent" | "exec" | "pty" | "observer" | "subscription" | "schedule"
        ) {
            aliases.insert(format!("{kind}/{local}"), subject.clone());
            if kind == "agent" {
                aliases.insert(local.to_owned(), subject.clone());
            }
        }
    }
    fn rewrite(value: &mut Value, aliases: &BTreeMap<String, String>) {
        match value {
            Value::String(value) => {
                if let Some(replacement) = aliases.get(value) {
                    *value = replacement.clone();
                }
            }
            Value::Array(values) => {
                for value in values {
                    rewrite(value, aliases);
                }
            }
            Value::Object(values) => {
                for value in values.values_mut() {
                    rewrite(value, aliases);
                }
            }
            _ => {}
        }
    }
    fn rewrite_agent_parties(value: &mut Value, run: &str) {
        match value {
            Value::Array(values) => {
                for value in values {
                    rewrite_agent_parties(value, run);
                }
            }
            Value::Object(values) => {
                let agent_party = matches!(
                    values.get("name").and_then(Value::as_str),
                    Some("to" | "from" | "under")
                );
                if agent_party
                    && let Some(party) = values
                        .get_mut("arguments")
                        .and_then(Value::as_array_mut)
                        .and_then(|arguments| arguments.first_mut())
                        .and_then(|value| value.as_str())
                        .map(str::to_owned)
                    && party != "requester"
                    && !party.starts_with("person/")
                {
                    let local = party.strip_prefix("agent/").unwrap_or(&party);
                    let is_exact_agent_subject = party.starts_with("agent/") && local.contains('/');
                    if !is_exact_agent_subject
                        && !local.starts_with(&format!("{run}/"))
                        && let Some(value) = values
                            .get_mut("arguments")
                            .and_then(Value::as_array_mut)
                            .and_then(|arguments| arguments.first_mut())
                    {
                        *value = Value::String(format!("agent/{run}/{local}"));
                    }
                }
                for value in values.values_mut() {
                    rewrite_agent_parties(value, run);
                }
            }
            _ => {}
        }
    }
    for subject in subjects.values_mut() {
        rewrite(&mut subject.desired, &aliases);
        rewrite_agent_parties(&mut subject.desired, run);
    }
}

pub(crate) fn parse_gate(node: &KdlNode, default_host: &str) -> Result<GateSpec, St3Error> {
    reject_type(node)?;
    ensure_only_properties(node, &["type"])?;
    let name = one_string_with_children(node)?;
    if name.is_empty() || name.len() > 160 {
        return Err(St3Error::new(
            "invalid-gate-name",
            "a gate name must contain 1 through 160 bytes",
        ));
    }
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("missing-gate-body", format!("gate `{name}` has no body")))?;
    let gate_type = property_string(node, "type")?;
    if gate_type
        .as_deref()
        .is_some_and(|kind| matches!(kind, "llm" | "human"))
        || body
            .nodes()
            .iter()
            .any(|child| child.name().value() == "exec")
    {
        return parse_running_gate(node, name, default_host);
    }
    if gate_type.is_some() {
        return Err(St3Error::new(
            "invalid-gate-type",
            format!("gate `{name}` has an invalid type"),
        ));
    }
    if body.nodes().len() != 1 {
        return Err(St3Error::new(
            "invalid-gate-shape",
            format!("gate `{name}` needs exactly one predicate"),
        ));
    }
    parse_predicate_gate(&body.nodes()[0], name)
}

pub(crate) fn gate_name(gate: &GateSpec) -> &str {
    match gate {
        GateSpec::Exists { name, .. }
        | GateSpec::Empty { name, .. }
        | GateSpec::Field { name, .. }
        | GateSpec::Every { name, .. }
        | GateSpec::NotEvery { name, .. }
        | GateSpec::Has { name, .. }
        | GateSpec::Lacks { name, .. }
        | GateSpec::Deadline { name, .. }
        | GateSpec::Mechanical { name, .. }
        | GateSpec::Llm { name, .. }
        | GateSpec::Human { name, .. } => name,
    }
}

pub(crate) fn parse_baseline_gate(node: &KdlNode, name: String) -> Result<GateSpec, St3Error> {
    reject_type(node)?;
    ensure_no_properties(node)?;
    parse_predicate_gate(node, name)
}

fn parse_predicate_gate(child: &KdlNode, name: String) -> Result<GateSpec, St3Error> {
    reject_type(child)?;
    match child.name().value() {
        "exists" => {
            ensure_no_properties(child)?;
            let subject = one_string(child)?;
            validate_full_subject(&subject)?;
            Ok(GateSpec::Exists { name, subject })
        }
        "empty" => {
            ensure_no_properties(child)?;
            let subject = one_string(child)?;
            if !subject.starts_with("mission-run/") {
                return Err(St3Error::new(
                    "invalid-empty-subject",
                    "empty requires a full mission run subject",
                ));
            }
            validate_full_subject(&subject)?;
            Ok(GateSpec::Empty { name, subject })
        }
        "has" | "lacks" => {
            let values = positional_strings(child)?;
            if values.len() != 2 {
                return Err(St3Error::new(
                    "invalid-text-predicate",
                    "has and lacks require a subject and text",
                ));
            }
            validate_full_subject(&values[0])?;
            if !matches!(
                values[0].split('/').next(),
                Some("file" | "doc" | "message")
            ) {
                return Err(St3Error::new(
                    "unsupported-predicate-subject",
                    "has and lacks require a file, document, or message subject",
                ));
            }
            Ok(if child.name().value() == "has" {
                GateSpec::Has {
                    name,
                    subject: values[0].clone(),
                    text: values[1].clone(),
                }
            } else {
                GateSpec::Lacks {
                    name,
                    subject: values[0].clone(),
                    text: values[1].clone(),
                }
            })
        }
        "field" => {
            ensure_no_properties(child)?;
            let entries = child
                .entries()
                .iter()
                .filter(|entry| entry.name().is_none())
                .map(|entry| entry.value())
                .collect::<Vec<_>>();
            if entries.len() != 4 {
                return Err(St3Error::new(
                    "invalid-field-predicate",
                    "field requires path, subject, operator, and value",
                ));
            }
            let path = value_string(entries[0])?;
            if !valid_field_path(&path) {
                return Err(St3Error::new(
                    "invalid-field-path",
                    format!("invalid field path `{path}`"),
                ));
            }
            let operator = value_string(entries[2])?;
            if !matches!(operator.as_str(), "is" | "starts-with" | "contains") {
                return Err(St3Error::new(
                    "invalid-field-operator",
                    format!("invalid field operator `{operator}`"),
                ));
            }
            let subject = value_string(entries[1])?;
            validate_full_subject(&subject)?;
            Ok(GateSpec::Field {
                name,
                path,
                subject,
                operator,
                value: json_value(entries[3])?,
            })
        }
        "every" | "not-every" => {
            ensure_no_properties(child)?;
            let entries = child
                .entries()
                .iter()
                .filter(|entry| entry.name().is_none())
                .map(|entry| entry.value())
                .collect::<Vec<_>>();
            if entries.len() != 2 {
                return Err(St3Error::new(
                    "invalid-quantified-predicate",
                    "every and not-every require a list field path and subject",
                ));
            }
            let path = value_string(entries[0])?;
            if !valid_field_path(&path) {
                return Err(St3Error::new(
                    "invalid-field-path",
                    format!("invalid field path `{path}`"),
                ));
            }
            let subject = value_string(entries[1])?;
            validate_full_subject(&subject)?;
            let body = child.children().ok_or_else(|| {
                St3Error::new(
                    "empty-quantified-predicate",
                    "every and not-every require at least one field predicate",
                )
            })?;
            if body.nodes().is_empty() {
                return Err(St3Error::new(
                    "empty-quantified-predicate",
                    "every and not-every require at least one field predicate",
                ));
            }
            let mut fields = Vec::with_capacity(body.nodes().len());
            for predicate in body.nodes() {
                if predicate.name().value() != "field" {
                    return Err(St3Error::new(
                        "invalid-quantified-predicate",
                        "every and not-every accept only field predicates",
                    ));
                }
                ensure_no_properties(predicate)?;
                ensure_no_children(predicate)?;
                let entries = predicate
                    .entries()
                    .iter()
                    .filter(|entry| entry.name().is_none())
                    .map(|entry| entry.value())
                    .collect::<Vec<_>>();
                if entries.len() != 3 {
                    return Err(St3Error::new(
                        "invalid-quantified-field",
                        "a quantified field requires path, operator, and value",
                    ));
                }
                let path = value_string(entries[0])?;
                if !valid_field_path(&path) {
                    return Err(St3Error::new(
                        "invalid-field-path",
                        format!("invalid field path `{path}`"),
                    ));
                }
                let operator = value_string(entries[1])?;
                if !matches!(operator.as_str(), "is" | "starts-with" | "contains") {
                    return Err(St3Error::new(
                        "invalid-field-operator",
                        format!("invalid field operator `{operator}`"),
                    ));
                }
                fields.push(crate::model::QuantifiedFieldSpec {
                    path,
                    operator,
                    value: json_value(entries[2])?,
                });
            }
            let gate = if child.name().value() == "every" {
                GateSpec::Every {
                    name,
                    path,
                    subject,
                    fields,
                }
            } else {
                GateSpec::NotEvery {
                    name,
                    path,
                    subject,
                    fields,
                }
            };
            Ok(gate)
        }
        "deadline" => {
            let duration = one_duration(child)?;
            Ok(GateSpec::Deadline {
                name,
                duration_ms: duration,
            })
        }
        other => Err(St3Error::new(
            "unknown-gate",
            format!("unknown gate predicate `{other}`"),
        )),
    }
}

fn parse_running_gate(
    node: &KdlNode,
    name: String,
    default_host: &str,
) -> Result<GateSpec, St3Error> {
    ensure_only_properties(node, &["type"])?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("missing-gate-body", format!("gate `{name}` has no body")))?;
    let gate_type = property_string(node, "type")?;
    let allowed: &[&str] = match gate_type.as_deref() {
        None => &["exec", "host", "workspace", "env", "time-limit"],
        Some("llm") => &[
            "model",
            "host",
            "workspace",
            "tools",
            "env",
            "token-budget",
            "time-limit",
            "prompt",
        ],
        Some("human") => &["reviewer", "question", "review"],
        Some(other) => {
            return Err(St3Error::new(
                "invalid-gate-type",
                format!("gate `{name}` has invalid type `{other}`"),
            ));
        }
    };
    reject_unknown_children(body, allowed, "gate", &name)?;
    for child in allowed.iter().filter(|child| **child != "review") {
        unique_child(body, child)?;
    }
    if gate_type.as_deref() == Some("human") {
        let reviewer = required_child_string(body, "reviewer", &name)?;
        if !reviewer.starts_with("person/") {
            return Err(St3Error::new(
                "invalid-human-reviewer",
                "a human gate needs a full person subject",
            ));
        }
        let question = child_string(body, "question")?;
        let mut review_targets = Vec::new();
        for field in body
            .nodes()
            .iter()
            .filter(|field| field.name().value() == "review")
        {
            let target = one_string(field)?;
            validate_full_subject(&target)?;
            if review_targets.contains(&target) {
                return Err(St3Error::new(
                    "duplicate-human-review-target",
                    format!("human review target `{target}` repeats"),
                ));
            }
            review_targets.push(target);
        }
        return Ok(GateSpec::Human {
            name,
            reviewer,
            question,
            review_targets,
        });
    }
    let host = placement_host(required_child_string(body, "host", &name)?, default_host);
    let workspace = required_child_string(body, "workspace", &name)?;
    let environment = parse_map_child(body, "env")?;
    if let Some(env) = unique_child(body, "env")? {
        validate_string_map(env, true)?;
    }
    match gate_type.as_deref() {
        None => {
            let time_limit_ms = child_string(body, "time-limit")?
                .map(|value| parse_duration(&value, true))
                .transpose()?
                .unwrap_or(120_000);
            Ok(GateSpec::Mechanical {
                name: name.clone(),
                command: required_child_string(body, "exec", &name)?,
                host,
                workspace,
                environment,
                time_limit_ms,
            })
        }
        Some("llm") => {
            let tools = child_strings(body, "tools")?.ok_or_else(|| {
                St3Error::new("missing-gate-field", format!("gate `{name}` needs tools"))
            })?;
            let token_budget = child_integer(body, "token-budget")?.ok_or_else(|| {
                St3Error::new(
                    "missing-gate-field",
                    format!("gate `{name}` needs token-budget"),
                )
            })?;
            if token_budget <= 0 {
                return Err(St3Error::new(
                    "invalid-token-budget",
                    "an LLM token budget must be positive",
                ));
            }
            for tool in &tools {
                if !matches!(tool.as_str(), "shell" | "git" | "gh" | "network") {
                    return Err(St3Error::new(
                        "unsupported-capability",
                        format!("gate tool `{tool}` is not registered"),
                    ));
                }
            }
            let time_limit_ms = child_string(body, "time-limit")?
                .ok_or_else(|| {
                    St3Error::new(
                        "missing-gate-field",
                        format!("gate `{name}` needs time-limit"),
                    )
                })
                .and_then(|value| parse_duration(&value, true))?;
            Ok(GateSpec::Llm {
                name: name.clone(),
                model: required_child_string(body, "model", &name)?,
                host,
                workspace,
                tools,
                environment,
                token_budget: token_budget as u64,
                time_limit_ms,
                prompt: required_child_string(body, "prompt", &name)?,
            })
        }
        Some("human") => unreachable!("human gates return above"),
        Some(_) => unreachable!("gate type was validated"),
    }
}

#[allow(clippy::too_many_arguments)]
fn driver_member(
    driver: &KdlNode,
    subject: &str,
    runtime_id: &str,
    host: &str,
    workspace: &str,
    workspace_create: bool,
    environment: &BTreeMap<String, String>,
    display_name: Option<String>,
    lifecycle: MemberLifecycle,
    restart: RestartType,
    restart_intensity: RestartIntensity,
    shutdown_timeout_ms: u64,
) -> Result<MemberSpec, St3Error> {
    let name = one_string_with_children(driver)?;
    let children = driver.children().ok_or_else(|| {
        St3Error::new(
            "missing-driver-body",
            format!("harness `{name}` has no body"),
        )
    })?;
    let prompt = crate::boot::compose_prompt(child_string(children, "prompt")?.as_deref());
    let model = child_string(children, "model")?;
    let effort = child_string(children, "effort")?;
    let extra = child_strings(children, "args")?.unwrap_or_default();
    let mut provider = vec![name.clone()];
    if name == "claude" {
        // A typed Claude harness is always the real interactive TUI and always carries st3's
        // native channel. `dev-channels #true` is retained only so declarations written before
        // the channel became intrinsic continue to parse; opting out would create an agent that
        // cannot receive graph messages, so false is rejected below.
        provider.extend(["--channels".into(), st2::claude_channel::ST3_CHANNEL.into()]);
        // The channel wakes the real TUI, while Claude's own lifecycle hooks externalize the
        // resulting turn. Supplying the canonical registration as an additional native settings
        // source keeps arbitrary user workspaces untouched and gives every typed st3 seat the
        // same UserPromptSubmit/Stop state edges as a materialized st2 seat.
        provider.extend([
            "--settings".into(),
            st2::hooks::claude_st3_settings_registration().to_string(),
        ]);
    }
    if let Some(model) = model {
        match name.as_str() {
            "codex" => provider.extend(["--model".into(), model]),
            _ => provider.extend(["--model".into(), model]),
        }
    }
    if let Some(effort) = effort {
        match name.as_str() {
            "codex" => provider.extend(["-c".into(), format!("model_reasoning_effort={effort}")]),
            "pi" | "omp" => provider.extend(["--thinking".into(), effort]),
            "opencode" => {
                return Err(St3Error::new(
                    "invalid-driver-child",
                    "opencode does not accept effort",
                ));
            }
            _ => provider.extend(["--effort".into(), effort]),
        }
    }
    provider.extend(extra);
    match name.as_str() {
        "opencode" => provider.extend(["--prompt".into(), prompt]),
        _ => provider.push(prompt),
    }
    let mut wrapper = vec![
        "st3".into(),
        "driver".into(),
        name.clone(),
        "--subject".into(),
        subject.into(),
        "--".into(),
    ];
    wrapper.extend(provider);
    Ok(MemberSpec {
        kind: MemberKind::Agent,
        host: host.into(),
        runtime_id: runtime_id.into(),
        workspace: workspace.into(),
        workspace_create,
        cwd: workspace.into(),
        terminal: true,
        launch: LaunchSpec::Argv(wrapper),
        environment: environment.clone(),
        tags: BTreeMap::from([("st3.subject".into(), subject.into())]),
        display_name,
        lifecycle,
        restart,
        restart_intensity,
        shutdown_timeout_ms,
        driver: Some(name),
    })
}

#[allow(clippy::too_many_arguments)]
fn task_member(
    node: &KdlNode,
    subject: &str,
    terminal: bool,
    default_host: &str,
    default_workspace: &str,
    default_workspace_create: bool,
    default_environment: &BTreeMap<String, String>,
    default_lifecycle: MemberLifecycle,
    default_restart: RestartType,
    default_intensity: RestartIntensity,
    default_shutdown_timeout_ms: u64,
    standalone: bool,
) -> Result<MemberSpec, St3Error> {
    let body = node.children().ok_or_else(|| {
        St3Error::new(
            "missing-task-body",
            format!("member `{subject}` has no body"),
        )
    })?;
    validate_task_body(body, subject, standalone)?;
    let host = placement_host(
        child_string(body, "host")?.unwrap_or_else(|| default_host.into()),
        default_host,
    );
    let workspace_spec = parse_workspace(body)?;
    let workspace = workspace_spec
        .as_ref()
        .map(|(workspace, _)| workspace.clone())
        .unwrap_or_else(|| default_workspace.into());
    let workspace_create = workspace_spec
        .map(|(_, create)| create)
        .unwrap_or(default_workspace_create);
    let cwd = child_string(body, "cwd")?.unwrap_or_else(|| workspace.clone());
    let runtime_id = child_string(body, "id")?.unwrap_or_else(|| runtime_id(subject));
    let mut environment = default_environment.clone();
    environment.extend(parse_map_child(body, "env")?);
    if let Some(unset) = child_strings(body, "unset")? {
        for name in unset {
            environment.remove(&name);
        }
    }
    let command = child_string(body, "command")?;
    let argv = child_strings(body, "argv")?;
    let launch = compact_launch(command, argv)?.ok_or_else(|| {
        St3Error::new(
            "missing-task-launch",
            format!("member `{subject}` needs command or argv"),
        )
    })?;
    let lifecycle = default_lifecycle;
    let restart = restart_type_value(body)?
        .map(|value| parse_restart_type(Some(value)))
        .transpose()?
        .unwrap_or(default_restart);
    let restart_intensity = if has_block_child(body, "restart") {
        parse_restart_intensity(body)?
    } else {
        default_intensity
    };
    let shutdown_timeout_ms = child_string(body, "shutdown-timeout")?
        .map(|value| parse_duration(&value, true))
        .transpose()?
        .unwrap_or(default_shutdown_timeout_ms);
    Ok(MemberSpec {
        kind: if terminal {
            MemberKind::Pty
        } else {
            MemberKind::Exec
        },
        host,
        runtime_id,
        workspace,
        workspace_create,
        cwd,
        terminal,
        launch,
        environment,
        tags: parse_tags(body)?,
        display_name: None,
        lifecycle,
        restart,
        restart_intensity,
        shutdown_timeout_ms,
        driver: None,
    })
}

fn compact_launch(
    command: Option<String>,
    argv: Option<Vec<String>>,
) -> Result<Option<LaunchSpec>, St3Error> {
    match (command, argv) {
        (Some(command), None) => Ok(Some(LaunchSpec::Shell(command))),
        (None, Some(argv)) if !argv.is_empty() => Ok(Some(LaunchSpec::Argv(argv))),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(St3Error::new(
            "multiple-launches",
            "a member cannot contain both command and argv",
        )),
        (None, Some(_)) => Err(St3Error::new("empty-argv", "argv needs a program")),
    }
}

fn parse_restart_type(value: Option<String>) -> Result<RestartType, St3Error> {
    match value.as_deref() {
        None | Some("always") => Ok(RestartType::Always),
        Some("on-failure") => Ok(RestartType::OnFailure),
        Some("never") => Ok(RestartType::Never),
        Some(value) => Err(St3Error::new(
            "invalid-restart",
            format!("invalid restart type `{value}`"),
        )),
    }
}

fn placement_host(host: String, default_host: &str) -> String {
    if host == "local" {
        default_host.into()
    } else {
        host
    }
}

fn parse_restart_intensity(document: &KdlDocument) -> Result<RestartIntensity, St3Error> {
    let nodes = document
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "restart" && node.children().is_some())
        .collect::<Vec<_>>();
    let Some(node) = nodes.first().copied() else {
        return Ok(RestartIntensity::default());
    };
    if nodes.len() > 1 {
        return Err(St3Error::new(
            "duplicate-child",
            "a restart intensity block repeats",
        ));
    }
    let body = node.children().expect("checked");
    let attempts = child_integer(body, "attempts")?.unwrap_or(3);
    if attempts <= 0 || attempts > u32::MAX as i128 {
        return Err(St3Error::new(
            "invalid-restart-attempts",
            "restart attempts must be a positive u32",
        ));
    }
    let interval_ms = child_string(body, "interval")?
        .map(|value| parse_duration(&value, true))
        .transpose()?
        .unwrap_or(60_000);
    let delay_ms = child_string(body, "delay")?
        .map(|value| parse_duration(&value, false))
        .transpose()?
        .unwrap_or(0);
    let mode = child_string(body, "mode")?.unwrap_or_else(|| "delay".into());
    if !matches!(mode.as_str(), "delay" | "fail") {
        return Err(St3Error::new(
            "invalid-restart-mode",
            format!("invalid restart mode `{mode}`"),
        ));
    }
    Ok(RestartIntensity {
        attempts: attempts as u32,
        interval_ms,
        delay_ms,
        mode,
    })
}

fn restart_type_value(document: &KdlDocument) -> Result<Option<String>, St3Error> {
    let nodes = document
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "restart" && node.children().is_none())
        .collect::<Vec<_>>();
    match nodes.as_slice() {
        [] => Ok(None),
        [node] => one_string(node).map(Some),
        _ => Err(St3Error::new("duplicate-child", "a restart type repeats")),
    }
}

fn validate_agent_body(document: &KdlDocument, owner: &str) -> Result<(), St3Error> {
    const ALLOWED: &[&str] = &[
        "identity",
        "name",
        "description",
        "host",
        "workspace",
        "under",
        "restart",
        "shutdown-timeout",
        "command",
        "argv",
        "env",
        "render",
        "harness",
        "mission-authority",
        "pty",
        "exec",
    ];
    reject_unknown_children(document, ALLOWED, "agent", owner)?;
    for child in [
        "identity",
        "name",
        "description",
        "host",
        "workspace",
        "shutdown-timeout",
        "command",
        "argv",
        "env",
        "render",
        "harness",
        "mission-authority",
    ] {
        unique_child(document, child)?;
    }
    if let Some(authority) = unique_child(document, "mission-authority")? {
        ensure_bare(authority)?;
        let body = authority.children().ok_or_else(|| {
            St3Error::new(
                "empty-mission-authority",
                "mission-authority needs at least one rule",
            )
        })?;
        reject_unknown_children(
            body,
            &["publish", "start", "revise"],
            "mission-authority",
            owner,
        )?;
        let mut rules = BTreeSet::new();
        for rule in body.nodes() {
            ensure_no_properties(rule)?;
            ensure_no_children(rule)?;
            let pattern = one_string(rule)?;
            validate_mission_authority_pattern(&pattern)?;
            if !rules.insert((rule.name().value().to_owned(), pattern.clone())) {
                return Err(St3Error::new(
                    "duplicate-mission-authority",
                    format!(
                        "agent `{owner}` repeats mission authority `{} {pattern}`",
                        rule.name().value()
                    ),
                ));
            }
        }
        if rules.is_empty() {
            return Err(St3Error::new(
                "empty-mission-authority",
                "mission-authority needs at least one rule",
            ));
        }
    }
    for under in document
        .nodes()
        .iter()
        .filter(|child| child.name().value() == "under")
    {
        ensure_only_properties(under, &["reason"])?;
        let target = one_string(under)?;
        let target = target.strip_prefix("agent/").unwrap_or(&target);
        validate_name(target, false)?;
        if let Some(reason) = property_string(under, "reason")?
            && (reason.is_empty() || reason.len() > 500)
        {
            return Err(St3Error::new(
                "invalid-under-reason",
                "an under reason must contain 1 through 500 bytes",
            ));
        }
    }
    validate_restart_forms(document)?;
    if let Some(name) = child_string(document, "name")?
        && name.len() > 160
    {
        return Err(St3Error::new(
            "display-name-too-long",
            "an agent display name cannot exceed 160 bytes",
        ));
    }
    if let Some(description) = child_string(document, "description")?
        && description.len() > 1_000
    {
        return Err(St3Error::new(
            "description-too-long",
            "an agent description cannot exceed 1,000 bytes",
        ));
    }
    parse_workspace(document)?;
    if let Some(env) = unique_child(document, "env")? {
        validate_string_map(env, true)?;
    }
    if let Some(render) = unique_child(document, "render")? {
        validate_render(render)?;
    }
    for driver in document
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "harness")
    {
        validate_driver(driver)?;
    }
    Ok(())
}

fn validate_mission_authority_pattern(pattern: &str) -> Result<(), St3Error> {
    if pattern.starts_with("mission/") || pattern.contains('*') && !pattern.ends_with("/*") {
        return Err(St3Error::new(
            "invalid-mission-authority-pattern",
            "mission authority needs an exact mission ID or a terminal `/*` namespace",
        ));
    }
    let mission = pattern.strip_suffix("/*").unwrap_or(pattern);
    if mission.is_empty() || mission.contains('*') {
        return Err(St3Error::new(
            "invalid-mission-authority-pattern",
            "mission authority needs an exact mission ID or a terminal `/*` namespace",
        ));
    }
    crate::mission::validate_mission_id(mission)
}

pub fn agent_mission_authority(desired: &Value) -> crate::model::MissionAuthority {
    let mut authority = crate::model::MissionAuthority::default();
    let Some(children) = desired.get("children").and_then(Value::as_array) else {
        return authority;
    };
    let Some(block) = children
        .iter()
        .find(|child| child.get("name").and_then(Value::as_str) == Some("mission-authority"))
    else {
        return authority;
    };
    for rule in block
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(action) = rule.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(pattern) = rule
            .get("arguments")
            .and_then(Value::as_array)
            .and_then(|arguments| arguments.first())
            .and_then(Value::as_str)
        else {
            continue;
        };
        match action {
            "publish" => authority.publish.push(pattern.to_owned()),
            "start" => authority.start.push(pattern.to_owned()),
            "revise" => authority.revise.push(pattern.to_owned()),
            _ => {}
        }
    }
    authority
}

fn validate_task_body(
    document: &KdlDocument,
    owner: &str,
    standalone: bool,
) -> Result<(), St3Error> {
    let mut allowed = vec!["id", "command", "argv", "cwd", "tags", "env", "unset"];
    if standalone {
        allowed.extend(["host", "workspace", "restart", "shutdown-timeout", "render"]);
    }
    reject_unknown_children(document, &allowed, "member", owner)?;
    for child in ["id", "command", "argv", "cwd", "tags", "env", "unset"] {
        unique_child(document, child)?;
    }
    if standalone {
        for child in ["host", "workspace", "shutdown-timeout", "render"] {
            unique_child(document, child)?;
        }
        validate_restart_forms(document)?;
        parse_workspace(document)?;
    }
    if let Some(tags) = unique_child(document, "tags")? {
        validate_tags(tags)?;
    }
    if let Some(env) = unique_child(document, "env")? {
        validate_string_map(env, true)?;
    }
    let unset = child_strings(document, "unset")?.unwrap_or_default();
    for name in &unset {
        validate_environment_name(name)?;
    }
    let environment = parse_map_child(document, "env")?;
    if let Some(name) = unset.iter().find(|name| environment.contains_key(*name)) {
        return Err(St3Error::new(
            "environment-set-and-unset",
            format!("member `{owner}` both sets and unsets `{name}`"),
        ));
    }
    if let Some(render) = unique_child(document, "render")? {
        validate_render(render)?;
    }
    Ok(())
}

fn validate_restart_forms(document: &KdlDocument) -> Result<(), St3Error> {
    let nodes = document
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "restart")
        .collect::<Vec<_>>();
    let scalar = nodes
        .iter()
        .filter(|node| node.children().is_none())
        .count();
    let block = nodes
        .iter()
        .filter(|node| node.children().is_some())
        .count();
    if scalar > 1 || block > 1 || nodes.len() > 2 {
        return Err(St3Error::new(
            "duplicate-child",
            "a restart type or intensity block repeats",
        ));
    }
    if let Some(node) = nodes.iter().find(|node| node.children().is_some()) {
        ensure_no_properties(node)?;
        if !positional_values(node).is_empty() {
            return Err(St3Error::new(
                "unexpected-value",
                "a restart intensity block cannot have values",
            ));
        }
        let body = node.children().expect("selected a block");
        reject_unknown_children(
            body,
            &["attempts", "interval", "delay", "mode"],
            "restart",
            "restart",
        )?;
        for child in ["attempts", "interval", "delay", "mode"] {
            unique_child(body, child)?;
        }
    }
    Ok(())
}

fn validate_driver(node: &KdlNode) -> Result<(), St3Error> {
    ensure_no_properties(node)?;
    let provider = one_string_with_children(node)?;
    let body = node.children().ok_or_else(|| {
        St3Error::new(
            "missing-driver-body",
            format!("harness `{provider}` has no body"),
        )
    })?;
    let allowed: &[&str] = match provider.as_str() {
        "claude" => &["model", "effort", "dev-channels", "prompt", "args"],
        "codex" | "pi" | "omp" => &["model", "effort", "prompt", "args"],
        "opencode" => &["model", "prompt", "args"],
        _ => return Err(St3Error::new("unknown-driver", "unknown typed driver")),
    };
    reject_unknown_children(body, allowed, "harness", &provider)?;
    for child in allowed {
        unique_child(body, child)?;
    }
    if provider == "claude"
        && let Some(node) = unique_child(body, "dev-channels")?
        && !one_bool(node)?
    {
        return Err(St3Error::new(
            "native-channel-required",
            "a typed Claude harness always requires the st3 native channel; use `exec` for an unmanaged non-interactive process",
        ));
    }
    Ok(())
}

fn validate_render(node: &KdlNode) -> Result<(), St3Error> {
    ensure_bare(node)?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("empty-render", "a render block cannot be empty"))?;
    for operation in body.nodes() {
        reject_type(operation)?;
        let name = operation.name().value();
        match name {
            "copy" => {
                ensure_only_properties(operation, &["executable"])?;
                require_string_count(operation, 2)?;
                ensure_no_children(operation)?;
            }
            "file" | "json-upsert" => {
                let allowed = if name == "json-upsert" {
                    &["arrays", "executable"][..]
                } else {
                    &["executable"][..]
                };
                ensure_only_properties(operation, allowed)?;
                let args = positional_strings_without_children(operation)?;
                let child_content = operation.children().is_some();
                if !matches!((args.len(), child_content), (2, false) | (1, true)) {
                    return Err(St3Error::new(
                        "invalid-render-operation",
                        format!("render `{name}` needs a destination and exactly one content form"),
                    ));
                }
                if let Some(children) = operation.children() {
                    reject_unknown_children(children, &["content"], "render operation", name)?;
                    required_child_string(children, "content", name)?;
                }
                if name == "json-upsert" {
                    if let Some(arrays) = property_string(operation, "arrays")?
                        && !matches!(arrays.as_str(), "replace" | "union")
                    {
                        return Err(St3Error::new(
                            "invalid-json-array-mode",
                            format!("invalid JSON array mode `{arrays}`"),
                        ));
                    }
                    let source = if args.len() == 2 {
                        args[1].clone()
                    } else {
                        required_child_string(
                            operation.children().expect("child content"),
                            "content",
                            name,
                        )?
                    };
                    let value: Value = serde_json::from_str(&source)
                        .map_err(|error| St3Error::new("invalid-render-json", error.to_string()))?;
                    if !value.is_object() {
                        return Err(St3Error::new(
                            "invalid-render-json",
                            "json-upsert content must be an object",
                        ));
                    }
                }
            }
            "ensure-line" => {
                ensure_only_properties(operation, &["executable"])?;
                require_string_count(operation, 2)?;
                ensure_no_children(operation)?;
            }
            "git-exclude" => {
                ensure_no_properties(operation)?;
                let values = positional_strings(operation)?;
                if values.is_empty() {
                    return Err(St3Error::new(
                        "invalid-git-exclude",
                        "git-exclude needs at least one path",
                    ));
                }
            }
            other => {
                return Err(St3Error::new(
                    "unknown-render-operation",
                    format!("unknown render operation `{other}`"),
                ));
            }
        }
        if let Some(value) = property_bool(operation, "executable")? {
            let _ = value;
        }
    }
    Ok(())
}

fn validate_doc(node: &KdlNode) -> Result<(), St3Error> {
    ensure_no_properties(node)?;
    let name = one_string_with_children(node)?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("missing-doc-body", format!("doc `{name}` needs a hash")))?;
    reject_unknown_children(body, &["hash"], "doc", &name)?;
    let hash = required_child_string(body, "hash", &name)?;
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(St3Error::new(
            "invalid-document-hash",
            format!("doc `{name}` needs a 64-character SHA-256 hash"),
        ));
    }
    Ok(())
}

fn validate_resource(node: &KdlNode) -> Result<(), St3Error> {
    ensure_no_properties(node)?;
    one_string_with_children(node)?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("missing-resource-body", "a resource needs a kind"))?;
    reject_unknown_children(body, &["kind"], "resource", "resource")?;
    let kind = required_child_string(body, "kind", "resource")?;
    st3_schema::registry()
        .validate_resource_kind(&kind)
        .map_err(|error| St3Error::new(error.code, error.message))?;
    Ok(())
}

fn validate_observer(node: &KdlNode) -> Result<(), St3Error> {
    ensure_no_properties(node)?;
    one_string_with_children(node)?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("missing-observer-body", "an observer needs a body"))?;
    if body.nodes().len() == 1 && body.nodes()[0].name().value() == "stop" {
        ensure_bare(&body.nodes()[0])?;
        return Ok(());
    }
    reject_unknown_children(
        body,
        &["resource", "provider", "locator", "field"],
        "observer",
        "observer",
    )?;
    let resource = required_child_string(body, "resource", "observer")?;
    validate_full_subject(&resource)?;
    if !resource.starts_with("resource/") {
        return Err(St3Error::new(
            "invalid-observer-resource",
            "an observer resource must use a `resource/` subject",
        ));
    }
    for required in ["provider", "locator"] {
        required_child_string(body, required, "observer")?;
    }
    let fields = repeated_child_strings(body, "field")?;
    if fields.is_empty() {
        return Err(St3Error::new(
            "missing-observer-field",
            "an observer needs at least one field",
        ));
    }
    if fields.iter().collect::<BTreeSet<_>>().len() != fields.len() {
        return Err(St3Error::new(
            "duplicate-observer-field",
            "an observer field repeats",
        ));
    }
    Ok(())
}

fn validate_subscription(node: &KdlNode) -> Result<(), St3Error> {
    ensure_no_properties(node)?;
    one_string_with_children(node)?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("missing-subscription-body", "a subscription needs a body"))?;
    if body.nodes().len() == 1 && body.nodes()[0].name().value() == "stop" {
        ensure_bare(&body.nodes()[0])?;
        return Ok(());
    }
    reject_unknown_children(
        body,
        &["observer", "to", "on", "when", "delivery"],
        "subscription",
        "subscription",
    )?;
    let observer = required_child_string(body, "observer", "subscription")?;
    validate_full_subject(&observer)?;
    if !observer.starts_with("observer/") {
        return Err(St3Error::new(
            "invalid-subscription-observer",
            "a subscription observer must use an `observer/` subject",
        ));
    }
    unique_child(body, "delivery")?;
    let fields = repeated_child_strings(body, "on")?;
    if fields.is_empty() {
        return Err(St3Error::new(
            "missing-subscription-field",
            "a subscription needs at least one `on` field",
        ));
    }
    if fields.iter().collect::<BTreeSet<_>>().len() != fields.len() {
        return Err(St3Error::new(
            "duplicate-subscription-field",
            "a subscription field repeats",
        ));
    }
    if let Some(condition) = unique_child(body, "when")? {
        parse_subscription_condition(condition)?;
    }
    let delivery = unique_child(body, "delivery")?.ok_or_else(|| {
        St3Error::new(
            "missing-subscription-delivery",
            "a subscription needs delivery",
        )
    })?;
    let kind = first_string(delivery)?;
    match kind.as_str() {
        "message" => {
            ensure_no_children(delivery)?;
            let target = required_child_string(body, "to", "subscription")?;
            validate_full_subject(&target)?;
        }
        "mission" => {
            if child_string(body, "to")?.is_some() {
                return Err(St3Error::new(
                    "invalid-subscription-target",
                    "a mission delivery cannot contain `to`",
                ));
            }
            let delivery_body = delivery.children().ok_or_else(|| {
                St3Error::new(
                    "missing-mission-delivery",
                    "a mission delivery needs a body",
                )
            })?;
            reject_unknown_children(
                delivery_body,
                &["mission", "resource", "workspace", "requester"],
                "mission delivery",
                "delivery",
            )?;
            for name in ["mission", "resource", "workspace", "requester"] {
                unique_child(delivery_body, name)?;
            }
            let reference = required_child_string(delivery_body, "mission", "mission delivery")?;
            validate_exact_mission_reference(&reference)?;
            let input = required_child_string(delivery_body, "resource", "mission delivery")?;
            validate_name(&input, false)?;
            let workspace = required_child_string(delivery_body, "workspace", "mission delivery")?;
            if workspace.trim().is_empty() {
                return Err(St3Error::new(
                    "invalid-mission-delivery-workspace",
                    "a mission delivery workspace cannot be empty",
                ));
            }
            if let Some(requester) = child_string(delivery_body, "requester")? {
                validate_full_subject(&requester)?;
                if !requester.starts_with("agent/") && !requester.starts_with("person/") {
                    return Err(St3Error::new(
                        "invalid-mission-delivery-requester",
                        "a mission delivery requester must use an `agent/` or `person/` subject",
                    ));
                }
            }
        }
        _ => {
            return Err(St3Error::new(
                "unsupported-subscription-delivery",
                "a subscription delivery must be `message` or `mission`",
            ));
        }
    }
    Ok(())
}

fn parse_subscription_condition(node: &KdlNode) -> Result<SubscriptionConditionSpec, St3Error> {
    ensure_bare(node)?;
    let body = node.children().ok_or_else(|| {
        St3Error::new(
            "missing-subscription-condition",
            "subscription `when` needs one predicate",
        )
    })?;
    let [predicate] = body.nodes() else {
        return Err(St3Error::new(
            "invalid-subscription-condition",
            "subscription `when` needs exactly one predicate",
        ));
    };
    reject_type(predicate)?;
    ensure_no_properties(predicate)?;
    let entries = positional_values(predicate);
    match predicate.name().value() {
        "field" => {
            ensure_no_children(predicate)?;
            if entries.len() != 3 {
                return Err(St3Error::new(
                    "invalid-subscription-condition",
                    "a subscription field condition requires path, operator, and value",
                ));
            }
            let path = value_string(entries[0])?;
            validate_field_path(&path)?;
            let operator = predicate_operator(entries[1])?;
            Ok(SubscriptionConditionSpec::Field {
                path,
                operator,
                value: json_value(entries[2])?,
            })
        }
        "every" | "not-every" => {
            if entries.len() != 1 {
                return Err(St3Error::new(
                    "invalid-subscription-condition",
                    "a quantified subscription condition requires one list field path",
                ));
            }
            let path = value_string(entries[0])?;
            validate_field_path(&path)?;
            let body = predicate.children().ok_or_else(|| {
                St3Error::new(
                    "empty-subscription-condition",
                    "a quantified subscription condition needs at least one field predicate",
                )
            })?;
            if body.nodes().is_empty() {
                return Err(St3Error::new(
                    "empty-subscription-condition",
                    "a quantified subscription condition needs at least one field predicate",
                ));
            }
            let mut fields = Vec::with_capacity(body.nodes().len());
            for field in body.nodes() {
                if field.name().value() != "field" {
                    return Err(St3Error::new(
                        "invalid-subscription-condition",
                        "every and not-every accept only field predicates",
                    ));
                }
                reject_type(field)?;
                ensure_no_properties(field)?;
                ensure_no_children(field)?;
                let entries = positional_values(field);
                if entries.len() != 3 {
                    return Err(St3Error::new(
                        "invalid-subscription-condition",
                        "a quantified field requires path, operator, and value",
                    ));
                }
                let path = value_string(entries[0])?;
                validate_field_path(&path)?;
                fields.push(QuantifiedFieldSpec {
                    path,
                    operator: predicate_operator(entries[1])?,
                    value: json_value(entries[2])?,
                });
            }
            if predicate.name().value() == "every" {
                Ok(SubscriptionConditionSpec::Every { path, fields })
            } else {
                Ok(SubscriptionConditionSpec::NotEvery { path, fields })
            }
        }
        other => Err(St3Error::new(
            "unknown-subscription-condition",
            format!("unknown subscription condition `{other}`"),
        )),
    }
}

fn validate_field_path(path: &str) -> Result<(), St3Error> {
    if valid_field_path(path) {
        Ok(())
    } else {
        Err(St3Error::new(
            "invalid-field-path",
            format!("invalid field path `{path}`"),
        ))
    }
}

fn predicate_operator(value: &KdlValue) -> Result<String, St3Error> {
    let operator = value_string(value)?;
    if matches!(operator.as_str(), "is" | "starts-with" | "contains") {
        Ok(operator)
    } else {
        Err(St3Error::new(
            "invalid-field-operator",
            format!("invalid field operator `{operator}`"),
        ))
    }
}

fn validate_message(node: &KdlNode) -> Result<(), St3Error> {
    ensure_no_properties(node)?;
    one_string_with_children(node)?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("missing-message-body", "a message needs a body"))?;
    reject_unknown_children(
        body,
        &["from", "to", "content", "title", "in-reply-to", "tag"],
        "message",
        "message",
    )?;
    child_string(body, "from")?;
    required_child_string(body, "to", "message")?;
    child_string(body, "title")?;
    if let Some(parent) = child_string(body, "in-reply-to")?
        && !parent.starts_with("message/")
    {
        return Err(St3Error::new(
            "invalid-message-reference",
            "a message reply must use a full `message/ID` subject",
        ));
    }
    let tags = repeated_child_strings(body, "tag")?;
    if tags.iter().collect::<BTreeSet<_>>().len() != tags.len() {
        return Err(St3Error::new(
            "duplicate-message-tag",
            "a message tag repeats",
        ));
    }
    let content = required_child_string(body, "content", "message")?;
    if content.trim().is_empty() {
        return Err(St3Error::new(
            "empty-message",
            "a message needs nonempty content",
        ));
    }
    if content.len() > 4_096 && !content.starts_with("doc/") {
        return Err(St3Error::new(
            "message-too-large",
            "an inline message cannot exceed 4 KiB",
        ));
    }
    Ok(())
}

fn validate_schedule(node: &KdlNode) -> Result<(), St3Error> {
    ensure_no_properties(node)?;
    one_string_with_children(node)?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("missing-schedule-body", "a schedule needs a body"))?;
    if body.nodes().len() == 1 && body.nodes()[0].name().value() == "stop" {
        ensure_bare(&body.nodes()[0])?;
        ensure_no_children(&body.nodes()[0])?;
        return Ok(());
    }
    reject_unknown_children(
        body,
        &[
            "host",
            "at",
            "every",
            "anchor",
            "catch-up",
            "max-catch-up",
            "work",
        ],
        "schedule",
        "schedule",
    )?;
    for child in [
        "host",
        "at",
        "every",
        "anchor",
        "catch-up",
        "max-catch-up",
        "work",
    ] {
        unique_child(body, child)?;
    }
    let at = child_string(body, "at")?;
    let every = child_string(body, "every")?;
    if at.is_some() == every.is_some() {
        return Err(St3Error::new(
            "invalid-schedule-time",
            "a schedule needs exactly one of `at` and `every`",
        ));
    }
    let anchor = child_string(body, "anchor")?;
    if at.is_some() && anchor.is_some() || every.is_some() && anchor.is_none() {
        return Err(St3Error::new(
            "invalid-schedule-anchor",
            "an interval schedule needs an anchor and a one-time schedule cannot have one",
        ));
    }
    if let Some(at) = at.as_deref().or(anchor.as_deref()) {
        parse_utc_time(at)?;
    }
    if let Some(every) = every {
        parse_duration(&every, true)?;
    }
    let catch_up = child_string(body, "catch-up")?;
    let max = child_integer(body, "max-catch-up")?;
    if at.is_some() && (catch_up.is_some() || max.is_some()) {
        return Err(St3Error::new(
            "invalid-schedule-catch-up",
            "a one-time schedule cannot have catch-up controls",
        ));
    }
    if let Some(policy) = catch_up.as_deref() {
        if !matches!(policy, "all" | "latest" | "skip") {
            return Err(St3Error::new(
                "invalid-schedule-catch-up",
                format!("invalid catch-up policy `{policy}`"),
            ));
        }
        if (policy == "all") != max.is_some() {
            return Err(St3Error::new(
                "invalid-schedule-catch-up",
                "catch-up `all` requires max-catch-up, and other policies forbid it",
            ));
        }
    } else if max.is_some() {
        return Err(St3Error::new(
            "invalid-schedule-catch-up",
            "max-catch-up requires catch-up `all`",
        ));
    }
    if let Some(max) = max
        && (max <= 0 || max > u32::MAX as i128)
    {
        return Err(St3Error::new(
            "invalid-schedule-catch-up",
            "max-catch-up must be a positive u32",
        ));
    }
    let work = unique_child(body, "work")?.ok_or_else(|| {
        St3Error::new("missing-schedule-work", "a schedule needs a work template")
    })?;
    ensure_bare(work)?;
    let work_body = work
        .children()
        .ok_or_else(|| St3Error::new("missing-schedule-work", "schedule work is empty"))?;
    reject_unknown_children(
        work_body,
        &["mission", "workspace", "input"],
        "schedule work",
        "work",
    )?;
    unique_child(work_body, "mission")?;
    unique_child(work_body, "workspace")?;
    let reference = required_child_string(work_body, "mission", "schedule work")?;
    validate_exact_mission_reference(&reference)?;
    let workspace = required_child_string(work_body, "workspace", "schedule work")?;
    if workspace.trim().is_empty() {
        return Err(St3Error::new(
            "invalid-schedule-workspace",
            "schedule work needs a workspace",
        ));
    }
    let mut inputs = BTreeSet::new();
    for input in work_body
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "input")
    {
        ensure_no_properties(input)?;
        ensure_no_children(input)?;
        let values = positional_strings(input)?;
        if values.len() != 2 {
            return Err(St3Error::new(
                "invalid-schedule-input",
                "a schedule input needs a name and value",
            ));
        }
        validate_name(&values[0], false)?;
        if !inputs.insert(values[0].clone()) {
            return Err(St3Error::new(
                "duplicate-schedule-input",
                "a schedule input repeats",
            ));
        }
    }
    Ok(())
}

fn validate_exact_mission_reference(reference: &str) -> Result<(), St3Error> {
    let reference = reference.strip_prefix("mission/").unwrap_or(reference);
    let (mission, revision) = reference.rsplit_once('@').ok_or_else(|| {
        St3Error::new(
            "unpinned-mission-reference",
            "a mission delivery needs an exact MISSION@REVISION reference",
        )
    })?;
    crate::mission::validate_mission_id(mission)?;
    if revision.len() != 64 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(St3Error::new(
            "invalid-mission-revision",
            "a mission revision must be a 64-character hexadecimal hash",
        ));
    }
    Ok(())
}

fn reject_unknown_children(
    document: &KdlDocument,
    allowed: &[&str],
    kind: &str,
    owner: &str,
) -> Result<(), St3Error> {
    for child in document.nodes() {
        if !allowed.contains(&child.name().value()) {
            return Err(St3Error::new(
                "unknown-child",
                format!(
                    "{kind} `{owner}` does not accept child `{}`",
                    child.name().value()
                ),
            ));
        }
    }
    Ok(())
}

fn validate_string_map(node: &KdlNode, environment: bool) -> Result<(), St3Error> {
    ensure_bare(node)?;
    let Some(body) = node.children() else {
        return Ok(());
    };
    let mut names = HashSet::new();
    for child in body.nodes() {
        let name = child.name().value();
        if !names.insert(name) {
            return Err(St3Error::new(
                "duplicate-map-key",
                format!("map key `{name}` repeats"),
            ));
        }
        if environment {
            validate_environment_name(name)?;
            if crate::mission::is_reserved_context_name(name) {
                return Err(St3Error::new(
                    "reserved-context-variable",
                    format!("environment cannot override `{name}`"),
                ));
            }
        }
        one_string(child)?;
    }
    Ok(())
}

pub(crate) fn validate_deferred_declaration(node: &KdlNode) -> Result<(), St3Error> {
    if node.name().value() == "account" {
        return Err(St3Error::new(
            "account-inside-mission",
            "an account declaration must be at the root",
        ));
    }
    if node.name().value() == "env" {
        validate_string_map(node, true)?;
    }
    if let Some(children) = node.children() {
        for child in children.nodes() {
            validate_deferred_declaration(child)?;
        }
    }
    Ok(())
}

fn validate_tags(node: &KdlNode) -> Result<(), St3Error> {
    reject_type(node)?;
    ensure_no_children(node)?;
    if node.entries().iter().any(|entry| entry.name().is_none()) {
        return Err(St3Error::new(
            "invalid-tags",
            "a tags entry needs a property name",
        ));
    }
    let mut names = HashSet::new();
    for entry in node.entries() {
        let name = entry.name().expect("checked").value();
        if !names.insert(name) {
            return Err(St3Error::new(
                "duplicate-tag",
                format!("tag `{name}` repeats"),
            ));
        }
        value_string(entry.value())?;
    }
    Ok(())
}

fn validate_environment_name(name: &str) -> Result<(), St3Error> {
    let valid = name
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid {
        return Err(St3Error::new(
            "invalid-environment-name",
            format!("invalid environment name `{name}`"),
        ));
    }
    Ok(())
}

fn parse_utc_time(value: &str) -> Result<i64, St3Error> {
    if !value.ends_with('Z') {
        return Err(St3Error::new(
            "invalid-utc-time",
            "an absolute time must use the UTC `Z` offset",
        ));
    }
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| time.timestamp_millis())
        .map_err(|error| St3Error::new("invalid-utc-time", error.to_string()))
}

pub fn schedule_spec(value: &Value, default_host: &str) -> Option<ScheduleSpec> {
    let children = value.get("children")?.as_array()?;
    if children.len() == 1 && children[0].get("name").and_then(Value::as_str) == Some("stop") {
        return Some(ScheduleSpec {
            stopped: true,
            host: default_host.into(),
            at_unix_ms: None,
            every_ms: None,
            anchor_unix_ms: None,
            catch_up: "latest".into(),
            max_catch_up: None,
            work: None,
        });
    }
    let host = canonical_child_value(value, "host")
        .and_then(Value::as_str)
        .unwrap_or(default_host)
        .to_owned();
    let host = placement_host(host, default_host);
    let at_unix_ms = canonical_child_value(value, "at")
        .and_then(Value::as_str)
        .and_then(|value| parse_utc_time(value).ok());
    let every_ms = canonical_child_value(value, "every")
        .and_then(Value::as_str)
        .and_then(|value| parse_duration(value, true).ok());
    let anchor_unix_ms = canonical_child_value(value, "anchor")
        .and_then(Value::as_str)
        .and_then(|value| parse_utc_time(value).ok());
    let catch_up = canonical_child_value(value, "catch-up")
        .and_then(Value::as_str)
        .unwrap_or("latest")
        .to_owned();
    let max_catch_up = canonical_child_value(value, "max-catch-up")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    let work_node = children
        .iter()
        .find(|child| child.get("name").and_then(Value::as_str) == Some("work"))?;
    let reference = canonical_child_value(work_node, "mission")?.as_str()?;
    let reference = reference.strip_prefix("mission/").unwrap_or(reference);
    let (mission, revision) = reference.rsplit_once('@')?;
    let mut inputs = BTreeMap::new();
    for input in work_node
        .get("children")?
        .as_array()?
        .iter()
        .filter(|child| child.get("name").and_then(Value::as_str) == Some("input"))
    {
        let values = input.get("arguments")?.as_array()?;
        inputs.insert(
            values.first()?.as_str()?.to_owned(),
            values.get(1)?.as_str()?.to_owned(),
        );
    }
    Some(ScheduleSpec {
        stopped: false,
        host,
        at_unix_ms,
        every_ms,
        anchor_unix_ms,
        catch_up,
        max_catch_up,
        work: Some(crate::model::ScheduledWork {
            mission: mission.to_owned(),
            revision: revision.to_owned(),
            workspace: canonical_child_value(work_node, "workspace")?
                .as_str()?
                .to_owned(),
            inputs,
        }),
    })
}

pub fn observer_spec(value: &Value) -> Option<ObserverSpec> {
    let children = value.get("children")?.as_array()?;
    if children.len() == 1 && children[0].get("name").and_then(Value::as_str) == Some("stop") {
        return Some(ObserverSpec {
            resource: String::new(),
            provider: String::new(),
            locator: String::new(),
            fields: Vec::new(),
            stopped: true,
        });
    }
    Some(ObserverSpec {
        resource: canonical_child_value(value, "resource")?
            .as_str()?
            .to_owned(),
        provider: canonical_child_value(value, "provider")?
            .as_str()?
            .to_owned(),
        locator: canonical_child_value(value, "locator")?
            .as_str()?
            .to_owned(),
        fields: canonical_child_values(value, "field"),
        stopped: false,
    })
}

pub fn subscription_spec(value: &Value) -> Option<SubscriptionSpec> {
    let children = value.get("children")?.as_array()?;
    if children.len() == 1 && children[0].get("name").and_then(Value::as_str) == Some("stop") {
        return Some(SubscriptionSpec {
            observer: String::new(),
            to: String::new(),
            fields: Vec::new(),
            condition: None,
            delivery: String::new(),
            mission: None,
            revision: None,
            resource_input: None,
            workspace: None,
            requester: None,
            stopped: true,
        });
    }
    let delivery_node = children
        .iter()
        .find(|child| child.get("name").and_then(Value::as_str) == Some("delivery"))?;
    let delivery = delivery_node
        .get("arguments")?
        .as_array()?
        .first()?
        .as_str()?
        .to_owned();
    let mission_reference = canonical_child_value(delivery_node, "mission").and_then(Value::as_str);
    let (mission, revision) = mission_reference
        .and_then(|value| {
            value
                .strip_prefix("mission/")
                .unwrap_or(value)
                .rsplit_once('@')
        })
        .map_or((None, None), |(mission, revision)| {
            (Some(mission.to_owned()), Some(revision.to_owned()))
        });
    Some(SubscriptionSpec {
        observer: canonical_child_value(value, "observer")?
            .as_str()?
            .to_owned(),
        to: canonical_child_value(value, "to")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        fields: canonical_child_values(value, "on"),
        condition: canonical_subscription_condition(value),
        delivery,
        mission,
        revision,
        resource_input: canonical_child_value(delivery_node, "resource")
            .and_then(Value::as_str)
            .map(str::to_owned),
        workspace: canonical_child_value(delivery_node, "workspace")
            .and_then(Value::as_str)
            .map(str::to_owned),
        requester: canonical_child_value(delivery_node, "requester")
            .and_then(Value::as_str)
            .map(str::to_owned),
        stopped: false,
    })
}

fn canonical_subscription_condition(value: &Value) -> Option<SubscriptionConditionSpec> {
    let when = value
        .get("children")?
        .as_array()?
        .iter()
        .find(|child| child.get("name").and_then(Value::as_str) == Some("when"))?;
    let predicates = when.get("children")?.as_array()?;
    let [predicate] = predicates.as_slice() else {
        return None;
    };
    let name = predicate.get("name")?.as_str()?;
    let arguments = predicate.get("arguments")?.as_array()?;
    match name {
        "field" => {
            let [path, operator, value] = arguments.as_slice() else {
                return None;
            };
            Some(SubscriptionConditionSpec::Field {
                path: path.as_str()?.to_owned(),
                operator: operator.as_str()?.to_owned(),
                value: value.clone(),
            })
        }
        "every" | "not-every" => {
            let [path] = arguments.as_slice() else {
                return None;
            };
            let fields = predicate
                .get("children")?
                .as_array()?
                .iter()
                .map(|field| {
                    if field.get("name").and_then(Value::as_str) != Some("field") {
                        return None;
                    }
                    let [path, operator, value] = field.get("arguments")?.as_array()?.as_slice()
                    else {
                        return None;
                    };
                    Some(QuantifiedFieldSpec {
                        path: path.as_str()?.to_owned(),
                        operator: operator.as_str()?.to_owned(),
                        value: value.clone(),
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            if name == "every" {
                Some(SubscriptionConditionSpec::Every {
                    path: path.as_str()?.to_owned(),
                    fields,
                })
            } else {
                Some(SubscriptionConditionSpec::NotEvery {
                    path: path.as_str()?.to_owned(),
                    fields,
                })
            }
        }
        _ => None,
    }
}

pub fn agent_under(value: &Value) -> Vec<crate::model::UnderSpec> {
    let Some(children) = value.get("children").and_then(Value::as_array) else {
        return Vec::new();
    };
    children
        .iter()
        .filter(|child| child.get("name").and_then(Value::as_str) == Some("under"))
        .filter_map(|under| {
            let target = under.get("arguments")?.as_array()?.first()?.as_str()?;
            let agent = if target.starts_with("agent/") {
                target.to_owned()
            } else {
                format!("agent/{target}")
            };
            let reason = under
                .pointer("/properties/reason")
                .and_then(Value::as_str)
                .map(str::to_owned);
            Some(crate::model::UnderSpec { agent, reason })
        })
        .collect()
}

fn normalize_agent_under(value: &mut Value, default_host: &str, owner_run: Option<&str>) {
    let Some(children) = value.get_mut("children").and_then(Value::as_array_mut) else {
        return;
    };
    for child in children
        .iter_mut()
        .filter(|child| child.get("name").and_then(Value::as_str) == Some("under"))
    {
        let Some(target) = child
            .get_mut("arguments")
            .and_then(Value::as_array_mut)
            .and_then(|arguments| arguments.first_mut())
        else {
            continue;
        };
        let Some(name) = target.as_str() else {
            continue;
        };
        let name = name.strip_prefix("agent/").unwrap_or(name);
        let identity = if name.contains('/') {
            name.to_owned()
        } else if let Some(run) = owner_run {
            format!("{}/{name}", owner_run_id(run))
        } else if name.contains('.') {
            name.to_owned()
        } else {
            format!("{default_host}.{name}")
        };
        *target = Value::String(format!("agent/{identity}"));
    }
}

fn canonical_child_value<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value
        .get("children")?
        .as_array()?
        .iter()
        .find(|child| child.get("name").and_then(Value::as_str) == Some(name))?
        .get("arguments")?
        .as_array()?
        .first()
}

fn canonical_child_values(value: &Value, name: &str) -> Vec<String> {
    value
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|child| child.get("name").and_then(Value::as_str) == Some(name))
        .filter_map(|child| {
            child
                .get("arguments")
                .and_then(Value::as_array)
                .and_then(|arguments| arguments.first())
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

fn one_bool(node: &KdlNode) -> Result<bool, St3Error> {
    ensure_no_properties(node)?;
    ensure_no_children(node)?;
    let [entry] = node.entries() else {
        return Err(St3Error::new(
            "wrong-argument-count",
            format!("node `{}` needs one Boolean", node.name().value()),
        ));
    };
    match entry.value() {
        KdlValue::Bool(value) => Ok(*value),
        _ => Err(St3Error::new(
            "expected-boolean",
            format!("node `{}` needs one Boolean", node.name().value()),
        )),
    }
}

fn property_bool(node: &KdlNode, property: &str) -> Result<Option<bool>, St3Error> {
    node.entries()
        .iter()
        .find(|entry| entry.name().is_some_and(|name| name.value() == property))
        .map(|entry| match entry.value() {
            KdlValue::Bool(value) => Ok(*value),
            _ => Err(St3Error::new(
                "expected-boolean",
                format!("property `{property}` needs a Boolean"),
            )),
        })
        .transpose()
}

fn require_string_count(node: &KdlNode, count: usize) -> Result<Vec<String>, St3Error> {
    let values = positional_strings_without_children(node)?;
    if values.len() != count {
        return Err(St3Error::new(
            "wrong-argument-count",
            format!("node `{}` needs {count} string values", node.name().value()),
        ));
    }
    Ok(values)
}

fn positional_strings_without_children(node: &KdlNode) -> Result<Vec<String>, St3Error> {
    positional_values(node)
        .into_iter()
        .map(value_string)
        .collect()
}

fn parse_tags(document: &KdlDocument) -> Result<BTreeMap<String, String>, St3Error> {
    let Some(node) = unique_child(document, "tags")? else {
        return Ok(BTreeMap::new());
    };
    let mut tags = BTreeMap::new();
    for entry in node.entries() {
        let name = entry.name().ok_or_else(|| {
            St3Error::new("invalid-tags", "a tags entry must have a property name")
        })?;
        let value = value_string(entry.value())?;
        if tags.insert(name.value().into(), value).is_some() {
            return Err(St3Error::new(
                "duplicate-tag",
                format!("tag `{}` repeats", name.value()),
            ));
        }
    }
    Ok(tags)
}

fn collect_document_refs(node: &KdlNode, output: &mut BTreeSet<String>) -> Result<(), St3Error> {
    for entry in node.entries() {
        if let KdlValue::String(value) = entry.value()
            && value.starts_with("doc/")
        {
            validate_document_ref(value)?;
            output.insert(value.clone());
        }
    }
    if let Some(children) = node.children() {
        for child in children.nodes() {
            collect_document_refs(child, output)?;
        }
    }
    Ok(())
}

fn validate_document_ref(value: &str) -> Result<(), St3Error> {
    let (name, hash) = value.rsplit_once('@').unwrap_or((value, ""));
    if name.len() <= 4
        || name.contains("..")
        || name.ends_with('/')
        || (!hash.is_empty()
            && (hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())))
    {
        return Err(St3Error::new(
            "invalid-document-reference",
            format!("invalid document reference `{value}`"),
        ));
    }
    Ok(())
}

fn insert_subject(context: &mut ParseContext, subject: DesiredSubject) -> Result<(), St3Error> {
    let name = subject.subject.clone();
    if context.subjects.insert(name.clone(), subject).is_some() {
        return Err(St3Error::new(
            "duplicate-subject",
            format!("one publish declares `{name}` more than once"),
        ));
    }
    Ok(())
}

fn canonical_node(node: &KdlNode) -> Result<Value, St3Error> {
    let mut properties = BTreeMap::<String, Value>::new();
    let mut arguments = Vec::new();
    for entry in node.entries() {
        let value = json_value(entry.value())?;
        if let Some(name) = entry.name() {
            if properties.insert(name.value().into(), value).is_some() {
                return Err(St3Error::new(
                    "duplicate-property",
                    format!(
                        "node `{}` repeats property `{}`",
                        node.name().value(),
                        name.value()
                    ),
                ));
            }
        } else {
            arguments.push(value);
        }
    }
    let children = node
        .children()
        .map(|document| {
            document
                .nodes()
                .iter()
                .map(canonical_node)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    let mut output = Map::new();
    output.insert("name".into(), Value::String(node.name().value().into()));
    if !arguments.is_empty() {
        output.insert("arguments".into(), Value::Array(arguments));
    }
    if !properties.is_empty() {
        output.insert(
            "properties".into(),
            serde_json::to_value(properties).expect("BTreeMap serializes"),
        );
    }
    if !children.is_empty() {
        output.insert("children".into(), Value::Array(children));
    }
    Ok(Value::Object(output))
}

fn hash_json(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).expect("normalized JSON serializes");
    hex::encode(Sha256::digest(bytes))
}

fn json_value(value: &KdlValue) -> Result<Value, St3Error> {
    Ok(match value {
        KdlValue::String(value) => Value::String(value.clone()),
        KdlValue::Integer(value) => {
            let integer = i64::try_from(*value).map_err(|_| {
                St3Error::new(
                    "integer-range",
                    "a KDL integer must fit a signed 64-bit value",
                )
            })?;
            Value::Number(integer.into())
        }
        KdlValue::Float(value) => Value::Number(
            serde_json::Number::from_f64(*value)
                .ok_or_else(|| St3Error::new("invalid-number", "a float must be finite"))?,
        ),
        KdlValue::Bool(value) => Value::Bool(*value),
        KdlValue::Null => Value::Null,
    })
}

fn reject_type(node: &KdlNode) -> Result<(), St3Error> {
    if node.ty().is_some() {
        Err(St3Error::new(
            "unexpected-type",
            format!(
                "node `{}` cannot have a type annotation",
                node.name().value()
            ),
        ))
    } else {
        Ok(())
    }
}

fn ensure_bare(node: &KdlNode) -> Result<(), St3Error> {
    reject_type(node)?;
    if !node.entries().is_empty() {
        return Err(St3Error::new(
            "unexpected-value",
            format!("node `{}` cannot have values", node.name().value()),
        ));
    }
    Ok(())
}

fn ensure_no_children(node: &KdlNode) -> Result<(), St3Error> {
    if node.children().is_some() {
        Err(St3Error::new(
            "unexpected-children",
            format!("node `{}` cannot have children", node.name().value()),
        ))
    } else {
        Ok(())
    }
}

fn ensure_no_properties(node: &KdlNode) -> Result<(), St3Error> {
    ensure_only_properties(node, &[])
}

fn ensure_only_properties(node: &KdlNode, allowed: &[&str]) -> Result<(), St3Error> {
    reject_type(node)?;
    let mut seen = HashSet::new();
    for entry in node.entries().iter().filter(|entry| entry.name().is_some()) {
        let name = entry.name().expect("filtered").value();
        if !allowed.contains(&name) {
            return Err(St3Error::new(
                "unknown-property",
                format!(
                    "node `{}` does not accept property `{name}`",
                    node.name().value()
                ),
            ));
        }
        if !seen.insert(name) {
            return Err(St3Error::new(
                "duplicate-property",
                format!("node `{}` repeats property `{name}`", node.name().value()),
            ));
        }
    }
    Ok(())
}

fn one_string(node: &KdlNode) -> Result<String, St3Error> {
    ensure_no_children(node)?;
    one_string_with_children(node)
}

fn one_string_with_children(node: &KdlNode) -> Result<String, St3Error> {
    first_string(node).and_then(|first| {
        if positional_values(node).len() == 1 {
            Ok(first)
        } else {
            Err(St3Error::new(
                "wrong-argument-count",
                format!(
                    "node `{}` needs one string; received `{}`",
                    node.name().value(),
                    node
                ),
            ))
        }
    })
}

fn first_string(node: &KdlNode) -> Result<String, St3Error> {
    positional_values(node)
        .first()
        .ok_or_else(|| {
            St3Error::new(
                "missing-argument",
                format!("node `{}` needs a name", node.name().value()),
            )
        })
        .and_then(|value| value_string(value))
}

fn positional_values(node: &KdlNode) -> Vec<&KdlValue> {
    node.entries()
        .iter()
        .filter(|entry| entry.name().is_none())
        .map(|entry| entry.value())
        .collect()
}

fn positional_strings(node: &KdlNode) -> Result<Vec<String>, St3Error> {
    ensure_no_properties(node)?;
    ensure_no_children(node)?;
    positional_values(node)
        .into_iter()
        .map(value_string)
        .collect()
}

fn value_string(value: &KdlValue) -> Result<String, St3Error> {
    match value {
        KdlValue::String(value) if !value.is_empty() => Ok(value.clone()),
        _ => Err(St3Error::new(
            "expected-string",
            "this value must be a non-empty string",
        )),
    }
}

fn property_string(node: &KdlNode, property: &str) -> Result<Option<String>, St3Error> {
    node.entries()
        .iter()
        .find(|entry| entry.name().is_some_and(|name| name.value() == property))
        .map(|entry| value_string(entry.value()))
        .transpose()
}

fn unique_child<'a>(
    document: &'a KdlDocument,
    name: &str,
) -> Result<Option<&'a KdlNode>, St3Error> {
    let matches = document
        .nodes()
        .iter()
        .filter(|node| node.name().value() == name)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [node] => Ok(Some(*node)),
        _ => Err(St3Error::new(
            "duplicate-child",
            format!("child `{name}` repeats"),
        )),
    }
}

fn child_string(document: &KdlDocument, name: &str) -> Result<Option<String>, St3Error> {
    unique_child(document, name)?.map(one_string).transpose()
}

fn parse_workspace(document: &KdlDocument) -> Result<Option<(String, bool)>, St3Error> {
    let Some(node) = unique_child(document, "workspace")? else {
        return Ok(None);
    };
    ensure_only_properties(node, &["create"])?;
    let workspace = one_string(node)?;
    let create = property_bool(node, "create")?.unwrap_or(false);
    Ok(Some((workspace, create)))
}

fn required_child_string(
    document: &KdlDocument,
    child: &str,
    owner: &str,
) -> Result<String, St3Error> {
    child_string(document, child)?
        .ok_or_else(|| St3Error::new("missing-child", format!("`{owner}` needs child `{child}`")))
}

fn child_strings(document: &KdlDocument, name: &str) -> Result<Option<Vec<String>>, St3Error> {
    unique_child(document, name)?
        .map(positional_strings)
        .transpose()
}

fn repeated_child_strings(document: &KdlDocument, name: &str) -> Result<Vec<String>, St3Error> {
    document
        .nodes()
        .iter()
        .filter(|node| node.name().value() == name)
        .map(one_string)
        .collect()
}

fn child_integer(document: &KdlDocument, name: &str) -> Result<Option<i128>, St3Error> {
    let Some(node) = unique_child(document, name)? else {
        return Ok(None);
    };
    ensure_no_properties(node)?;
    ensure_no_children(node)?;
    let [entry] = node.entries() else {
        return Err(St3Error::new(
            "wrong-argument-count",
            format!("child `{name}` needs one integer"),
        ));
    };
    match entry.value() {
        KdlValue::Integer(value) => Ok(Some(*value)),
        _ => Err(St3Error::new(
            "expected-integer",
            format!("child `{name}` needs one integer"),
        )),
    }
}

fn parse_map_child(
    document: &KdlDocument,
    name: &str,
) -> Result<BTreeMap<String, String>, St3Error> {
    let Some(node) = unique_child(document, name)? else {
        return Ok(BTreeMap::new());
    };
    ensure_bare(node)?;
    let Some(children) = node.children() else {
        return Ok(BTreeMap::new());
    };
    let mut output = BTreeMap::new();
    for child in children.nodes() {
        let value = one_string(child)?;
        let key = child.name().value().to_owned();
        if output.insert(key.clone(), value).is_some() {
            return Err(St3Error::new(
                "duplicate-map-key",
                format!("map key `{key}` repeats"),
            ));
        }
    }
    Ok(output)
}

fn has_block_child(document: &KdlDocument, name: &str) -> bool {
    document
        .nodes()
        .iter()
        .any(|node| node.name().value() == name && node.children().is_some())
}

fn one_duration(node: &KdlNode) -> Result<u64, St3Error> {
    ensure_no_properties(node)?;
    ensure_no_children(node)?;
    let [entry] = node.entries() else {
        return Err(St3Error::new(
            "wrong-argument-count",
            format!("node `{}` needs one duration", node.name().value()),
        ));
    };
    match entry.value() {
        KdlValue::String(value) => parse_duration(value, true),
        KdlValue::Integer(value) if *value > 0 => u64::try_from(*value)
            .map(|value| value * 1_000)
            .map_err(|_| St3Error::new("duration-range", "duration is too large")),
        _ => Err(St3Error::new(
            "invalid-duration",
            "a required duration must be positive",
        )),
    }
}

pub fn parse_duration(value: &str, positive: bool) -> Result<u64, St3Error> {
    let (number, multiplier) = if let Some(value) = value.strip_suffix("ms") {
        (value, 1)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1_000)
    } else if let Some(value) = value.strip_suffix('m') {
        (value, 60_000)
    } else if let Some(value) = value.strip_suffix('h') {
        (value, 3_600_000)
    } else if let Some(value) = value.strip_suffix('d') {
        (value, 86_400_000)
    } else {
        (value, 1_000)
    };
    let number = number
        .parse::<u64>()
        .map_err(|_| St3Error::new("invalid-duration", format!("invalid duration `{value}`")))?;
    let duration = number
        .checked_mul(multiplier)
        .ok_or_else(|| St3Error::new("duration-range", "duration is too large"))?;
    if positive && duration == 0 {
        return Err(St3Error::new(
            "invalid-duration",
            "a required duration must be positive",
        ));
    }
    Ok(duration)
}

fn validate_name(value: &str, full: bool) -> Result<(), St3Error> {
    if value.is_empty() || value.len() > 512 || !value.is_ascii() {
        return Err(St3Error::new(
            "invalid-subject-name",
            format!("invalid subject name `{value}`"),
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"._-@/".contains(&byte))
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || value.ends_with('/')
        || value.split('/').any(|part| part.is_empty() || part == "..")
        || (full && !value.contains('/'))
    {
        return Err(St3Error::new(
            "invalid-subject-name",
            format!("invalid subject name `{value}`"),
        ));
    }
    Ok(())
}

fn validate_full_subject(value: &str) -> Result<(), St3Error> {
    if value.contains("${") {
        if value.starts_with("${")
            && value.ends_with('}')
            && value[2..value.len() - 1].chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '.')
            })
        {
            return Ok(());
        }
        let mut concrete = String::with_capacity(value.len());
        let mut rest = value;
        while let Some(start) = rest.find("${") {
            concrete.push_str(&rest[..start]);
            let tail = &rest[start + 2..];
            let Some(end) = tail.find('}') else {
                return Err(St3Error::new(
                    "invalid-variable",
                    "a variable reference has no closing brace",
                ));
            };
            concrete.push('x');
            rest = &tail[end + 1..];
        }
        concrete.push_str(rest);
        return validate_full_subject(&concrete);
    }
    if let Some(rest) = value.strip_prefix("file/") {
        let Some((host, path)) = rest.split_once(':') else {
            return Err(St3Error::new(
                "invalid-file-subject",
                "a file subject needs `file/HOST:/ABSOLUTE-PATH`",
            ));
        };
        validate_name(host, false)?;
        if !path.starts_with('/') || path.contains("/../") || path.ends_with("/..") {
            return Err(St3Error::new(
                "invalid-file-subject",
                "a file subject needs a safe absolute path",
            ));
        }
        return Ok(());
    }
    validate_name(value, true)
}

fn namespaced(namespace: &str, value: &str) -> String {
    if value.starts_with(&format!("{namespace}/")) {
        value.into()
    } else {
        format!("{namespace}/{value}")
    }
}

fn runtime_id(subject: &str) -> String {
    subject.replace('/', ".")
}

fn bounded_runtime_id(candidate: &str) -> String {
    const MAX_BYTES: usize = 48;
    const DIGEST_BYTES: usize = 20;
    if candidate.len() <= MAX_BYTES {
        return candidate.into();
    }
    let digest = hex::encode(Sha256::digest(candidate.as_bytes()));
    let prefix_bytes = MAX_BYTES - DIGEST_BYTES - 1;
    format!("{}.{}", &candidate[..prefix_bytes], &digest[..DIGEST_BYTES])
}

fn valid_field_path(path: &str) -> bool {
    path.split('.').all(|segment| {
        !segment.is_empty()
            && segment
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphabetic())
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_st3_eval_uses_the_current_graph_grammar() {
        let evals = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("evals/st3");
        let mut parsed = 0;
        for entry in std::fs::read_dir(&evals).unwrap() {
            let entry = entry.unwrap();
            if !entry.file_type().unwrap().is_dir() {
                continue;
            }
            let eval = entry.path().join("eval.kdl");
            if !eval.is_file() {
                continue;
            }
            let source = std::fs::read_to_string(&eval)
                .unwrap()
                .replace("${EVAL_ROOT}", "/tmp/st3-eval-fixture");
            let intent = parse_test_intent(&source, "eval-node")
                .unwrap_or_else(|error| panic!("{}: {error}", eval.display()));
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                intent.missions.iter().any(|(id, mission)| {
                    id.starts_with(&format!("eval/{name}"))
                        && mission.state == crate::model::MissionState::Ready
                }),
                "{} must declare its ready root mission",
                eval.display()
            );
            parsed += 1;
        }
        assert!(parsed >= 24, "the st3 eval corpus unexpectedly shrank");
    }

    #[test]
    fn work_wake_revision_fixtures_use_the_current_graph_grammar() {
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("evals/st3/work-wake-reliability/fixtures");
        for name in ["revisable-v1.kdl", "revisable-v2.kdl", "revisable-v3.kdl"] {
            let fixture = fixtures.join(name);
            let source = std::fs::read_to_string(&fixture)
                .unwrap()
                .replace("{{WORKER}}", "agent/eval-node.worker");
            parse_test_intent(&source, "eval-node")
                .unwrap_or_else(|error| panic!("{}: {error}", fixture.display()));
        }
    }

    #[test]
    fn accepts_a_durable_agent_seat_but_rejects_unowned_tasks() {
        let intent = parse_intent(
            "version 2\nagent \"fleet/cos/standing/cos\" { command \"true\" }",
            "host",
        )
        .expect("a durable seat is valid desired state");
        let seat = &intent.subjects["agent/fleet/cos/standing/cos"];
        assert!(seat.owner_run.is_none());
        assert_eq!(
            seat.member.as_ref().unwrap().runtime_id,
            "fleet.cos.standing.cos"
        );

        let error = parse_intent("version 2\npty \"worker\" { command \"true\" }", "host")
            .expect_err("an unowned task must fail");
        assert_eq!(error.code, "runtime-outside-mission");
    }

    #[test]
    fn rejects_the_removed_wrapper_and_accepts_direct_roots() {
        let removed = parse_intent(
            "version 2\nsubgraph { mission \"work\" state=\"ready\" { goal \"Do the work.\" } }",
            "node",
        )
        .unwrap_err();
        assert_eq!(removed.code, "removed-subgraph");

        let direct = parse_intent(
            "version 2\nmission \"work\" state=\"ready\" { goal \"Do the work.\" }",
            "node",
        )
        .unwrap();
        assert!(direct.missions.contains_key("work"));
    }

    #[test]
    fn repair_is_a_strict_root_declaration() {
        let record = "a".repeat(64);
        let replacement = "b".repeat(64);
        let intent = parse_intent(
            &format!(
                "version 2\nrepair \"record/{record}\" {{ replacement \"{replacement}\"; reason \"replace invalid input\" }}"
            ),
            "node",
        )
        .unwrap();
        assert_eq!(intent.replica_repairs.len(), 1);
        assert_eq!(
            intent.replica_repairs[0].record_ref,
            format!("record/{record}")
        );

        let invalid = parse_intent(
            &format!(
                "version 2\nrepair \"record/{record}\" {{ replacement \"short\"; reason \"replace invalid input\" }}"
            ),
            "node",
        )
        .unwrap_err();
        assert_eq!(invalid.code, "invalid-replacement-claim");
    }

    #[test]
    fn rejects_the_removed_plan_vocabulary() {
        for source in [
            "version 2\nplan \"old\" state=\"ready\" { goal \"Do the work.\" }",
            "version 2\nplan-run \"old\" { }",
        ] {
            let error = parse_intent(source, "node").expect_err("old root syntax must fail");
            assert_eq!(error.code, "unknown-node");
        }

        for field in ["produces-plan \"work\"", "uses-plan output-of=\"compile\""] {
            let source = format!(
                "version 2\nmission \"work\" state=\"ready\" {{ goal \"Do the work.\"; step \"compile\" {{ {field} }} }}"
            );
            let error = parse_intent(&source, "node").expect_err("old step syntax must fail");
            assert_eq!(error.code, "unknown-step-field");
        }
    }

    #[test]
    fn account_declarations_are_root_only_and_strict() {
        let source = r#"
version 2

  account "claude/team-a" {
    provider "anthropic"
    external-account "team-a"
    auth-type "subscription"
  }

"#;
        let intent = parse_intent(source, "node").unwrap();
        assert!(intent.subjects.contains_key("account/claude/team-a"));

        let nested = parse_execution_intent(source, "node", "run-1")
            .expect_err("a mission run cannot own an account");
        assert_eq!(nested.code, "account-inside-mission");

        let deferred = r#"
version 2

  mission "bad" state="ready" {
    goal "Reject nested accounts."
    step "work" {

        account "claude/team-a" {
          provider "anthropic"
          external-account "team-a"
          auth-type "subscription"
        }

    }
  }

"#;
        assert_eq!(
            parse_intent(deferred, "node").unwrap_err().code,
            "account-inside-mission"
        );

        let invalid_auth = source.replace("subscription", "session-cookie");
        assert_eq!(
            parse_intent(&invalid_auth, "node").unwrap_err().code,
            "invalid-auth-type"
        );

        let unknown_field = source.replace(
            "    auth-type \"subscription\"",
            "    auth-type \"subscription\"\n    quota \"100\"",
        );
        assert_eq!(
            parse_intent(&unknown_field, "node").unwrap_err().code,
            "unknown-child"
        );
    }

    #[test]
    fn rejects_st2_document_versions() {
        for source in [
            " agent \"worker\" { command \"true\" } ",
            "version 0\n agent \"worker\" { command \"true\" } ",
            "version 1\n agent \"worker\" { command \"true\" } ",
        ] {
            let error = parse_intent(source, "host").expect_err("st2 KDL must fail");
            assert_eq!(error.code, "unsupported-kdl-version");
        }
    }

    #[test]
    fn parses_plain_agent_and_document_version() {
        let intent = parse_test_intent(
            r#"
version 2

  message "task" {
    to "worker"
    content "doc/tasks/work@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  }
  agent "worker" {
    workspace "/work"
    restart "never"
    harness "claude" {
      prompt "Work on the task."
    }
  }

"#,
            "node",
        )
        .expect("new KDL parses");
        assert!(intent.subjects.contains_key("agent/node.worker"));
        assert!(intent.subjects.contains_key("message/task"));
        assert_eq!(intent.document_refs.len(), 1);
    }

    #[test]
    fn a_named_mission_run_produces_a_valid_agent_runtime_id() {
        let intent = parse_execution_intent(
            r#"version 2
agent "worker" { command "true" }
"#,
            "node",
            "fixture/network-a/run-one",
        )
        .unwrap();
        let member = intent.subjects["agent/fixture/network-a/run-one/worker"]
            .member
            .as_ref()
            .unwrap();
        assert_eq!(member.runtime_id, "fixture.network-a.run-one.worker");
    }

    #[test]
    fn a_long_mission_run_produces_a_bounded_stable_runtime_id() {
        let source = r#"version 2
agent "worker" { command "true" }
"#;
        let first = parse_execution_intent(
            source,
            "node",
            "eval/mission-authority/produced/0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let repeated = parse_execution_intent(
            source,
            "node",
            "eval/mission-authority/produced/0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let other = parse_execution_intent(
            source,
            "node",
            "eval/mission-authority/produced/fedcba9876543210fedcba9876543210",
        )
        .unwrap();
        let subject =
            "agent/eval/mission-authority/produced/0123456789abcdef0123456789abcdef/worker";
        let first_id = &first.subjects[subject].member.as_ref().unwrap().runtime_id;
        let repeated_id = &repeated.subjects[subject]
            .member
            .as_ref()
            .unwrap()
            .runtime_id;
        let other_id = &other
            .subjects
            ["agent/eval/mission-authority/produced/fedcba9876543210fedcba9876543210/worker"]
            .member
            .as_ref()
            .unwrap()
            .runtime_id;

        assert_eq!(first_id, repeated_id);
        assert_ne!(first_id, other_id);
        assert!(first_id.len() <= 48);
    }

    #[test]
    fn a_run_keeps_an_exact_external_agent_party() {
        let intent = parse_execution_intent(
            r#"version 2
agent "worker" { command "true" }
message "local" { from "requester"; to "worker"; content "Local." }
message "external" {
  from "requester"
  to "agent/other/run/peer"
  content "External."
}
"#,
            "node",
            "message/run",
        )
        .unwrap();
        assert_eq!(
            canonical_child_value(&intent.subjects["message/local"].desired, "to"),
            Some(&Value::String("agent/message/run/worker".into()))
        );
        assert_eq!(
            canonical_child_value(&intent.subjects["message/external"].desired, "to"),
            Some(&Value::String("agent/other/run/peer".into()))
        );
    }

    #[test]
    fn claude_always_uses_the_native_interactive_channel() {
        let intent = parse_test_intent(
            r#"
version 2

  agent "worker" {
    workspace "/work"
    harness "claude" {
      prompt "Work on the task."
    }
  }

"#,
            "node",
        )
        .expect("new KDL parses");
        let launch = &intent.subjects["agent/node.worker"]
            .member
            .as_ref()
            .unwrap()
            .launch;
        let crate::model::LaunchSpec::Argv(argv) = launch else {
            panic!("the native driver needs argv");
        };
        assert!(argv.iter().any(|arg| arg == "claude"));
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["--channels", st2::claude_channel::ST3_CHANNEL])
        );
        let settings = argv
            .windows(2)
            .find(|pair| pair[0] == "--settings")
            .map(|pair| &pair[1])
            .expect("typed Claude seats carry lifecycle hook settings");
        assert_eq!(
            serde_json::from_str::<Value>(settings).unwrap(),
            st2::hooks::claude_st3_settings_registration()
        );
        assert!(
            !argv
                .iter()
                .any(|arg| { arg.starts_with("--dangerously-load-development-channels") })
        );
    }

    #[test]
    fn model_free_provider_contracts_build_exact_native_argv() {
        for (provider, extra, prefix) in [
            ("pi", "effort \"high\"", vec!["pi", "--thinking", "high"]),
            ("opencode", "", vec!["opencode", "--prompt"]),
            (
                "omp",
                "effort \"medium\"",
                vec!["omp", "--thinking", "medium"],
            ),
        ] {
            let source = format!(
                r#"version 2

  agent "worker" {{
    workspace "/work"
    harness {provider:?} {{
      {extra}
      prompt "Do the work."
    }}
  }}
"#,
            );
            let intent = parse_test_intent(&source, "node").unwrap();
            let member = intent.subjects["agent/node.worker"]
                .member
                .as_ref()
                .unwrap();
            let LaunchSpec::Argv(argv) = &member.launch else {
                panic!("the typed provider did not build argv");
            };
            let expected_prompt = crate::boot::compose_prompt(Some("Do the work."));
            let mut expected = prefix;
            expected.push(&expected_prompt);
            assert!(
                argv.windows(expected.len())
                    .any(|window| window == expected),
                "{provider}: {argv:?}"
            );
        }
    }

    #[test]
    fn a_harness_without_an_authored_prompt_uses_the_boot_prompt() {
        for provider in ["claude", "codex", "pi", "omp", "opencode"] {
            let intent = parse_test_intent(
                &format!(
                    "version 2\nagent \"worker\" {{ workspace \"/work\"; harness {provider:?} {{}} }}\n"
                ),
                "node",
            )
            .unwrap();
            let member = intent.subjects["agent/node.worker"]
                .member
                .as_ref()
                .unwrap();
            let LaunchSpec::Argv(argv) = &member.launch else {
                panic!("the {provider} driver did not build argv");
            };
            assert_eq!(
                argv.last().map(String::as_str),
                Some(crate::boot::BOOT_PROMPT),
                "{provider}: {argv:?}"
            );
        }
    }

    #[test]
    fn parses_mission_order_and_dependencies() {
        let intent = parse_intent(
            r#"
version 2

  mission "build" state="ready" {
    goal "Complete mission build."
    step "build" {
      title "The first step passes"
       exec "one" { command "true"; restart "never" }
      gate "condition-1" { field "status" "exec/one" is "exited" }
    }

    step "review" {
      title "The work is reviewed"
      depends-on { step "build" completed }
      gate "condition-2" { field "decision" "resource/review" is "approved" }
    }
  }

"#,
            "node",
        )
        .expect("mission KDL parses");
        let mission = &intent.missions["build"];
        assert_eq!(mission.display_order, ["build", "review"]);
        assert!(matches!(
            &mission.steps["review"].dependencies[0],
            crate::model::DependencySpec::Step { step, state }
                if step == "build" && state == "completed"
        ));
    }

    #[test]
    fn local_placement_resolves_to_the_receiving_node() {
        let source = r#"
            version 2

              host "local" {
                document "doc/hosts/node-a@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
              }
              exec "setup" {
                host "local"
                command "true"
              }
              mission "proof" state="ready" {
                goal "Complete mission proof."
                step "verify" {
                  title "The local gate passes"
                  gate "verify" {
                    exec "true"
                    host "local"
                    workspace "."
                  }
                }
              }

        "#;

        let intent = parse_test_intent(source, "node-a").unwrap();

        assert!(intent.subjects.contains_key("host/node-a"));
        assert_eq!(
            intent.subjects["host/node-a"].desired["arguments"][0],
            "node-a"
        );
        assert_eq!(
            intent.subjects["exec/setup"].member.as_ref().unwrap().host,
            "node-a"
        );
        let GateSpec::Mechanical { host, .. } = &intent.missions["proof"].steps["verify"].gates[0]
        else {
            panic!("the test gate is not mechanical");
        };
        assert_eq!(host, "node-a");
    }

    #[test]
    fn nested_tasks_inherit_restart_controls() {
        let intent = parse_test_intent(
            r#"
version 2

  agent "worker" {
    workspace "/work"
    restart "never"
    shutdown-timeout "9s"
    restart {
      attempts 7
      interval "2m"
      delay "3s"
      mode "fail"
    }
    exec "build" { command "true" }
  }

"#,
            "node",
        )
        .unwrap();
        let member = intent.subjects["exec/node.worker/build"]
            .member
            .as_ref()
            .unwrap();
        assert_eq!(member.lifecycle, MemberLifecycle::Service);
        assert_eq!(member.restart, RestartType::Never);
        assert_eq!(member.shutdown_timeout_ms, 9_000);
        assert_eq!(member.restart_intensity.attempts, 7);
        assert_eq!(member.restart_intensity.interval_ms, 120_000);
        assert_eq!(member.restart_intensity.delay_ms, 3_000);
        assert_eq!(member.restart_intensity.mode, "fail");
    }

    #[test]
    fn every_terminal_member_records_its_st3_subject() {
        let intent = parse_test_intent(
            r#"
version 2

  terminal "standalone" { command "sleep 1" }
  agent "worker" {
    workspace "/work"
    command "sleep 1"
    terminal "helper" { command "sleep 1" }
  }

"#,
            "node",
        )
        .unwrap();
        let agent = intent.subjects["agent/node.worker"]
            .member
            .as_ref()
            .unwrap();
        let standalone = intent.subjects["pty/standalone"].member.as_ref().unwrap();
        let helper = intent.subjects["pty/node.worker/helper"]
            .member
            .as_ref()
            .unwrap();
        assert_eq!(agent.tags["st3.subject"], "agent/node.worker");
        assert_eq!(standalone.tags["st3.subject"], "pty/standalone");
        assert_eq!(helper.tags["st3.subject"], "pty/node.worker/helper");
        assert!(intent.deprecated_syntax.is_empty());
        assert_eq!(
            intent.normalized["declarations"][0]["name"],
            Value::String("terminal".into())
        );
    }

    #[test]
    fn legacy_pty_input_normalizes_to_terminal_with_a_deprecation_marker() {
        let terminal = parse_test_intent(
            "version 2\n terminal \"shell\" { command \"sleep 1\" }",
            "node",
        )
        .unwrap();
        let pty =
            parse_test_intent("version 2\n pty \"shell\" { command \"sleep 1\" }", "node").unwrap();

        assert_eq!(pty.source_hash, terminal.source_hash);
        assert_eq!(pty.normalized, terminal.normalized);
        assert!(pty.deprecated_syntax.contains("pty"));
        assert!(terminal.deprecated_syntax.is_empty());
    }

    #[test]
    fn under_is_repeatable_non_owning_agent_metadata() {
        let source = r#"
version 2

  agent "lead" {
    workspace "/work"
    under "worker" reason="the worker supplies a specialist view"
    harness "codex" { prompt "Coordinate only when needed." }
  }
  agent "worker" {
    workspace "/work"
    under "lead" reason="the lead combines the result"
    under "missing"
    harness "codex" { prompt "Do the assigned work." }
  }

"#;
        let intent = parse_test_intent(source, "node").unwrap();
        let worker = agent_under(&intent.subjects["agent/node.worker"].desired);
        assert_eq!(worker.len(), 2);
        assert_eq!(worker[0].agent, "agent/node.lead");
        assert_eq!(
            worker[0].reason.as_deref(),
            Some("the lead combines the result")
        );

        let store = crate::store::Store::open_memory("node").unwrap();
        let preview = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        assert!(preview.blockers.is_empty(), "{:?}", preview.blockers);
        assert!(
            preview
                .warnings
                .iter()
                .any(|warning| warning.contains("missing agent"))
        );
        assert!(
            preview
                .warnings
                .iter()
                .any(|warning| warning.contains("contains a cycle"))
        );
    }

    #[test]
    fn a_host_can_reference_exact_documents() {
        let hash = "a".repeat(64);
        let source = format!(
            r#"version 2
host "node" {{
  document "doc/hosts/node@{hash}"
  agent "worker" {{ workspace "/work"; harness "codex" {{}} }}
}}
"#
        );
        let intent = parse_test_intent(&source, "node").unwrap();
        assert!(
            intent
                .document_refs
                .contains(&format!("doc/hosts/node@{hash}"))
        );
        let host = &intent.subjects["host/node"].desired;
        let document = host["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|child| child["name"] == "document")
            .unwrap();
        assert_eq!(
            document["arguments"]
                .as_array()
                .and_then(|values| values.first())
                .and_then(Value::as_str),
            Some(format!("doc/hosts/node@{hash}").as_str())
        );

        let unpinned = parse_test_intent(
            "version 2\nhost \"node\" { document \"doc/hosts/node\" }\n",
            "node",
        )
        .unwrap_err();
        assert_eq!(unpinned.code, "unpinned-host-document");
    }

    #[test]
    fn observer_fields_are_provider_neutral() {
        let intent = parse_test_intent(
            r#"
version 2

  resource "queue" { kind "custom.example.queue" }
  observer "queue-watch" {
    resource "resource/queue"
    provider "example.queue"
    locator "jobs/ready"
    field "priority"
  }

"#,
            "node",
        )
        .expect("a provider defines its own fields");

        let observer = observer_spec(&intent.subjects["observer/queue-watch"].desired)
            .expect("the observer has a valid specification");
        assert_eq!(observer.provider, "example.queue");
        assert_eq!(observer.fields, ["priority"]);
    }

    #[test]
    fn strict_grammar_rejects_unknown_children_and_properties() {
        let child = parse_test_intent(
            r#"version 2
 agent "worker" { command "true"; retired #true } "#,
            "node",
        )
        .expect_err("retired is old syntax");
        assert_eq!(child.code, "unknown-child");

        let property = parse_test_intent(
            r#"version 2
 exec "work" mystery="value" { command "true" } "#,
            "node",
        )
        .expect_err("unknown property");
        assert_eq!(property.code, "unknown-property");

        let obsolete_resource_binding = parse_test_intent(
            r#"version 2
 resource "source" { kind "custom.st3.document-source"; binding "late" } "#,
            "node",
        )
        .expect_err("resources are unbound until a claim supplies facts");
        assert_eq!(obsolete_resource_binding.code, "unknown-child");
    }

    #[test]
    fn authored_messages_reject_empty_content() {
        let error = parse_intent(
            "version 2\n message \"empty\" { to \"worker\"; content \"  \" } ",
            "node",
        )
        .expect_err("an empty message must not enter the delivery FIFO");
        assert_eq!(error.code, "empty-message");
    }

    #[test]
    fn a_mission_can_pin_bare_document_references() {
        let source = r#"
version 2

  message "task" {
    to "worker"
    content "doc/tasks/work"
  }

"#;
        let resolved = resolve_document_references(
            source,
            &BTreeMap::from([("doc/tasks/work".into(), "a".repeat(64))]),
        )
        .unwrap();
        let intent = parse_intent(&resolved, "node").unwrap();
        assert_eq!(
            intent.document_refs.into_iter().next().unwrap(),
            format!("doc/tasks/work@{}", "a".repeat(64))
        );
    }

    #[test]
    fn planning_sessions_get_stable_bounded_planner_subjects() {
        let long =
            "planning-session/planning/release-with-a-long-name/01994d32d8ef7f8ca18a0170ef58db30";
        let subject = planning_planner_subject(long);
        assert_eq!(subject, planning_planner_subject(long));
        assert!(subject.starts_with("agent/planner."));
        assert!(subject.strip_prefix("agent/").unwrap().len() <= 32);
        assert_ne!(
            subject,
            planning_planner_subject("planning-session/planning/release/another")
        );
    }

    #[test]
    fn a_targeted_planner_receives_its_exact_session_and_run_context() {
        let source = format!(
            r#"
version 2
planning-session "planning/release/revise" {{
  mission "release"
  request "doc/planning/request@{}"
  workspace "/work/release"
  requester "person/operator"
  planner "codex" {{ model "gpt-5.6-sol"; effort "medium" }}
  target-run "mission-run/release/live"
  target-generation "run-generation/release/live/2"
}}
"#,
            "a".repeat(64)
        );
        let intent = parse_intent(&source, "node").unwrap();
        let planner_subject = planning_planner_subject("planning-session/planning/release/revise");
        let planner = &intent.subjects[&planner_subject];
        let desired = serde_json::to_string(&planner.desired).unwrap();
        assert!(
            desired.contains("st3 launch submit planning/release/revise"),
            "{desired}"
        );
        assert!(
            desired.contains("st3 --json mission show mission-run/release/live"),
            "{desired}"
        );
        assert!(
            desired.contains("run-generation/release/live/2"),
            "{desired}"
        );
    }

    #[test]
    fn a_subscription_can_open_an_exact_mission_with_a_resource_input() {
        let revision = "a".repeat(64);
        let source = format!(
            r#"version 2
resource "repo" {{ kind "vcs.repository" }}
observer "github" {{ resource "resource/repo"; provider "github.repository"; locator "owner/repo"; field "pull_requests" }}
subscription "reviews" {{
    observer "observer/github"
    on "pull_requests"
    delivery "mission" {{
      mission "review@{revision}"
      resource "pull-request"
      workspace "/work/reviews"
      requester "agent/fleet/repository/standing/owner"
    }}
}}"#
        );
        let intent = parse_test_intent(&source, "node").unwrap();
        let subscription = intent
            .subjects
            .values()
            .find(|item| item.kind == "subscription")
            .unwrap();
        let spec = subscription_spec(&subscription.desired).unwrap();
        assert_eq!(spec.delivery, "mission");
        assert_eq!(spec.mission.as_deref(), Some("review"));
        assert_eq!(spec.revision.as_deref(), Some(revision.as_str()));
        assert_eq!(spec.resource_input.as_deref(), Some("pull-request"));
        assert_eq!(
            spec.requester.as_deref(),
            Some("agent/fleet/repository/standing/owner")
        );
    }

    #[test]
    fn a_subscription_can_wait_until_every_observed_item_matches() {
        let source = r#"version 2
resource "pull" { kind "vcs.pull-request" }
observer "pull" { resource "resource/pull"; provider "github.pull-request"; locator "owner/repo#1"; field "checks" }
subscription "green" {
  observer "observer/pull"
  to "agent/fleet/cos/standing/cos"
  on "checks"
  when {
    every "checks" {
      field "status" "is" "completed"
      field "conclusion" "is" "success"
    }
  }
  delivery "message"
}"#;
        let intent = parse_test_intent(source, "node").unwrap();
        let subscription = intent
            .subjects
            .values()
            .find(|item| item.kind == "subscription")
            .unwrap();
        let spec = subscription_spec(&subscription.desired).unwrap();
        assert_eq!(
            spec.condition,
            Some(SubscriptionConditionSpec::Every {
                path: "checks".into(),
                fields: vec![
                    QuantifiedFieldSpec {
                        path: "status".into(),
                        operator: "is".into(),
                        value: Value::String("completed".into()),
                    },
                    QuantifiedFieldSpec {
                        path: "conclusion".into(),
                        operator: "is".into(),
                        value: Value::String("success".into()),
                    },
                ],
            })
        );

        let invalid = source.replace("every \"checks\" {", "some \"checks\" {");
        assert_eq!(
            parse_test_intent(&invalid, "node").unwrap_err().code,
            "unknown-subscription-condition"
        );
    }
}
