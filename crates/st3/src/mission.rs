use std::collections::{BTreeMap, BTreeSet};

use kdl::{KdlDocument, KdlNode, KdlValue};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::model::{
    BaselineSpec, CompletionSpec, DependencySpec, GateSpec, LoopAttentionSpec, LoopExhaustionSpec,
    LoopSpec, LoopStopSpec, MetricSource, MetricSpec, MissionInputKind, MissionInputSpec,
    MissionSpec, MissionState, ProductSpec, RetrySpec, RevisionCutover, St3Error, StepSpec,
    UsedMissionSpec, WorkSelector,
};

const VARIABLES: &[&str] = &[
    "ST_MISSION",
    "ST_MISSION_REVISION",
    "ST_MISSION_RUN",
    "ST_RUN_GENERATION",
    "ST_ROOT_MISSION_RUN",
    "ST_ROOT_MISSION_RUN_ID",
    "ST_WORKSPACE",
    "ST_REQUESTER",
    "ST_STEP",
    "ST_STEP_RUN",
    "ST_ATTEMPT",
    "ST_ASSIGNEE",
    "ST_PARENT_STEP_RUN",
    "ST_GATE",
    "ST_AGENT",
    "ST_LOOP_ROUND",
    "ST_LOOP_FEEDBACK",
    "ST_LOOP_ITEM_ID",
    "ST_CANDIDATE_INDEX",
    "loop.round",
    "loop.feedback",
    "loop.item.id",
    "candidate.index",
    "PATH",
];

pub(crate) const LOOP_ROUND_INPUT: &str = "__st3_loop_round";
pub(crate) const LOOP_FEEDBACK_INPUT: &str = "__st3_loop_feedback";
pub(crate) const LOOP_ITEM_INPUT: &str = "__st3_loop_item";
pub(crate) const CANDIDATE_INDEX_INPUT: &str = "__st3_candidate_index";

pub(crate) fn is_reserved_context_name(name: &str) -> bool {
    (VARIABLES.contains(&name) && name != "PATH") || name == "ST3_SUBJECT"
}

pub(crate) fn validate_mission_id(value: &str) -> Result<(), St3Error> {
    validate_id(value, "mission")
}

pub fn parse_missions(
    document: &KdlDocument,
    default_host: &str,
) -> Result<BTreeMap<String, MissionSpec>, St3Error> {
    let mut missions = BTreeMap::new();
    let mut top_level = Vec::new();
    for node in document.nodes() {
        if node.name().value() == "mission" {
            let mission = parse_mission(node, default_host, true, true)?;
            top_level.push(mission.id.clone());
            insert_mission(&mut missions, mission)?;
        }
    }
    let mut embedded = Vec::new();
    for id in top_level {
        collect_embedded_missions(&missions[&id], &mut embedded);
    }
    for mission in embedded {
        insert_mission(&mut missions, mission)?;
    }
    Ok(missions)
}

fn collect_embedded_missions(mission: &MissionSpec, output: &mut Vec<MissionSpec>) {
    for id in &mission.display_order {
        let step = &mission.steps[id];
        if let Some(loop_spec) = &step.loop_spec {
            output.push((*loop_spec.round).clone());
            collect_embedded_missions(&loop_spec.round, output);
            if let Some(branch) = &loop_spec.on_keep {
                output.push((**branch).clone());
                collect_embedded_missions(branch, output);
            }
            if let Some(branch) = &loop_spec.on_discard {
                output.push((**branch).clone());
                collect_embedded_missions(branch, output);
            }
        }
        if let Some(nested) = &step.nested_mission {
            collect_embedded_missions(nested, output);
        }
    }
}

pub(crate) fn mission_closure_ids(mission: &MissionSpec) -> BTreeSet<String> {
    fn collect(mission: &MissionSpec, include_self: bool, output: &mut BTreeSet<String>) {
        if include_self {
            output.insert(mission.id.clone());
        }
        for id in &mission.display_order {
            let step = &mission.steps[id];
            if let Some(loop_spec) = &step.loop_spec {
                collect(&loop_spec.round, true, output);
                if let Some(branch) = &loop_spec.on_keep {
                    collect(branch, true, output);
                }
                if let Some(branch) = &loop_spec.on_discard {
                    collect(branch, true, output);
                }
            }
            if let Some(nested) = &step.nested_mission {
                // A nested step graph lives inside its parent's immutable mission
                // revision. Only loop rounds are additionally published as named
                // mission revisions.
                collect(nested, false, output);
            }
        }
    }

    let mut output = BTreeSet::new();
    collect(mission, true, &mut output);
    output
}

pub fn top_level_mission_ids(missions: &BTreeMap<String, MissionSpec>) -> BTreeSet<String> {
    let mut embedded = BTreeSet::new();
    for mission in missions.values() {
        for id in mission_closure_ids(mission) {
            if id != mission.id {
                embedded.insert(id);
            }
        }
    }
    missions
        .keys()
        .filter(|id| !embedded.contains(*id))
        .cloned()
        .collect()
}

pub fn find_step<'a>(mission: &'a MissionSpec, path: &str) -> Option<&'a StepSpec> {
    for id in &mission.display_order {
        let step = &mission.steps[id];
        if step.path == path {
            return Some(step);
        }
        if let Some(nested) = &step.nested_mission
            && let Some(found) = find_step(nested, path)
        {
            return Some(found);
        }
    }
    None
}

pub fn parent_step_path<'a>(mission: &'a MissionSpec, path: &str) -> Option<&'a str> {
    fn find<'a>(mission: &'a MissionSpec, path: &str, parent: Option<&'a str>) -> Option<&'a str> {
        for id in &mission.display_order {
            let step = &mission.steps[id];
            if step.path == path {
                return parent;
            }
            if let Some(nested) = &step.nested_mission
                && let Some(found) = find(nested, path, Some(&step.path))
            {
                return Some(found);
            }
        }
        None
    }
    find(mission, path, None)
}

fn insert_mission(
    missions: &mut BTreeMap<String, MissionSpec>,
    mission: MissionSpec,
) -> Result<(), St3Error> {
    if missions
        .insert(mission.id.clone(), mission.clone())
        .is_some()
    {
        return Err(St3Error::new(
            "duplicate-mission",
            format!("mission `{}` repeats", mission.id),
        ));
    }
    Ok(())
}

fn parse_mission(
    node: &KdlNode,
    default_host: &str,
    require_state: bool,
    default_completion: bool,
) -> Result<MissionSpec, St3Error> {
    reject_type(node)?;
    ensure_only_properties(
        node,
        &[
            "state",
            "timeout",
            "revisions",
            "revision-reviewer",
            "revision-cutover",
        ],
    )?;
    let id = first_string(node)?;
    validate_id(&id, "mission")?;
    let authored_state = property_string(node, "state")?;
    if require_state && authored_state.is_none() {
        return Err(St3Error::new(
            "missing-mission-state",
            format!("mission `{id}` needs an explicit state"),
        ));
    }
    let state = authored_state
        .map(|value| parse_mission_state(&value))
        .transpose()?
        .unwrap_or(MissionState::Ready);
    let timeout_ms = property_string(node, "timeout")?
        .map(|value| parse_duration(&value))
        .transpose()?;
    if timeout_ms == Some(0) {
        return Err(St3Error::new(
            "invalid-mission-timeout",
            format!("mission `{id}` timeout must be greater than zero"),
        ));
    }
    let revisions_human_only = parse_revision_protection(node)?;
    let revision_reviewer = parse_revision_reviewer(node, revisions_human_only)?;
    let revision_cutover = match property_string(node, "revision-cutover")?.as_deref() {
        None | Some("restart-active") => RevisionCutover::RestartActive,
        Some("when-idle") => RevisionCutover::WhenIdle,
        Some(value) => {
            return Err(St3Error::new(
                "invalid-revision-cutover",
                format!("mission `{id}` has invalid revision cutover `{value}`"),
            ));
        }
    };
    let children = node
        .children()
        .ok_or_else(|| St3Error::new("empty-mission", format!("mission `{id}` has no steps")))?;
    let mut inputs = BTreeMap::new();
    for input_node in children
        .nodes()
        .iter()
        .filter(|child| child.name().value() == "input")
    {
        let input = parse_mission_input(input_node)?;
        if inputs.insert(input.name.clone(), input.clone()).is_some() {
            return Err(St3Error::new(
                "duplicate-mission-input",
                format!("mission `{id}` repeats input `{}`", input.name),
            ));
        }
    }
    let input_names = inputs.keys().cloned().collect::<BTreeSet<_>>();
    let mut goals = Vec::new();
    let mut constraints = Vec::new();
    let mut baselines = Vec::new();
    let mut products = Vec::new();
    let mut gates = Vec::new();
    let mut steps = BTreeMap::new();
    let mut display_order = Vec::new();
    let mut max_active_runs = Some(1);
    let mut concurrent_runs_seen = false;
    let mut assigned_to = None;
    let mut available_to = Vec::new();
    let mut completion = None;
    let mut finally_seen = false;
    let mut produces_seen = false;
    let mut baseline_names = BTreeSet::new();
    let mut gate_names = BTreeSet::new();
    let mut declarations = Vec::new();
    let mut revision_owners = Vec::new();
    for child in children.nodes() {
        match child.name().value() {
            "goal" => goals.push(plain_string(child)?),
            "constraint" => push_constraint(&mut constraints, child, &format!("mission `{id}`"))?,
            "input" => {}
            "concurrent-runs" => {
                if concurrent_runs_seen {
                    return Err(St3Error::new(
                        "duplicate-mission-field",
                        format!("mission `{id}` repeats `concurrent-runs`"),
                    ));
                }
                max_active_runs = parse_concurrent_runs(child)?;
                concurrent_runs_seen = true;
            }
            "assigned-to" => {
                if assigned_to.is_some() {
                    return Err(St3Error::new(
                        "duplicate-mission-field",
                        format!("mission `{id}` repeats `assigned-to`"),
                    ));
                }
                assigned_to = Some(normalize_assignee(&first_string(child)?, default_host));
            }
            "available-to" => {
                let agent = normalize_assignee(&first_string(child)?, default_host);
                if available_to.contains(&agent) {
                    return Err(St3Error::new(
                        "duplicate-work-agent",
                        format!("mission `{id}` repeats available agent `{agent}`"),
                    ));
                }
                available_to.push(agent);
            }
            "baseline" => {
                let baseline = parse_baseline(child)?;
                if !baseline_names.insert(baseline.name.clone()) {
                    return Err(St3Error::new(
                        "duplicate-baseline",
                        format!("mission `{id}` repeats baseline `{}`", baseline.name),
                    ));
                }
                baselines.push(baseline);
            }
            "produces" if !produces_seen => {
                products = parse_products(child)?;
                produces_seen = true;
            }
            "produces" => {
                return Err(St3Error::new(
                    "duplicate-mission-field",
                    format!("mission `{id}` repeats `produces`"),
                ));
            }
            "gate" => {
                let gate = crate::graph::parse_gate(child, default_host)?;
                if !gate_names.insert(crate::graph::gate_name(&gate).to_owned()) {
                    return Err(St3Error::new(
                        "duplicate-gate",
                        format!(
                            "mission `{id}` repeats gate `{}`",
                            crate::graph::gate_name(&gate)
                        ),
                    ));
                }
                gates.push(gate);
            }
            "step" => {
                let step = parse_step(child, "", default_host, false, &input_names)?;
                if steps.insert(step.id.clone(), step.clone()).is_some() {
                    return Err(St3Error::new(
                        "duplicate-step",
                        format!("mission `{id}` repeats step `{}`", step.id),
                    ));
                }
                display_order.push(step.id);
            }
            "queue" => {
                for step in parse_queue(child, default_host, &input_names)? {
                    if steps.insert(step.id.clone(), step.clone()).is_some() {
                        return Err(St3Error::new(
                            "duplicate-step",
                            format!("mission `{id}` repeats step `{}`", step.id),
                        ));
                    }
                    display_order.push(step.id);
                }
            }
            "loop" => {
                let step = parse_loop_group(child, &id, default_host, &inputs, timeout_ms)?;
                if steps.insert(step.id.clone(), step.clone()).is_some() {
                    return Err(St3Error::new(
                        "duplicate-step",
                        format!("mission `{id}` repeats graph node `{}`", step.id),
                    ));
                }
                display_order.push(step.id);
            }
            "completion" => {
                if completion.is_some() {
                    return Err(St3Error::new(
                        "duplicate-mission-field",
                        format!("mission `{id}` repeats `completion`"),
                    ));
                }
                completion = Some(parse_completion(child, default_host)?);
            }
            "finally" => {
                if finally_seen {
                    return Err(St3Error::new(
                        "duplicate-mission-field",
                        format!("mission `{id}` repeats `finally`"),
                    ));
                }
                ensure_bare(child)?;
                let body = child.children().ok_or_else(|| {
                    St3Error::new(
                        "empty-finally",
                        format!("mission `{id}` has an empty finally block"),
                    )
                })?;
                if body.nodes().is_empty() {
                    return Err(St3Error::new(
                        "empty-finally",
                        format!("mission `{id}` has an empty finally block"),
                    ));
                }
                for final_node in body.nodes() {
                    if final_node.name().value() != "step" {
                        return Err(St3Error::new(
                            "invalid-finally-child",
                            format!(
                                "mission `{id}` finally cannot contain `{}`",
                                final_node.name().value()
                            ),
                        ));
                    }
                    let step = parse_step(final_node, "", default_host, true, &input_names)?;
                    if steps.insert(step.id.clone(), step.clone()).is_some() {
                        return Err(St3Error::new(
                            "duplicate-step",
                            format!("mission `{id}` repeats step `{}`", step.id),
                        ));
                    }
                    display_order.push(step.id);
                }
                finally_seen = true;
            }
            "account" => {
                return Err(St3Error::new(
                    "account-inside-mission",
                    format!("mission `{id}` cannot own an account"),
                ));
            }
            name if crate::graph::is_mission_declaration(name) => {
                crate::graph::validate_deferred_declaration(child)?;
                let mut declaration = KdlDocument::new();
                declaration.nodes_mut().push(child.clone());
                revision_owners.extend(direct_agent_owners(&declaration, default_host)?);
                revision_owners.sort();
                revision_owners.dedup();
                declarations.push(child.clone());
            }
            other => {
                return Err(St3Error::new(
                    "invalid-mission-child",
                    format!("mission `{id}` cannot contain `{other}`"),
                ));
            }
        }
    }
    validate_goal_count(&format!("mission `{id}`"), &goals, true)?;
    // Missions are finite work graphs by default. Durable harness availability belongs to a
    // top-level agent seat, so omitting completion must never silently create a standing run.
    let completion = if default_completion {
        completion.or(Some(CompletionSpec::AllStepsExhausted))
    } else {
        completion
    };
    let work_selector =
        build_work_selector(&format!("mission `{id}`"), assigned_to, available_to, false)?;
    validate_dependencies(&id, &steps)?;
    if let Some(CompletionSpec::Dependencies { dependencies }) = &completion {
        validate_dependency_targets(&id, "completion", dependencies, &steps)?;
        for dependency in dependencies {
            if let DependencySpec::Step { step, .. } = dependency
                && steps[step].finally
            {
                return Err(St3Error::new(
                    "completion-depends-on-final-step",
                    format!("completion in mission `{id}` cannot depend on final step `{step}`"),
                ));
            }
        }
    }
    let declarations_kdl = declarations_document(declarations);
    let mut mission = MissionSpec {
        subject: format!("mission/{id}"),
        id,
        state,
        revision: String::new(),
        inputs,
        max_active_runs,
        timeout_ms,
        revision_owners,
        revisions_human_only,
        revision_reviewer,
        revision_cutover,
        declarations_kdl,
        work_selector,
        completion,
        goals,
        constraints,
        baselines,
        products,
        gates,
        steps,
        display_order,
    };
    validate_variables(
        &serde_json::to_value(&mission).map_err(internal)?,
        &input_names,
    )?;
    mission.revision = hash(&mission)?;
    Ok(mission)
}

fn parse_loop_group(
    node: &KdlNode,
    parent_mission: &str,
    default_host: &str,
    inputs: &BTreeMap<String, MissionInputSpec>,
    parent_timeout_ms: Option<u64>,
) -> Result<StepSpec, St3Error> {
    reject_type(node)?;
    if ["for-each", "field", "max-parallel"]
        .into_iter()
        .any(|name| node.get(name).is_some())
    {
        return Err(St3Error::new(
            "removed-loop-shape",
            "loop for-each and parallel collection modes were removed; compose explicit mission steps around a sequential loop",
        ));
    }
    ensure_only_properties(node, &["timeout"])?;
    let id = first_string(node)?;
    validate_id(&id, "loop")?;
    let path = id.clone();
    let timeout_ms = property_string(node, "timeout")?
        .map(|value| parse_duration(&value))
        .transpose()?;
    if timeout_ms == Some(0) {
        return Err(St3Error::new(
            "invalid-loop-timeout",
            format!("loop `{id}` timeout must be greater than zero"),
        ));
    }
    let for_each = None;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("empty-loop", format!("loop `{id}` is empty")))?;
    let rounds = child_integer(body, "max-rounds")?
        .ok_or_else(|| St3Error::new("missing-loop-rounds", "loop needs `max-rounds`"))?;
    if !(1..=100).contains(&rounds) {
        return Err(St3Error::new(
            "invalid-loop-rounds",
            "loop max-rounds must be between 1 and 100",
        ));
    }
    let dependencies = body
        .nodes()
        .iter()
        .filter(|child| child.name().value() == "depends-on")
        .map(|child| parse_dependencies(child, default_host))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let mut metrics = Vec::new();
    let mut metric_names = BTreeSet::new();
    for child in body
        .nodes()
        .iter()
        .filter(|child| child.name().value() == "metric")
    {
        let metric = parse_metric(child, default_host)?;
        if !metric_names.insert(metric.name.clone()) {
            return Err(St3Error::new(
                "duplicate-loop-metric",
                format!("loop `{id}` repeats metric `{}`", metric.name),
            ));
        }
        metrics.push(metric);
    }
    let stop = body
        .nodes()
        .iter()
        .find(|child| child.name().value() == "stop")
        .map(|child| parse_loop_stop(child, &metric_names))
        .transpose()?
        .unwrap_or_default();
    let until = body
        .nodes()
        .iter()
        .find(|child| child.name().value() == "until")
        .map(|child| parse_until(child, default_host))
        .transpose()?
        .unwrap_or_default();
    for metric in &metrics {
        if let MetricSource::Gate { gate } = &metric.source
            && !until
                .iter()
                .any(|candidate| crate::graph::gate_name(candidate) == gate)
        {
            return Err(St3Error::new(
                "unknown-metric-gate",
                format!("metric `{}` names unknown gate `{gate}`", metric.name),
            ));
        }
    }
    if unique_named_child(body, "candidates")?.is_some() {
        return Err(St3Error::new(
            "removed-loop-shape",
            "loop best-of-N candidates were removed; model candidates as explicit mission steps and gate the selected result",
        ));
    }
    let candidates = None;
    let round_node = unique_named_child(body, "round")?.ok_or_else(|| {
        St3Error::new(
            "missing-loop-round",
            format!("loop `{id}` needs one round block"),
        )
    })?;
    let child_timeout_ms = [timeout_ms, parent_timeout_ms].into_iter().flatten().min();
    let round = parse_embedded_mission(
        round_node,
        &format!("__st3/{parent_mission}/loop/{id}/round"),
        default_host,
        inputs,
        child_timeout_ms,
    )?;
    if round.completion.is_none() {
        return Err(St3Error::new(
            "missing-round-completion",
            format!("loop `{id}` round needs an explicit completion"),
        ));
    }
    let keep_metric = unique_named_child(body, "keep-if")?
        .map(|child| {
            ensure_only_properties(child, &["metric"])?;
            ensure_no_children(child)?;
            property_string(child, "metric")?.ok_or_else(|| {
                St3Error::new("missing-keep-metric", "keep-if needs a metric property")
            })
        })
        .transpose()?;
    if let Some(metric) = &keep_metric
        && !metric_names.contains(metric)
    {
        return Err(St3Error::new(
            "unknown-keep-metric",
            format!("loop `{id}` keep-if names unknown metric `{metric}`"),
        ));
    }
    let on_keep = parse_loop_branch(
        body,
        "on-keep",
        parent_mission,
        &id,
        default_host,
        inputs,
        child_timeout_ms,
    )?;
    let on_discard = parse_loop_branch(
        body,
        "on-discard",
        parent_mission,
        &id,
        default_host,
        inputs,
        child_timeout_ms,
    )?;
    if candidates.is_some() {
        if keep_metric.is_some() {
            return Err(St3Error::new(
                "conflicting-candidate-keep-policy",
                format!("loop `{id}` candidates use their selector instead of keep-if"),
            ));
        }
        if on_keep.is_some() != on_discard.is_some() {
            return Err(St3Error::new(
                "incomplete-candidate-projection",
                format!("loop `{id}` candidates need both on-keep and on-discard"),
            ));
        }
    } else if keep_metric.is_some() != (on_keep.is_some() && on_discard.is_some()) {
        return Err(St3Error::new(
            "incomplete-keep-policy",
            format!("loop `{id}` keep-if, on-keep, and on-discard must appear together"),
        ));
    }
    let (on_exhausted, exhaustion_attention) = unique_named_child(body, "on-exhausted")?
        .map(|child| parse_loop_exhaustion(child, default_host))
        .transpose()?
        .unwrap_or((LoopExhaustionSpec::default(), None));
    for child in body.nodes() {
        if !matches!(
            child.name().value(),
            "max-rounds"
                | "depends-on"
                | "metric"
                | "stop"
                | "until"
                | "candidates"
                | "round"
                | "keep-if"
                | "on-keep"
                | "on-discard"
                | "on-exhausted"
        ) {
            return Err(St3Error::new(
                "invalid-loop-field",
                format!("loop `{id}` cannot contain `{}`", child.name().value()),
            ));
        }
    }
    let loop_spec = LoopSpec {
        id: id.clone(),
        path: path.clone(),
        max_rounds: rounds as u32,
        timeout_ms,
        for_each,
        candidates,
        metrics,
        stop,
        until,
        round: Box::new(round),
        keep_metric,
        on_keep,
        on_discard,
        on_exhausted,
        exhaustion_attention,
    };
    let mut step = StepSpec {
        id,
        path,
        queue: None,
        queue_position: None,
        title: None,
        goals: Vec::new(),
        constraints: Vec::new(),
        timeout_ms: None,
        retry: RetrySpec {
            attempts: rounds as u32,
            backoff_ms: 0,
        },
        finally: false,
        work_selector: Some(WorkSelector::Agentless),
        revision_owners: Vec::new(),
        revisions_human_only: false,
        revision_reviewer: None,
        dependencies,
        baselines: Vec::new(),
        documents: Vec::new(),
        declarations_kdl: None,
        products: Vec::new(),
        produces_mission: None,
        uses_mission: None,
        gates: Vec::new(),
        nested_mission: None,
        loop_spec: Some(Box::new(loop_spec)),
        definition_hash: String::new(),
    };
    step.definition_hash = hash(&step)?;
    Ok(step)
}

fn parse_embedded_mission(
    node: &KdlNode,
    id: &str,
    default_host: &str,
    inputs: &BTreeMap<String, MissionInputSpec>,
    timeout_ms: Option<u64>,
) -> Result<MissionSpec, St3Error> {
    reject_type(node)?;
    ensure_only_properties(node, &[])?;
    let mut body = KdlDocument::new();
    for input in inputs.values() {
        let mut input_node = KdlNode::new("input");
        input_node
            .entries_mut()
            .push(kdl::KdlEntry::new(input.name.as_str()));
        input_node.entries_mut().push(kdl::KdlEntry::new_prop(
            "kind",
            match input.kind {
                MissionInputKind::Text => "text",
                MissionInputKind::Resource => "resource",
            },
        ));
        body.nodes_mut().push(input_node);
    }
    for name in [
        LOOP_ROUND_INPUT,
        LOOP_FEEDBACK_INPUT,
        LOOP_ITEM_INPUT,
        CANDIDATE_INDEX_INPUT,
    ] {
        let mut input_node = KdlNode::new("input");
        input_node.entries_mut().push(kdl::KdlEntry::new(name));
        input_node
            .entries_mut()
            .push(kdl::KdlEntry::new_prop("kind", "text"));
        body.nodes_mut().push(input_node);
    }
    let mut goal = KdlNode::new("goal");
    goal.entries_mut()
        .push(kdl::KdlEntry::new("Execute one bounded graph iteration."));
    body.nodes_mut().push(goal);
    let children = node.children().ok_or_else(|| {
        St3Error::new(
            "empty-embedded-mission",
            format!("`{}` has no body", node.name().value()),
        )
    })?;
    body.nodes_mut().extend(children.nodes().iter().cloned());
    let mut mission = KdlNode::new("mission");
    mission.entries_mut().push(kdl::KdlEntry::new(id));
    mission.set_children(body);
    let mut parsed = parse_mission(&mission, default_host, false, false)?;
    parsed.max_active_runs = None;
    parsed.timeout_ms = timeout_ms;
    parsed.revision.clear();
    parsed.revision = hash(&parsed)?;
    Ok(parsed)
}

fn parse_loop_branch(
    body: &KdlDocument,
    name: &str,
    parent_mission: &str,
    loop_id: &str,
    default_host: &str,
    inputs: &BTreeMap<String, MissionInputSpec>,
    timeout_ms: Option<u64>,
) -> Result<Option<Box<MissionSpec>>, St3Error> {
    unique_named_child(body, name)?
        .map(|node| {
            let mission = parse_embedded_mission(
                node,
                &format!("__st3/{parent_mission}/loop/{loop_id}/{name}"),
                default_host,
                inputs,
                timeout_ms,
            )?;
            if mission.completion.is_none() {
                return Err(St3Error::new(
                    "missing-branch-completion",
                    format!("loop `{loop_id}` {name} needs an explicit completion"),
                ));
            }
            Ok(Box::new(mission))
        })
        .transpose()
}

fn parse_until(node: &KdlNode, default_host: &str) -> Result<Vec<GateSpec>, St3Error> {
    ensure_bare(node)?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("empty-loop-until", "until is empty"))?;
    if body.nodes().is_empty() {
        return Err(St3Error::new("empty-loop-until", "until is empty"));
    }
    let mut names = BTreeSet::new();
    body.nodes()
        .iter()
        .map(|child| {
            if child.name().value() != "gate" {
                return Err(St3Error::new(
                    "invalid-loop-until",
                    "until accepts only gate blocks",
                ));
            }
            let gate = crate::graph::parse_gate(child, default_host)?;
            if !names.insert(crate::graph::gate_name(&gate).to_owned()) {
                return Err(St3Error::new("duplicate-gate", "an until gate repeats"));
            }
            Ok(gate)
        })
        .collect()
}

fn parse_metric(node: &KdlNode, default_host: &str) -> Result<MetricSpec, St3Error> {
    reject_type(node)?;
    ensure_only_properties(node, &["direction", "min-improvement"])?;
    let name = first_string(node)?;
    validate_id(&name, "metric")?;
    let direction = property_string(node, "direction")?.ok_or_else(|| {
        St3Error::new(
            "missing-metric-direction",
            format!("metric `{name}` needs direction"),
        )
    })?;
    if !matches!(direction.as_str(), "higher" | "lower") {
        return Err(St3Error::new(
            "invalid-metric-direction",
            "metric direction must be higher or lower",
        ));
    }
    let min_improvement = property_number(node, "min-improvement")?.unwrap_or(0.0);
    if !min_improvement.is_finite() || min_improvement < 0.0 {
        return Err(St3Error::new(
            "invalid-metric-improvement",
            "metric min-improvement must be a finite non-negative number",
        ));
    }
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("empty-metric", format!("metric `{name}` is empty")))?;
    if body.nodes().len() != 1 {
        return Err(St3Error::new(
            "invalid-metric-source",
            format!("metric `{name}` needs exactly one source"),
        ));
    }
    let source = &body.nodes()[0];
    let source = match source.name().value() {
        "from-gate" => MetricSource::Gate {
            gate: first_string(source)?,
        },
        "field" => {
            let values = positional_strings(source)?;
            if values.len() != 2 {
                return Err(St3Error::new(
                    "invalid-metric-field",
                    "a metric field needs a path and a resource subject",
                ));
            }
            MetricSource::Field {
                path: values[0].clone(),
                subject: values[1].clone(),
            }
        }
        "exec" => {
            let command = first_string(source)?;
            let source_body = source.children().ok_or_else(|| {
                St3Error::new("missing-metric-exec-body", "a metric exec needs a body")
            })?;
            MetricSource::Exec {
                command,
                host: placement_host_for_mission(
                    child_string(source_body, "host")?.ok_or_else(|| {
                        St3Error::new("missing-metric-host", "a metric exec needs host")
                    })?,
                    default_host,
                ),
                workspace: child_string(source_body, "workspace")?.ok_or_else(|| {
                    St3Error::new("missing-metric-workspace", "a metric exec needs workspace")
                })?,
                environment: parse_string_map(source_body, "env")?,
                time_limit_ms: child_string(source_body, "time-limit")?
                    .map(|value| parse_duration(&value))
                    .transpose()?
                    .unwrap_or(120_000),
            }
        }
        other => {
            return Err(St3Error::new(
                "invalid-metric-source",
                format!("metric `{name}` cannot use `{other}`"),
            ));
        }
    };
    Ok(MetricSpec {
        name,
        direction,
        min_improvement,
        source,
    })
}

fn parse_loop_stop(node: &KdlNode, metrics: &BTreeSet<String>) -> Result<LoopStopSpec, St3Error> {
    ensure_bare(node)?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("empty-loop-stop", "stop is empty"))?;
    let mut stop = LoopStopSpec::default();
    for child in body.nodes() {
        match child.name().value() {
            "plateau" => {
                ensure_only_properties(child, &["metric", "rounds"])?;
                let metric = property_string(child, "metric")?.ok_or_else(|| {
                    St3Error::new("missing-plateau-metric", "plateau needs metric")
                })?;
                if !metrics.contains(&metric) {
                    return Err(St3Error::new(
                        "unknown-plateau-metric",
                        format!("plateau names unknown metric `{metric}`"),
                    ));
                }
                let rounds = property_integer(child, "rounds")?.ok_or_else(|| {
                    St3Error::new("missing-plateau-rounds", "plateau needs rounds")
                })?;
                if !(1..=100).contains(&rounds) {
                    return Err(St3Error::new(
                        "invalid-plateau-rounds",
                        "plateau rounds must be between 1 and 100",
                    ));
                }
                if stop.plateau_metric.replace(metric).is_some() {
                    return Err(St3Error::new("duplicate-plateau", "plateau repeats"));
                }
                stop.plateau_rounds = Some(rounds as u32);
            }
            "repeated-failure" => {
                let value = child_integer_value(child)?;
                if !(2..=100).contains(&value) {
                    return Err(St3Error::new(
                        "invalid-repeated-failure",
                        "repeated-failure must be between 2 and 100",
                    ));
                }
                if stop.repeated_failure.replace(value as u32).is_some() {
                    return Err(St3Error::new(
                        "duplicate-repeated-failure",
                        "repeated-failure repeats",
                    ));
                }
            }
            "token-budget" => {
                let value = child_integer_value(child)?;
                if value <= 0 {
                    return Err(St3Error::new(
                        "invalid-loop-token-budget",
                        "a loop token budget must be positive",
                    ));
                }
                if stop.token_budget.replace(value as u64).is_some() {
                    return Err(St3Error::new(
                        "duplicate-loop-token-budget",
                        "token-budget repeats",
                    ));
                }
            }
            other => {
                return Err(St3Error::new(
                    "invalid-loop-stop",
                    format!("stop cannot contain `{other}`"),
                ));
            }
        }
    }
    Ok(stop)
}

fn parse_loop_exhaustion(
    node: &KdlNode,
    default_host: &str,
) -> Result<(LoopExhaustionSpec, Option<LoopAttentionSpec>), St3Error> {
    ensure_bare(node)?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("empty-loop-exhaustion", "on-exhausted is empty"))?;
    if body.nodes().is_empty() || body.nodes().len() > 2 {
        return Err(St3Error::new(
            "invalid-loop-exhaustion",
            "on-exhausted needs fail, succeed, or one human gate",
        ));
    }
    let child = &body.nodes()[0];
    let outcome = match child.name().value() {
        "fail" => {
            ensure_bare(child)?;
            LoopExhaustionSpec::Fail
        }
        "succeed" => {
            ensure_bare(child)?;
            LoopExhaustionSpec::Succeed
        }
        "gate" => match crate::graph::parse_gate(child, default_host)? {
            gate @ GateSpec::Human { .. } => LoopExhaustionSpec::Human { gate },
            _ => {
                return Err(St3Error::new(
                    "invalid-loop-exhaustion-gate",
                    "on-exhausted accepts only a human gate",
                ));
            }
        },
        _ => {
            return Err(St3Error::new(
                "invalid-loop-exhaustion",
                "on-exhausted needs fail, succeed, or one human gate",
            ));
        }
    };
    let attention = body.nodes().get(1).map(parse_loop_attention).transpose()?;
    if attention.is_some() && !matches!(outcome, LoopExhaustionSpec::Fail) {
        return Err(St3Error::new(
            "invalid-loop-exhaustion-attention",
            "only a failed loop exhaustion can request attention",
        ));
    }
    Ok((outcome, attention))
}

fn parse_loop_attention(node: &KdlNode) -> Result<LoopAttentionSpec, St3Error> {
    if node.name().value() != "attention" {
        return Err(St3Error::new(
            "invalid-loop-exhaustion",
            "the second on-exhausted entry must be attention",
        ));
    }
    reject_type(node)?;
    ensure_only_properties(node, &[])?;
    let title = first_string(node)?;
    if title.trim().is_empty() {
        return Err(St3Error::new(
            "invalid-loop-attention-title",
            "loop exhaustion attention needs a title",
        ));
    }
    let body = node.children().ok_or_else(|| {
        St3Error::new(
            "missing-loop-attention-body",
            "loop exhaustion attention needs reviewer and severity",
        )
    })?;
    for child in body.nodes() {
        if !matches!(child.name().value(), "reviewer" | "severity") {
            return Err(St3Error::new(
                "invalid-loop-attention-field",
                format!(
                    "loop exhaustion attention cannot contain `{}`",
                    child.name().value()
                ),
            ));
        }
    }
    let reviewer = unique_named_child(body, "reviewer")?
        .ok_or_else(|| {
            St3Error::new(
                "missing-loop-attention-reviewer",
                "loop exhaustion attention needs a reviewer",
            )
        })
        .and_then(plain_string)?;
    if !reviewer.starts_with("person/") {
        return Err(St3Error::new(
            "invalid-loop-attention-reviewer",
            "loop exhaustion attention needs a full person subject",
        ));
    }
    let severity = unique_named_child(body, "severity")?
        .map(plain_string)
        .transpose()?
        .unwrap_or_else(|| "error".into());
    if !matches!(severity.as_str(), "warning" | "error") {
        return Err(St3Error::new(
            "invalid-loop-attention-severity",
            "loop exhaustion attention severity must be warning or error",
        ));
    }
    Ok(LoopAttentionSpec {
        title,
        reviewer,
        severity,
    })
}

fn unique_named_child<'a>(
    body: &'a KdlDocument,
    name: &str,
) -> Result<Option<&'a KdlNode>, St3Error> {
    let found = body
        .nodes()
        .iter()
        .filter(|node| node.name().value() == name)
        .collect::<Vec<_>>();
    if found.len() > 1 {
        return Err(St3Error::new(
            "duplicate-field",
            format!("`{name}` repeats"),
        ));
    }
    Ok(found.first().copied())
}

fn parse_step(
    node: &KdlNode,
    parent_path: &str,
    default_host: &str,
    finally: bool,
    input_names: &BTreeSet<String>,
) -> Result<StepSpec, St3Error> {
    reject_type(node)?;
    ensure_only_properties(node, &["timeout", "revisions", "revision-reviewer"])?;
    let id = first_string(node)?;
    validate_id(&id, "step")?;
    let path = if parent_path.is_empty() {
        id.clone()
    } else {
        format!("{parent_path}/{id}")
    };
    let timeout_ms = property_string(node, "timeout")?
        .map(|value| parse_duration(&value))
        .transpose()?;
    let revisions_human_only = parse_revision_protection(node)?;
    let revision_reviewer = parse_revision_reviewer(node, revisions_human_only)?;
    let mut title = None;
    let mut goals = Vec::new();
    let mut constraints = Vec::new();
    let mut assigned_to = None;
    let mut available_to = Vec::new();
    let mut agentless = false;
    let mut dependencies = Vec::new();
    let mut baselines = Vec::new();
    let mut documents = Vec::new();
    let mut declarations = Vec::new();
    let mut products = Vec::new();
    let mut produces_mission = None;
    let mut uses_mission = None;
    let mut gates = Vec::new();
    let mut nested_mission = None;
    let mut retry = RetrySpec::default();
    let mut revision_owners = Vec::new();
    if let Some(children) = node.children() {
        let mut names = BTreeSet::new();
        let mut baseline_names = BTreeSet::new();
        let mut gate_names = BTreeSet::new();
        for child in children.nodes() {
            let name = child.name().value();
            if !matches!(
                name,
                "goal"
                    | "constraint"
                    | "baseline"
                    | "gate"
                    | "depends-on"
                    | "document"
                    | "available-to"
            ) && !crate::graph::is_mission_declaration(name)
                && !names.insert(name.to_owned())
            {
                return Err(St3Error::new(
                    "duplicate-step-field",
                    format!("step `{path}` repeats `{name}`"),
                ));
            }
            match name {
                "title" => title = Some(first_string(child)?),
                "goal" => goals.push(plain_string(child)?),
                "constraint" => {
                    push_constraint(&mut constraints, child, &format!("step `{path}`"))?
                }
                "baseline" => {
                    let baseline = parse_baseline(child)?;
                    if !baseline_names.insert(baseline.name.clone()) {
                        return Err(St3Error::new(
                            "duplicate-baseline",
                            format!("step `{path}` repeats baseline `{}`", baseline.name),
                        ));
                    }
                    baselines.push(baseline);
                }
                "assigned-to" => {
                    assigned_to = Some(normalize_assignee(&first_string(child)?, default_host))
                }
                "available-to" => {
                    let agent = normalize_assignee(&first_string(child)?, default_host);
                    if available_to.contains(&agent) {
                        return Err(St3Error::new(
                            "duplicate-work-agent",
                            format!("step `{path}` repeats available agent `{agent}`"),
                        ));
                    }
                    available_to.push(agent);
                }
                "agentless" => {
                    ensure_bare(child)?;
                    agentless = true;
                }
                "depends-on" => dependencies.extend(parse_dependencies(child, default_host)?),
                "document" => documents.push(parse_step_document(child)?),
                "account" => {
                    return Err(St3Error::new(
                        "account-inside-mission",
                        format!("step `{path}` cannot own an account"),
                    ));
                }
                name if crate::graph::is_mission_declaration(name) => {
                    crate::graph::validate_deferred_declaration(child)?;
                    let mut declaration = KdlDocument::new();
                    declaration.nodes_mut().push(child.clone());
                    revision_owners.extend(direct_agent_owners(&declaration, default_host)?);
                    revision_owners.sort();
                    revision_owners.dedup();
                    declarations.push(child.clone());
                }
                "produces" => products = parse_products(child)?,
                "produces-mission" => produces_mission = Some(parse_produced_mission(child)?),
                "uses-mission" => uses_mission = Some(parse_used_mission(child)?),
                "gate" => {
                    let gate = crate::graph::parse_gate(child, default_host)?;
                    if matches!(gate, GateSpec::Deadline { .. }) {
                        return Err(St3Error::new(
                            "invalid-mission-deadline",
                            format!("step `{path}` must use its timeout property"),
                        ));
                    }
                    if !gate_names.insert(crate::graph::gate_name(&gate).to_owned()) {
                        return Err(St3Error::new(
                            "duplicate-gate",
                            format!(
                                "step `{path}` repeats gate `{}`",
                                crate::graph::gate_name(&gate)
                            ),
                        ));
                    }
                    gates.push(gate);
                }
                "mission" => {
                    let mut mission = parse_mission(child, default_host, false, true)?;
                    rewrite_nested_paths(&mut mission, &path)?;
                    nested_mission = Some(Box::new(mission));
                }
                "retry" => {
                    retry = parse_retry(child)?;
                }
                "loop" => {
                    return Err(St3Error::new(
                        "loop-is-graph-node",
                        format!(
                            "step `{path}` cannot contain `loop`; declare a named loop beside the step"
                        ),
                    ));
                }
                other => {
                    return Err(St3Error::new(
                        "unknown-step-field",
                        format!("step `{path}` cannot contain `{other}`"),
                    ));
                }
            }
        }
    }
    validate_goal_count(&format!("step `{path}`"), &goals, false)?;
    let work_selector = build_work_selector(
        &format!("step `{path}`"),
        assigned_to,
        available_to,
        agentless,
    )?;
    let declarations_kdl = declarations_document(declarations);
    let mut step = StepSpec {
        id,
        path,
        queue: None,
        queue_position: None,
        title,
        goals,
        constraints,
        timeout_ms,
        retry,
        finally,
        work_selector,
        revision_owners,
        revisions_human_only,
        revision_reviewer,
        dependencies,
        baselines,
        documents,
        declarations_kdl,
        products,
        produces_mission,
        uses_mission,
        gates,
        nested_mission,
        loop_spec: None,
        definition_hash: String::new(),
    };
    validate_variables(&serde_json::to_value(&step).map_err(internal)?, input_names)?;
    step.definition_hash = hash(&step)?;
    Ok(step)
}

fn parse_queue(
    node: &KdlNode,
    default_host: &str,
    input_names: &BTreeSet<String>,
) -> Result<Vec<StepSpec>, St3Error> {
    reject_type(node)?;
    ensure_only_properties(node, &[])?;
    let values = positional_strings(node)?;
    if values.len() != 1 {
        return Err(St3Error::new(
            "invalid-queue-id",
            "a queue needs exactly one ID",
        ));
    }
    let id = values[0].clone();
    validate_id(&id, "queue")?;
    let children = node
        .children()
        .ok_or_else(|| St3Error::new("empty-queue", format!("queue `{id}` has no steps")))?;
    let mut assigned_to = None;
    let mut available_to = Vec::new();
    let mut agentless = false;
    let mut steps = Vec::new();
    for child in children.nodes() {
        match child.name().value() {
            "assigned-to" => {
                if assigned_to.is_some() {
                    return Err(St3Error::new(
                        "duplicate-queue-field",
                        format!("queue `{id}` repeats `assigned-to`"),
                    ));
                }
                assigned_to = Some(normalize_assignee(&first_string(child)?, default_host));
            }
            "available-to" => {
                let agent = normalize_assignee(&first_string(child)?, default_host);
                if available_to.contains(&agent) {
                    return Err(St3Error::new(
                        "duplicate-work-agent",
                        format!("queue `{id}` repeats available agent `{agent}`"),
                    ));
                }
                available_to.push(agent);
            }
            "agentless" => {
                ensure_bare(child)?;
                agentless = true;
            }
            "step" => steps.push(parse_step(child, "", default_host, false, input_names)?),
            other => {
                return Err(St3Error::new(
                    "invalid-queue-child",
                    format!("queue `{id}` cannot contain `{other}`"),
                ));
            }
        }
    }
    if steps.is_empty() {
        return Err(St3Error::new(
            "empty-queue",
            format!("queue `{id}` needs at least one step"),
        ));
    }
    let queue_selector = build_work_selector(
        &format!("queue `{id}`"),
        assigned_to,
        available_to,
        agentless,
    )?;
    let mut prior = None::<String>;
    for (index, step) in steps.iter_mut().enumerate() {
        step.queue = Some(id.clone());
        step.queue_position = Some(u32::try_from(index + 1).map_err(internal)?);
        if step.work_selector.is_none() {
            step.work_selector.clone_from(&queue_selector);
        }
        if let Some(prior) = prior.as_deref() {
            let mut saw_completed = false;
            let mut dependencies = Vec::with_capacity(step.dependencies.len() + 1);
            for dependency in std::mem::take(&mut step.dependencies) {
                match &dependency {
                    DependencySpec::Step {
                        step: target,
                        state,
                    } if target == prior => {
                        if state != "completed" {
                            return Err(St3Error::new(
                                "conflicting-queue-dependency",
                                format!(
                                    "queue `{id}` step `{}` requires predecessor `{prior}` to be `{state}`",
                                    step.id
                                ),
                            ));
                        }
                        if !saw_completed {
                            dependencies.push(dependency);
                            saw_completed = true;
                        }
                    }
                    _ => dependencies.push(dependency),
                }
            }
            if !saw_completed {
                dependencies.push(DependencySpec::Step {
                    step: prior.to_owned(),
                    state: "completed".into(),
                });
            }
            step.dependencies = dependencies;
        }
        prior = Some(step.id.clone());
        step.definition_hash.clear();
        step.definition_hash = hash(step)?;
    }
    Ok(steps)
}

fn build_work_selector(
    context: &str,
    assigned_to: Option<String>,
    mut available_to: Vec<String>,
    agentless: bool,
) -> Result<Option<WorkSelector>, St3Error> {
    let selected = usize::from(assigned_to.is_some())
        + usize::from(!available_to.is_empty())
        + usize::from(agentless);
    if selected > 1 {
        return Err(St3Error::new(
            "conflicting-work-selector",
            format!("{context} must use only one of `assigned-to`, `available-to`, or `agentless`"),
        ));
    }
    available_to.sort();
    Ok(if let Some(agent) = assigned_to {
        Some(WorkSelector::Assigned { agent })
    } else if !available_to.is_empty() {
        Some(WorkSelector::Available {
            agents: available_to,
        })
    } else if agentless {
        Some(WorkSelector::Agentless)
    } else {
        None
    })
}

fn parse_mission_input(node: &KdlNode) -> Result<MissionInputSpec, St3Error> {
    reject_type(node)?;
    ensure_only_properties(node, &["kind"])?;
    ensure_no_children(node)?;
    let name = first_string(node)?;
    if name.contains('/') {
        return Err(St3Error::new(
            "invalid-mission-input",
            format!("mission input `{name}` cannot contain `/`"),
        ));
    }
    validate_id(&name, "mission input")?;
    let kind = match property_string(node, "kind")?.as_deref() {
        Some("text") => MissionInputKind::Text,
        Some("resource") => MissionInputKind::Resource,
        Some(value) => {
            return Err(St3Error::new(
                "invalid-mission-input-kind",
                format!("mission input `{name}` has invalid kind `{value}`"),
            ));
        }
        None => {
            return Err(St3Error::new(
                "missing-mission-input-kind",
                format!("mission input `{name}` needs `kind`"),
            ));
        }
    };
    Ok(MissionInputSpec { name, kind })
}

fn parse_concurrent_runs(node: &KdlNode) -> Result<Option<u32>, St3Error> {
    reject_type(node)?;
    ensure_only_properties(node, &["max"])?;
    ensure_no_children(node)?;
    if node.entries().iter().any(|entry| entry.name().is_none()) {
        return Err(St3Error::new(
            "invalid-concurrent-runs",
            "`concurrent-runs` does not accept positional values",
        ));
    }
    let Some(entry) = node.get("max") else {
        return Ok(None);
    };
    let Some(value) = entry.as_integer() else {
        return Err(St3Error::new(
            "invalid-concurrent-runs",
            "`concurrent-runs max` must be a positive integer",
        ));
    };
    let value = u32::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            St3Error::new(
                "invalid-concurrent-runs",
                "`concurrent-runs max` must be a positive integer",
            )
        })?;
    Ok(Some(value))
}

fn parse_completion(node: &KdlNode, default_host: &str) -> Result<CompletionSpec, St3Error> {
    ensure_bare(node)?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("empty-completion", "completion is empty"))?;
    let mut when = None;
    let mut dependencies = None;
    for child in body.nodes() {
        match child.name().value() {
            "when" if when.is_none() => when = Some(plain_string(child)?),
            "depends-on" if dependencies.is_none() => {
                dependencies = Some(parse_dependencies(child, default_host)?)
            }
            "when" | "depends-on" => {
                return Err(St3Error::new(
                    "duplicate-completion-field",
                    format!("completion repeats `{}`", child.name().value()),
                ));
            }
            other => {
                return Err(St3Error::new(
                    "invalid-completion-field",
                    format!("completion cannot contain `{other}`"),
                ));
            }
        }
    }
    match (when, dependencies) {
        (Some(value), None) if value == "all-steps-exhausted" => {
            Ok(CompletionSpec::AllStepsExhausted)
        }
        (Some(value), None) => Err(St3Error::new(
            "invalid-completion-condition",
            format!("completion condition `{value}` is not registered"),
        )),
        (None, Some(dependencies)) if !dependencies.is_empty() => {
            Ok(CompletionSpec::Dependencies { dependencies })
        }
        (Some(_), Some(_)) => Err(St3Error::new(
            "conflicting-completion-condition",
            "completion cannot contain both `when` and `depends-on`",
        )),
        _ => Err(St3Error::new(
            "empty-completion",
            "completion needs `when` or `depends-on`",
        )),
    }
}

fn parse_revision_protection(node: &KdlNode) -> Result<bool, St3Error> {
    match property_string(node, "revisions")?.as_deref() {
        None => Ok(false),
        Some("human-only") => Ok(true),
        Some(value) => Err(St3Error::new(
            "invalid-revision-protection",
            format!("`revisions` cannot be `{value}`"),
        )),
    }
}

fn parse_revision_reviewer(node: &KdlNode, human_only: bool) -> Result<Option<String>, St3Error> {
    let reviewer = property_string(node, "revision-reviewer")?;
    if reviewer.is_some() && !human_only {
        return Err(St3Error::new(
            "revision-reviewer-without-protection",
            "`revision-reviewer` requires revisions=\"human-only\"",
        ));
    }
    if reviewer
        .as_deref()
        .is_some_and(|value| !value.starts_with("person/"))
    {
        return Err(St3Error::new(
            "invalid-revision-reviewer",
            "a revision reviewer must be a full person subject",
        ));
    }
    Ok(reviewer)
}

fn direct_agent_owners(
    document: &KdlDocument,
    default_host: &str,
) -> Result<Vec<String>, St3Error> {
    let mut owners = document
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "agent")
        .map(|node| agent_owner(node, default_host))
        .collect::<Result<Vec<_>, _>>()?;
    owners.sort();
    owners.dedup();
    Ok(owners)
}

fn declarations_document(nodes: Vec<KdlNode>) -> Option<String> {
    if nodes.is_empty() {
        return None;
    }
    let mut document = KdlDocument::new();
    let mut version = KdlNode::new("version");
    version.entries_mut().push(kdl::KdlEntry::new(2));
    document.nodes_mut().push(version);
    document.nodes_mut().extend(nodes);
    document.autoformat();
    Some(document.to_string())
}

fn agent_owner(node: &KdlNode, _default_host: &str) -> Result<String, St3Error> {
    let name = first_string(node)?;
    let children = node.children().ok_or_else(|| {
        St3Error::new("missing-agent-body", format!("agent `{name}` has no body"))
    })?;
    let identity = children
        .nodes()
        .iter()
        .find(|child| child.name().value() == "identity")
        .map(first_string)
        .transpose()?
        .unwrap_or(name);
    Ok(format!("agent/${{ST_MISSION_RUN}}/{identity}"))
}

fn validate_goal_count(context: &str, goals: &[String], required: bool) -> Result<(), St3Error> {
    if (required && goals.is_empty()) || goals.len() > 3 {
        return Err(St3Error::new(
            "invalid-goal-count",
            format!(
                "{context} needs {} through 3 goals",
                if required { 1 } else { 0 }
            ),
        ));
    }
    Ok(())
}

fn push_constraint(
    constraints: &mut Vec<String>,
    node: &KdlNode,
    context: &str,
) -> Result<(), St3Error> {
    let constraint = plain_string(node)?;
    if constraint.is_empty() || constraint.len() > 1_000 {
        return Err(St3Error::new(
            "invalid-constraint",
            format!("a constraint in {context} must contain 1 through 1,000 bytes"),
        ));
    }
    if constraints.contains(&constraint) {
        return Err(St3Error::new(
            "duplicate-constraint",
            format!("{context} repeats constraint `{constraint}`"),
        ));
    }
    constraints.push(constraint);
    Ok(())
}

fn parse_baseline(node: &KdlNode) -> Result<BaselineSpec, St3Error> {
    reject_type(node)?;
    ensure_only_properties(node, &[])?;
    let name = first_string(node)?;
    if name.is_empty() || name.len() > 160 {
        return Err(St3Error::new(
            "invalid-baseline-name",
            "a baseline name must contain 1 through 160 bytes",
        ));
    }
    let body = node.children().ok_or_else(|| {
        St3Error::new(
            "missing-baseline-body",
            format!("baseline `{name}` has no predicates"),
        )
    })?;
    if body.nodes().is_empty() {
        return Err(St3Error::new(
            "empty-baseline",
            format!("baseline `{name}` has no predicates"),
        ));
    }
    let mut gates = Vec::new();
    for (index, predicate) in body.nodes().iter().enumerate() {
        let gate_name = if body.nodes().len() == 1 {
            name.clone()
        } else {
            format!("{name}/{}", index + 1)
        };
        let gate = crate::graph::parse_baseline_gate(predicate, gate_name)?;
        if matches!(
            gate,
            GateSpec::Deadline { .. }
                | GateSpec::Mechanical { .. }
                | GateSpec::Llm { .. }
                | GateSpec::Human { .. }
        ) {
            return Err(St3Error::new(
                "invalid-baseline-gate",
                "a baseline accepts only graph predicates",
            ));
        }
        gates.push(gate);
    }
    Ok(BaselineSpec { name, gates })
}

fn parse_step_document(node: &KdlNode) -> Result<String, St3Error> {
    reject_type(node)?;
    ensure_only_properties(node, &[])?;
    let values = positional_strings(node)?;
    if values.len() != 1 || node.children().is_some() {
        return Err(St3Error::new(
            "invalid-step-document",
            "document needs exactly one immutable document reference",
        ));
    }
    let reference = &values[0];
    let Some((name, revision)) = reference.rsplit_once('@') else {
        return Err(St3Error::new(
            "unpinned-step-document",
            "document needs an exact doc/NAME@HASH reference",
        ));
    };
    if !name.starts_with("doc/")
        || name.len() <= 4
        || name.contains("..")
        || name.ends_with('/')
        || revision.len() != 64
        || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(St3Error::new(
            "invalid-step-document",
            format!("invalid immutable document reference `{reference}`"),
        ));
    }
    Ok(format!("{name}@{}", revision.to_ascii_lowercase()))
}

fn rewrite_nested_paths(mission: &mut MissionSpec, parent: &str) -> Result<(), St3Error> {
    let old = std::mem::take(&mut mission.steps);
    let mut rewritten = BTreeMap::new();
    for (id, mut step) in old {
        step.path = format!("{parent}/{}/{}", mission.id, step.id);
        if let Some(nested) = step.nested_mission.as_mut() {
            rewrite_nested_paths(nested, &step.path)?;
        }
        step.definition_hash = hash(&step)?;
        rewritten.insert(id, step);
    }
    mission.steps = rewritten;
    mission.revision = hash(mission)?;
    Ok(())
}

fn parse_dependencies(
    node: &KdlNode,
    _default_host: &str,
) -> Result<Vec<DependencySpec>, St3Error> {
    reject_type(node)?;
    let mut output = positional_strings(node)?
        .into_iter()
        .map(|step| DependencySpec::Step {
            step,
            state: "completed".into(),
        })
        .collect::<Vec<_>>();
    if let Some(children) = node.children() {
        for child in children.nodes() {
            if child.name().value() == "step" {
                let values = positional_strings(child)?;
                if values.is_empty() || values.len() > 2 {
                    return Err(St3Error::new(
                        "invalid-step-dependency",
                        "a step dependency needs an ID and an optional state",
                    ));
                }
                let state = values.get(1).cloned().unwrap_or_else(|| "completed".into());
                if !matches!(state.as_str(), "completed" | "failed" | "terminal") {
                    return Err(St3Error::new(
                        "invalid-step-dependency-state",
                        format!("step dependency state `{state}` is not registered"),
                    ));
                }
                output.push(DependencySpec::Step {
                    step: values[0].clone(),
                    state,
                });
            } else {
                let parsed = crate::graph::parse_baseline_gate(
                    child,
                    format!("dependency/{}", output.len() + 1),
                )?;
                if matches!(
                    parsed,
                    GateSpec::Deadline { .. }
                        | GateSpec::Mechanical { .. }
                        | GateSpec::Llm { .. }
                        | GateSpec::Human { .. }
                ) {
                    return Err(St3Error::new(
                        "invalid-dependency-predicate",
                        "depends-on accepts only graph predicates and step states",
                    ));
                }
                output.push(DependencySpec::Predicate { gate: parsed });
            }
        }
    }
    if output.is_empty() {
        return Err(St3Error::new(
            "empty-depends-on",
            "depends-on cannot be empty",
        ));
    }
    Ok(output)
}

fn parse_products(node: &KdlNode) -> Result<Vec<ProductSpec>, St3Error> {
    ensure_bare(node)?;
    let children = node.children().ok_or_else(|| {
        St3Error::new(
            "empty-produces",
            "produces must contain at least one graph shape",
        )
    })?;
    let mut output = Vec::new();
    let mut subjects = BTreeSet::new();
    for product in children.nodes() {
        reject_type(product)?;
        let kind = product.name().value();
        if !matches!(kind, "resource" | "message" | "agent" | "exec" | "pty") {
            return Err(St3Error::new(
                "invalid-product-kind",
                format!("produces cannot match `{kind}`"),
            ));
        }
        let subject = normalize_subject(&first_string(product)?, kind);
        if !subjects.insert(subject.clone()) {
            return Err(St3Error::new(
                "duplicate-product",
                format!("produces repeats `{subject}`"),
            ));
        }
        let mut fields = BTreeMap::new();
        if let Some(body) = product.children() {
            for field in body.nodes() {
                reject_type(field)?;
                if field.children().is_some()
                    || field.entries().len() != 1
                    || field.entries()[0].name().is_some()
                {
                    return Err(St3Error::new(
                        "invalid-product-field",
                        format!(
                            "product field `{}` must contain one scalar value",
                            field.name().value()
                        ),
                    ));
                }
                let name = field.name().value().to_owned();
                let value = json_value(field.entries()[0].value())?;
                if fields.insert(name.clone(), value).is_some() {
                    return Err(St3Error::new(
                        "duplicate-product-field",
                        format!("product `{subject}` repeats field `{name}`"),
                    ));
                }
            }
        }
        if kind == "resource" {
            let resource_kind = fields.get("kind").and_then(Value::as_str).ok_or_else(|| {
                St3Error::new(
                    "missing-resource-kind",
                    format!("resource product `{subject}` needs a registered kind"),
                )
            })?;
            let facts = fields
                .iter()
                .filter(|(name, _)| name.as_str() != "kind")
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect::<BTreeMap<_, _>>();
            st3_schema::registry()
                .validate_resource_facts(resource_kind, &facts)
                .map_err(|error| St3Error::new(error.code, error.message))?;
        }
        output.push(ProductSpec { subject, fields });
    }
    Ok(output)
}

fn parse_produced_mission(node: &KdlNode) -> Result<String, St3Error> {
    reject_type(node)?;
    ensure_only_properties(node, &[])?;
    let values = positional_strings(node)?;
    if values.len() != 1 || node.children().is_some() {
        return Err(St3Error::new(
            "invalid-produces-mission",
            "produces-mission needs exactly one mission ID",
        ));
    }
    let id = values[0]
        .strip_prefix("mission/")
        .unwrap_or(&values[0])
        .to_owned();
    validate_id(&id, "mission")?;
    Ok(id)
}

fn parse_used_mission(node: &KdlNode) -> Result<UsedMissionSpec, St3Error> {
    reject_type(node)?;
    ensure_only_properties(node, &["output-of"])?;
    if node.children().is_some() {
        return Err(St3Error::new(
            "invalid-uses-mission",
            "uses-mission cannot contain a block",
        ));
    }
    let values = positional_strings(node)?;
    let output = property_string(node, "output-of")?;
    match (values.as_slice(), output) {
        ([], Some(step)) => {
            validate_id(&step, "step")?;
            Ok(UsedMissionSpec::StepOutput { step })
        }
        ([reference], None) => {
            let reference = reference.strip_prefix("mission/").unwrap_or(reference);
            let (mission, revision) = reference.rsplit_once('@').ok_or_else(|| {
                St3Error::new(
                    "unpinned-mission-reference",
                    "uses-mission needs an exact MISSION@REVISION reference",
                )
            })?;
            validate_id(mission, "mission")?;
            if revision.len() != 64 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(St3Error::new(
                    "invalid-mission-revision",
                    "a used mission revision must be a 64-character hexadecimal hash",
                ));
            }
            Ok(UsedMissionSpec::Revision {
                mission: mission.to_owned(),
                revision: revision.to_ascii_lowercase(),
            })
        }
        _ => Err(St3Error::new(
            "invalid-uses-mission",
            "uses-mission needs one exact mission reference or one output-of property",
        )),
    }
}

fn parse_retry(node: &KdlNode) -> Result<RetrySpec, St3Error> {
    ensure_bare(node)?;
    let body = node
        .children()
        .ok_or_else(|| St3Error::new("empty-retry", "retry is empty"))?;
    let attempts = child_integer(body, "attempts")?.unwrap_or(1);
    if !(1..=100).contains(&attempts) {
        return Err(St3Error::new(
            "invalid-retry-attempts",
            "retry attempts must be between 1 and 100",
        ));
    }
    let backoff_ms = child_string(body, "backoff")?
        .map(|value| parse_duration(&value))
        .transpose()?
        .unwrap_or(0);
    Ok(RetrySpec {
        attempts: attempts as u32,
        backoff_ms,
    })
}

fn validate_dependencies(
    mission: &str,
    steps: &BTreeMap<String, StepSpec>,
) -> Result<(), St3Error> {
    for step in steps.values() {
        validate_dependency_targets(
            mission,
            &format!("step `{}`", step.id),
            &step.dependencies,
            steps,
        )?;
        for dependency in &step.dependencies {
            if let DependencySpec::Step { step: target, .. } = dependency
                && steps[target].finally != step.finally
            {
                return Err(St3Error::new(
                    "cross-phase-dependency",
                    format!(
                        "step `{}` in mission `{mission}` cannot depend on step `{target}` from another phase",
                        step.id
                    ),
                ));
            }
        }
        if step.produces_mission.is_some() && step.uses_mission.is_some() {
            return Err(St3Error::new(
                "conflicting-mission-step",
                format!(
                    "step `{}` in mission `{mission}` cannot produce and use a mission",
                    step.id
                ),
            ));
        }
        if let Some(UsedMissionSpec::StepOutput { step: target }) = &step.uses_mission {
            let Some(producer) = steps.get(target) else {
                return Err(St3Error::new(
                    "unknown-mission-output",
                    format!(
                        "step `{}` in mission `{mission}` uses unknown step output `{target}`",
                        step.id
                    ),
                ));
            };
            if producer.produces_mission.is_none() {
                return Err(St3Error::new(
                    "not-a-mission-output",
                    format!(
                        "step `{}` in mission `{mission}` does not produce a mission",
                        producer.id
                    ),
                ));
            }
            let waits_for_output = step.dependencies.iter().any(|dependency| {
                matches!(dependency, DependencySpec::Step { step, state } if step == target && state == "completed")
            });
            if !waits_for_output {
                return Err(St3Error::new(
                    "missing-mission-output-dependency",
                    format!(
                        "step `{}` must depend on completed step `{target}` before it uses that mission output",
                        step.id
                    ),
                ));
            }
        }
    }
    fn visit(
        id: &str,
        steps: &BTreeMap<String, StepSpec>,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
    ) -> Result<(), St3Error> {
        if visited.contains(id) {
            return Ok(());
        }
        if !visiting.insert(id.to_owned()) {
            return Err(St3Error::new(
                "dependency-cycle",
                format!("the mission has a dependency cycle through step `{id}`"),
            ));
        }
        for dependency in &steps[id].dependencies {
            if let DependencySpec::Step { step, .. } = dependency {
                visit(step, steps, visiting, visited)?;
            }
        }
        visiting.remove(id);
        visited.insert(id.to_owned());
        Ok(())
    }
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for id in steps.keys() {
        visit(id, steps, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn validate_dependency_targets(
    mission: &str,
    context: &str,
    dependencies: &[DependencySpec],
    steps: &BTreeMap<String, StepSpec>,
) -> Result<(), St3Error> {
    for dependency in dependencies {
        if let DependencySpec::Step { step: target, .. } = dependency
            && !steps.contains_key(target)
        {
            return Err(St3Error::new(
                "unknown-step-dependency",
                format!("{context} in mission `{mission}` depends on unknown step `{target}`"),
            ));
        }
    }
    Ok(())
}

pub fn interpolate(source: &str, variables: &BTreeMap<String, String>) -> Result<String, St3Error> {
    let mut output = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let Some(end) = tail.find('}') else {
            return Err(St3Error::new(
                "invalid-variable",
                "a variable reference has no closing brace",
            ));
        };
        let name = &tail[..end];
        if !registered_variable(name) && !name.starts_with("input.") {
            return Err(St3Error::new(
                "unknown-variable",
                format!("variable `{name}` is not registered"),
            ));
        }
        let value = variables
            .get(name)
            .or_else(|| {
                name.starts_with("loop.item.")
                    .then(|| variables.get("loop.item.*"))
                    .flatten()
            })
            .ok_or_else(|| {
                St3Error::new(
                    "unavailable-variable",
                    format!("variable `{name}` is not available in this phase"),
                )
            })?;
        output.push_str(value);
        rest = &tail[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

pub fn interpolate_kdl(
    source: &str,
    variables: &BTreeMap<String, String>,
) -> Result<String, St3Error> {
    fn interpolate_document(
        document: &mut KdlDocument,
        variables: &BTreeMap<String, String>,
    ) -> Result<(), St3Error> {
        for node in document.nodes_mut() {
            for entry in node.entries_mut() {
                let Some(value) = entry.value().as_string() else {
                    continue;
                };
                let value = interpolate(value, variables)?;
                *entry.value_mut() = KdlValue::String(value);
            }
            if let Some(children) = node.children_mut() {
                interpolate_document(children, variables)?;
            }
        }
        Ok(())
    }

    let mut document = source
        .parse::<KdlDocument>()
        .map_err(|error| St3Error::new("invalid-kdl", error.to_string()))?;
    interpolate_document(&mut document, variables)?;
    document.autoformat();
    Ok(document.to_string())
}

fn validate_variables(value: &Value, input_names: &BTreeSet<String>) -> Result<(), St3Error> {
    match value {
        Value::String(value) => {
            let mut rest = value.as_str();
            while let Some(start) = rest.find("${") {
                let tail = &rest[start + 2..];
                let Some(end) = tail.find('}') else {
                    return Err(St3Error::new(
                        "invalid-variable",
                        "a variable reference has no closing brace",
                    ));
                };
                let name = &tail[..end];
                let declared_input = name
                    .strip_prefix("input.")
                    .is_some_and(|name| input_names.contains(name));
                if !registered_variable(name) && !declared_input {
                    return Err(St3Error::new(
                        "unknown-variable",
                        format!("variable `{name}` is not registered"),
                    ));
                }
                rest = &tail[end + 1..];
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_variables(value, input_names)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_variables(value, input_names)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn registered_variable(name: &str) -> bool {
    VARIABLES.contains(&name) || name.starts_with("loop.item.")
}

fn parse_mission_state(value: &str) -> Result<MissionState, St3Error> {
    match value {
        "draft" => Ok(MissionState::Draft),
        "ready" => Ok(MissionState::Ready),
        "retired" => Ok(MissionState::Retired),
        _ => Err(St3Error::new(
            "invalid-mission-state",
            format!("mission state `{value}` is not registered"),
        )),
    }
}

fn normalize_subject(value: &str, kind: &str) -> String {
    if value.starts_with(&format!("{kind}/")) {
        value.to_owned()
    } else {
        format!("{kind}/{value}")
    }
}

fn normalize_assignee(value: &str, default_host: &str) -> String {
    let identity = value.strip_prefix("agent/").unwrap_or(value);
    if identity.contains('.') || identity.contains("${") {
        format!("agent/{identity}")
    } else if identity.contains('/') {
        value.to_owned()
    } else {
        format!("agent/{default_host}.{identity}")
    }
}

fn validate_id(value: &str, kind: &str) -> Result<(), St3Error> {
    let invalid_path = kind != "mission" && value.contains('/');
    if value.is_empty()
        || value.len() > 160
        || invalid_path
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains("//")
        || value.chars().any(char::is_whitespace)
    {
        return Err(St3Error::new(
            "invalid-mission-id",
            format!("{kind} ID `{value}` is invalid"),
        ));
    }
    Ok(())
}

fn parse_duration(value: &str) -> Result<u64, St3Error> {
    let units = [
        ("ms", 1_u64),
        ("s", 1_000),
        ("m", 60_000),
        ("h", 3_600_000),
        ("d", 86_400_000),
    ];
    for (suffix, multiplier) in units {
        if let Some(number) = value.strip_suffix(suffix) {
            let number = number.parse::<u64>().map_err(|_| {
                St3Error::new("invalid-duration", format!("duration `{value}` is invalid"))
            })?;
            return number
                .checked_mul(multiplier)
                .ok_or_else(|| St3Error::new("invalid-duration", "the duration is too large"));
        }
    }
    Err(St3Error::new(
        "invalid-duration",
        format!("duration `{value}` needs ms, s, m, h, or d"),
    ))
}

fn hash(value: &impl Serialize) -> Result<String, St3Error> {
    let bytes = serde_json::to_vec(value).map_err(internal)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn internal(error: impl std::fmt::Display) -> St3Error {
    St3Error::new("internal", error.to_string())
}

fn reject_type(node: &KdlNode) -> Result<(), St3Error> {
    if node.ty().is_some() {
        Err(St3Error::new(
            "typed-node",
            "typed KDL nodes are not supported",
        ))
    } else {
        Ok(())
    }
}

fn ensure_bare(node: &KdlNode) -> Result<(), St3Error> {
    if node.ty().is_none() && node.entries().is_empty() {
        Ok(())
    } else {
        Err(St3Error::new(
            "invalid-node",
            format!(
                "`{}` must not have values or properties",
                node.name().value()
            ),
        ))
    }
}

fn ensure_no_children(node: &KdlNode) -> Result<(), St3Error> {
    if node.children().is_some() {
        Err(St3Error::new(
            "invalid-node",
            format!("`{}` must not have children", node.name().value()),
        ))
    } else {
        Ok(())
    }
}

fn ensure_only_properties(node: &KdlNode, allowed: &[&str]) -> Result<(), St3Error> {
    for entry in node.entries() {
        if let Some(name) = entry.name()
            && !allowed.contains(&name.value())
        {
            return Err(St3Error::new(
                "unknown-property",
                format!(
                    "`{}` has unknown property `{}`",
                    node.name().value(),
                    name.value()
                ),
            ));
        }
    }
    Ok(())
}

fn first_string(node: &KdlNode) -> Result<String, St3Error> {
    let values = positional_strings(node)?;
    if values.len() != 1 {
        return Err(St3Error::new(
            "invalid-value-count",
            format!("`{}` needs one string", node.name().value()),
        ));
    }
    Ok(values[0].clone())
}

fn plain_string(node: &KdlNode) -> Result<String, St3Error> {
    reject_type(node)?;
    ensure_only_properties(node, &[])?;
    if node.children().is_some() {
        return Err(St3Error::new(
            "invalid-node",
            format!("`{}` cannot contain a block", node.name().value()),
        ));
    }
    first_string(node)
}

fn positional_strings(node: &KdlNode) -> Result<Vec<String>, St3Error> {
    node.entries()
        .iter()
        .filter(|entry| entry.name().is_none())
        .map(|entry| match entry.value() {
            KdlValue::String(value) => Ok(value.clone()),
            _ => Err(St3Error::new(
                "invalid-value",
                format!("`{}` needs string values", node.name().value()),
            )),
        })
        .collect()
}

fn property_string(node: &KdlNode, name: &str) -> Result<Option<String>, St3Error> {
    match node.get(name) {
        None => Ok(None),
        Some(KdlValue::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(St3Error::new(
            "invalid-property",
            format!("property `{name}` must be a string"),
        )),
    }
}

fn property_integer(node: &KdlNode, name: &str) -> Result<Option<i64>, St3Error> {
    match node.get(name) {
        None => Ok(None),
        Some(KdlValue::Integer(value)) => i64::try_from(*value).map(Some).map_err(|_| {
            St3Error::new(
                "invalid-property",
                format!("property `{name}` is too large"),
            )
        }),
        Some(_) => Err(St3Error::new(
            "invalid-property",
            format!("property `{name}` must be an integer"),
        )),
    }
}

fn property_number(node: &KdlNode, name: &str) -> Result<Option<f64>, St3Error> {
    match node.get(name) {
        None => Ok(None),
        Some(KdlValue::Float(value)) => Ok(Some(*value)),
        Some(KdlValue::Integer(value)) => Ok(Some(*value as f64)),
        Some(_) => Err(St3Error::new(
            "invalid-property",
            format!("property `{name}` must be a number"),
        )),
    }
}

fn child_integer_value(node: &KdlNode) -> Result<i64, St3Error> {
    reject_type(node)?;
    ensure_only_properties(node, &[])?;
    ensure_no_children(node)?;
    if node.entries().len() != 1 {
        return Err(St3Error::new(
            "invalid-field",
            format!("`{}` needs one integer", node.name().value()),
        ));
    }
    match node.entries()[0].value() {
        KdlValue::Integer(value) => i64::try_from(*value)
            .map_err(|_| St3Error::new("invalid-field", "an integer is too large")),
        _ => Err(St3Error::new(
            "invalid-field",
            format!("`{}` needs one integer", node.name().value()),
        )),
    }
}

fn placement_host_for_mission(host: String, default_host: &str) -> String {
    if host == "local" {
        default_host.into()
    } else {
        host
    }
}

fn parse_string_map(
    document: &KdlDocument,
    name: &str,
) -> Result<BTreeMap<String, String>, St3Error> {
    let Some(node) = unique_named_child(document, name)? else {
        return Ok(BTreeMap::new());
    };
    ensure_bare(node)?;
    let Some(children) = node.children() else {
        return Ok(BTreeMap::new());
    };
    let mut output = BTreeMap::new();
    for child in children.nodes() {
        let key = child.name().value().to_owned();
        let value = first_string(child)?;
        if output.insert(key.clone(), value).is_some() {
            return Err(St3Error::new(
                "duplicate-map-key",
                format!("map key `{key}` repeats"),
            ));
        }
    }
    Ok(output)
}

fn child_string(document: &KdlDocument, name: &str) -> Result<Option<String>, St3Error> {
    let nodes = document
        .nodes()
        .iter()
        .filter(|node| node.name().value() == name)
        .collect::<Vec<_>>();
    if nodes.len() > 1 {
        return Err(St3Error::new(
            "duplicate-field",
            format!("`{name}` repeats"),
        ));
    }
    nodes.first().map(|node| first_string(node)).transpose()
}

fn child_integer(document: &KdlDocument, name: &str) -> Result<Option<i64>, St3Error> {
    let nodes = document
        .nodes()
        .iter()
        .filter(|node| node.name().value() == name)
        .collect::<Vec<_>>();
    if nodes.len() > 1 {
        return Err(St3Error::new(
            "duplicate-field",
            format!("`{name}` repeats"),
        ));
    }
    let Some(node) = nodes.first() else {
        return Ok(None);
    };
    if node.entries().len() != 1 {
        return Err(St3Error::new(
            "invalid-field",
            format!("`{name}` needs one integer"),
        ));
    }
    match node.entries()[0].value() {
        KdlValue::Integer(value) => i64::try_from(*value)
            .map(Some)
            .map_err(|_| St3Error::new("invalid-field", format!("`{name}` is too large"))),
        _ => Err(St3Error::new(
            "invalid-field",
            format!("`{name}` needs one integer"),
        )),
    }
}

fn json_value(value: &KdlValue) -> Result<Value, St3Error> {
    match value {
        KdlValue::String(value) => Ok(Value::String(value.clone())),
        KdlValue::Bool(value) => Ok(Value::Bool(*value)),
        KdlValue::Integer(value) => i64::try_from(*value)
            .map(Value::from)
            .map_err(|_| St3Error::new("invalid-number", "an integer is too large")),
        KdlValue::Float(value) => serde_json::Number::from_f64(*value)
            .map(Value::Number)
            .ok_or_else(|| St3Error::new("invalid-number", "a float must be finite")),
        KdlValue::Null => Ok(Value::Null),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn quantified_fields_are_available_to_gates_loops_and_dependencies() {
        let source = r#"
version 2

resource "pull" { kind "vcs.pull-request" }
mission "quantified" state="ready" {
  goal "Use observed check lists declaratively."
  gate "all checks passed" {
    every "checks" "resource/pull" {
      field "status" "is" "completed"
      field "conclusion" "is" "success"
    }
  }
  loop "wait" {
    max-rounds 2
    until {
      gate "checks settled" {
        every "checks" "resource/pull" {
          field "status" "is" "completed"
        }
      }
    }
    round { completion { when "all-steps-exhausted" } }
  }
  step "report-failure" {
    agentless
    depends-on {
      not-every "checks" "resource/pull" {
        field "conclusion" "is" "success"
      }
    }
  }
}
"#;
        let intent = crate::graph::parse_intent(source, "node").unwrap();
        let mission = &intent.missions["quantified"];
        assert!(matches!(
            &mission.gates[0],
            crate::model::GateSpec::Every { path, subject, fields, .. }
                if path == "checks" && subject == "resource/pull" && fields.len() == 2
        ));
        assert!(matches!(
            &mission.steps["wait"].loop_spec.as_ref().unwrap().until[0],
            crate::model::GateSpec::Every { fields, .. } if fields.len() == 1
        ));
        assert!(matches!(
            &mission.steps["report-failure"].dependencies[0],
            crate::model::DependencySpec::Predicate {
                gate: crate::model::GateSpec::NotEvery { fields, .. }
            } if fields.len() == 1
        ));

        for (predicate, code) in [
            (
                r#"every "checks" "resource/pull""#,
                "empty-quantified-predicate",
            ),
            (
                r#"every "checks" "resource/pull" { exists "resource/other" }"#,
                "invalid-quantified-predicate",
            ),
            (
                r#"not-every "checks" "resource/pull" { field "status" "is" }"#,
                "invalid-quantified-field",
            ),
        ] {
            let source = format!(
                "version 2\nmission \"bad\" state=\"ready\" {{ goal \"Reject a malformed quantifier.\"; gate \"bad\" {{ {predicate} }} }}"
            );
            let error = crate::graph::parse_intent(&source, "node").unwrap_err();
            assert_eq!(error.code, code, "{predicate}");
        }
    }

    #[test]
    fn first_class_loops_have_bounded_child_mission_rounds() {
        let source = r#"
version 2

mission "loop" state="ready" {
  goal "Repeat a graph until its gate passes."
  completion { when "all-steps-exhausted" }
  loop "improve" timeout="2h" {
    max-rounds 4
    until { gate "the result is ready" { exists "resource/result" } }
    round {
      completion { when "all-steps-exhausted" }
      step "work" { agentless }
    }
  }
}
"#;
        let intent = crate::graph::parse_intent(source, "node").unwrap();
        let step = &intent.missions["loop"].steps["improve"];
        let retry = &step.retry;
        assert_eq!(retry.attempts, 4);
        let loop_spec = step.loop_spec.as_ref().unwrap();
        assert_eq!(loop_spec.max_rounds, 4);
        assert_eq!(loop_spec.timeout_ms, Some(7_200_000));
        assert_eq!(loop_spec.round.timeout_ms, Some(7_200_000));
        assert_eq!(loop_spec.round.steps.len(), 1);
        assert!(intent.missions.contains_key(&loop_spec.round.id));

        let source = r#"
version 2
mission "alert-loop" state="ready" {
  goal "Alert a person after bounded failure."
  loop "review" {
    max-rounds 3
    round { completion { when "all-steps-exhausted" } }
    on-exhausted {
      fail
      attention "Automatic review failed" {
        reviewer "person/nathan"
        severity "error"
      }
    }
  }
}
"#;
        let intent = crate::graph::parse_intent(source, "node").unwrap();
        let loop_spec = intent.missions["alert-loop"].steps["review"]
            .loop_spec
            .as_ref()
            .unwrap();
        let attention = loop_spec.exhaustion_attention.as_ref().unwrap();
        assert_eq!(attention.title, "Automatic review failed");
        assert_eq!(attention.reviewer, "person/nathan");
        assert_eq!(attention.severity, "error");

        let source = r#"
version 2
mission "eval-loop" state="ready" timeout="20m" {
  goal "Bound every generated eval mission."
  loop "work" {
    max-rounds 2
    round { completion { when "all-steps-exhausted" } }
  }
}
"#;
        let intent = crate::graph::parse_intent(source, "node").unwrap();
        let loop_spec = intent.missions["eval-loop"].steps["work"]
            .loop_spec
            .as_ref()
            .unwrap();
        assert_eq!(loop_spec.round.timeout_ms, Some(1_200_000));

        for (source, code) in [
            (
                r#"version 2
mission "bad" state="ready" {
  goal "Reject an unbounded loop."
  loop "work" { round { completion { when "all-steps-exhausted" } } }
}"#,
                "missing-loop-rounds",
            ),
            (
                r#"version 2
mission "bad" state="ready" {
  goal "Reject a zero-round loop."
  loop "work" { max-rounds 0; round { completion { when "all-steps-exhausted" } } }
}"#,
                "invalid-loop-rounds",
            ),
            (
                r#"version 2
mission "bad" state="ready" {
  goal "Reject a loop without a round completion."
  loop "work" { max-rounds 2; round { step "one" { agentless } } }
}"#,
                "missing-round-completion",
            ),
            (
                r#"version 2
mission "bad" state="ready" {
  goal "Reject the removed step loop."
  step "work" { loop { max-rounds 2 } }
}"#,
                "loop-is-graph-node",
            ),
            (
                r#"version 2
mission "bad" state="ready" {
  goal "Reject attention after success."
  loop "work" {
    max-rounds 2
    round { completion { when "all-steps-exhausted" } }
    on-exhausted { succeed; attention "Wrong" { reviewer "person/nathan" } }
  }
}"#,
                "invalid-loop-exhaustion-attention",
            ),
        ] {
            assert_eq!(
                crate::graph::parse_intent(source, "node").unwrap_err().code,
                code
            );
        }
    }

    #[test]
    fn loop_collection_modes_are_removed_from_the_loop_grammar() {
        for source in [
            r#"version 2
mission "bad" state="ready" { goal "Reject for-each."; loop "items" for-each="resource/items" field="values" { max-rounds 2; round { step "read" { agentless } } } }"#,
            r#"version 2
mission "bad" state="ready" { goal "Reject best-of-N."; loop "choose" { max-rounds 1; candidates 3 { select metric="quality" }; round { step "create" { agentless } } } }"#,
        ] {
            assert_eq!(
                crate::graph::parse_intent(source, "node").unwrap_err().code,
                "removed-loop-shape"
            );
        }
    }

    #[test]
    fn parses_parallel_steps_nested_work_and_products() {
        let source = r#"
version 2

  mission "demo" state="ready" {
    goal "Complete mission demo."
    step "start" {
      agentless

        agent "worker" {
          workspace "."
          harness "codex" { prompt "Run durable work." }
        }

    }
    step "one" {
      assigned-to "agent/${ST_MISSION_RUN}/worker"
      depends-on { step "start" completed }
      mission "work" { goal "Complete mission work."; step "inspect" { } }
      produces {
        resource "mission-run/${ST_MISSION_RUN}/change" { kind "vcs.commit"; state "published" }
      }
    }
    step "two" { agentless; depends-on { step "start" completed } }
    step "join" { agentless; depends-on { step "one" completed; step "two" completed } }
    completion { when "all-steps-exhausted" }
    finally {
      step "cleanup" {
        agentless
         stop "agent/${ST_MISSION_RUN}/worker"
      }
    }
  }

"#;
        let intent = crate::graph::parse_intent(source, "node").unwrap();
        let mission = &intent.missions["demo"];
        assert_eq!(mission.steps.len(), 5);
        assert_eq!(mission.max_active_runs, Some(1));
        assert!(mission.steps["one"].nested_mission.is_some());
        assert_eq!(
            mission.steps["one"].products[0].subject,
            "resource/mission-run/${ST_MISSION_RUN}/change"
        );
    }

    #[test]
    fn rejects_checkpoint_and_dependency_cycles() {
        let old =
            crate::graph::parse_intent("version 2\n checkpoints \"old\" { } ", "node").unwrap_err();
        assert_eq!(old.code, "unknown-node");
        let cycle = crate::graph::parse_intent(
            "version 2\n\n  mission \"cycle\" state=\"ready\" {\n    goal \"The cycle is rejected.\"\n    step \"a\" { depends-on \"b\" }\n    step \"b\" { depends-on \"a\" }\n  }\n\n",
            "node",
        )
        .unwrap_err();
        assert_eq!(cycle.code, "dependency-cycle");
    }

    #[test]
    fn parses_a_human_review_contract() {
        let intent = crate::graph::parse_intent(
            r#"
version 2

  mission "review" state="ready" {
    goal "Complete mission review."
    step "approval" {
      gate "human-review" type="human" {
        reviewer "person/nathan"
        question "Is this change ready to merge?"
        review "resource/mission-run/${ST_MISSION_RUN}/pull-request"
        review "doc/reports/run@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      }
    }
  }

"#,
            "node",
        )
        .unwrap();
        let gate = &intent.missions["review"].steps["approval"].gates[0];
        let crate::model::GateSpec::Human {
            reviewer,
            question,
            review_targets,
            ..
        } = gate
        else {
            panic!("the gate is not human");
        };
        assert_eq!(reviewer, "person/nathan");
        assert_eq!(question.as_deref(), Some("Is this change ready to merge?"));
        assert_eq!(
            review_targets,
            &[
                "resource/mission-run/${ST_MISSION_RUN}/pull-request",
                "doc/reports/run@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            ]
        );
    }

    #[test]
    fn parses_attempt_bound_mission_production_and_exact_mission_use() {
        let intent = crate::graph::parse_intent(
            &format!(
                r#"
version 2

  mission "bootstrap" state="ready" {{
    goal "Complete mission bootstrap."
    step "compile" {{
      document "doc/project/mission@{}"
      produces-mission "project/work"
    }}
    step "execute" {{
      depends-on {{ step "compile" completed }}
      uses-mission output-of="compile"
    }}
    step "reuse" {{
      uses-mission "project/work@{}"
    }}
  }}

"#,
                "b".repeat(64),
                "a".repeat(64)
            ),
            "node",
        )
        .unwrap();
        let mission = &intent.missions["bootstrap"];
        assert_eq!(
            mission.steps["compile"].produces_mission.as_deref(),
            Some("project/work")
        );
        assert_eq!(
            mission.steps["compile"].documents,
            vec![format!("doc/project/mission@{}", "b".repeat(64))]
        );
        assert_eq!(
            mission.steps["execute"].uses_mission,
            Some(crate::model::UsedMissionSpec::StepOutput {
                step: "compile".into()
            })
        );
        assert_eq!(
            mission.steps["reuse"].uses_mission,
            Some(crate::model::UsedMissionSpec::Revision {
                mission: "project/work".into(),
                revision: "a".repeat(64),
            })
        );
    }

    #[test]
    fn rejects_unpinned_or_unordered_mission_use() {
        let unpinned = crate::graph::parse_intent(
            r#"version 2
 mission "bad" state="ready" { goal "Use a mission."; step "use" { uses-mission "work" } } "#,
            "node",
        )
        .unwrap_err();
        assert_eq!(unpinned.code, "unpinned-mission-reference");

        let unpinned_document = crate::graph::parse_intent(
            r#"version 2
 mission "bad" state="ready" { goal "Use a document."; step "use" { document "doc/project/mission" } } "#,
            "node",
        )
        .unwrap_err();
        assert_eq!(unpinned_document.code, "unpinned-step-document");

        let unordered = crate::graph::parse_intent(
            r#"version 2

  mission "bad" state="ready" {
    goal "Complete mission bad."
    step "compile" { produces-mission "work" }
    step "use" { uses-mission output-of="compile" }
  }
"#,
            "node",
        )
        .unwrap_err();
        assert_eq!(unordered.code, "missing-mission-output-dependency");
    }

    #[test]
    fn mission_and_step_contracts_accept_the_new_flat_language() {
        let intent = crate::graph::parse_intent(
            r#"
version 2

  mission "release" state="ready" {
    goal "Publish the release."
    goal "Keep the workspace clean."
    baseline "release is open" { field "state" "resource/release" is "open" }
    produces { resource "release" { kind "custom.test.release"; state "published" } }
    gate "release is approved" { field "approval" "resource/release" is "yes" }
    step "build" {
      goal "Build the artifact."
      goal "Record the checksum."
      baseline "source exists" { exists "resource/source" }
      produces { resource "artifact" { kind "custom.test.artifact"; state "published" } }
      gate "artifact is valid" { field "valid" "resource/artifact" is #true }
    }

    step "publish" { depends-on { step "build" completed } }
  }

"#,
            "node",
        )
        .unwrap();
        let mission = &intent.missions["release"];
        assert_eq!(mission.goals.len(), 2);
        assert_eq!(mission.baselines.len(), 1);
        assert_eq!(mission.products.len(), 1);
        assert_eq!(mission.gates.len(), 1);
        assert_eq!(mission.steps["build"].goals.len(), 2);
        assert_eq!(mission.steps["build"].baselines.len(), 1);
        assert_eq!(mission.steps["build"].products.len(), 1);
        assert_eq!(mission.steps["build"].gates.len(), 1);
        assert!(mission.steps["publish"].goals.is_empty());

        let unknown_product = crate::graph::parse_intent(
            r#"version 2

  mission "bad-product" state="ready" {
    goal "Publish one invalid resource."
    produces { resource "result" { kind "document.result"; state "published" } }
  }
"#,
            "node",
        )
        .unwrap_err();
        assert_eq!(unknown_product.code, "unknown-resource-kind");

        let duplicate_field = crate::graph::parse_intent(
            r#"version 2

  mission "duplicate-product-field" state="ready" {
    goal "Reject an ambiguous product."
    produces {
      resource "result" {
        kind "custom.test.result"
        state "ready"
        state "published"
      }
    }
  }
"#,
            "node",
        )
        .unwrap_err();
        assert_eq!(duplicate_field.code, "duplicate-product-field");
    }

    #[test]
    fn constraints_repeat_at_each_level_and_reject_empty_or_duplicate_text() {
        let intent = crate::graph::parse_intent(
            r#"version 2
 mission "safe" state="ready" {
   goal "Do the work."
   constraint "Do not push."
   constraint "Keep private data outside Git."
   step "inspect" {
     constraint "Do not edit files."
     constraint "Report only sanitized findings."
   }
 }"#,
            "node",
        )
        .unwrap();
        assert_eq!(
            intent.missions["safe"].constraints,
            ["Do not push.", "Keep private data outside Git."]
        );
        assert_eq!(
            intent.missions["safe"].steps["inspect"].constraints,
            ["Do not edit files.", "Report only sanitized findings."]
        );

        for (source, code) in [
            (
                r#"version 2
 mission "empty" state="ready" { goal "Reject empty text."; constraint "" }"#,
                "invalid-constraint",
            ),
            (
                r#"version 2
 mission "duplicate" state="ready" {
   goal "Reject duplicate text."
   constraint "Do not push."
   constraint "Do not push."
 }"#,
                "duplicate-constraint",
            ),
            (
                r#"version 2
 mission "duplicate-step" state="ready" {
   goal "Reject duplicate step text."
   step "work" {
     constraint "Do not push."
     constraint "Do not push."
   }
 }"#,
                "duplicate-constraint",
            ),
        ] {
            assert_eq!(
                crate::graph::parse_intent(source, "node").unwrap_err().code,
                code
            );
        }
    }

    #[test]
    fn selectors_completion_and_finally_use_the_explicit_language() {
        let intent = crate::graph::parse_intent(
            r#"
version 2

  mission "pool" state="ready" {
    goal "Complete the pool work."
    available-to "agent/node.one"
    available-to "agent/node.two"
    completion { depends-on { step "assigned" completed } }
    step "inherited" { }
    step "assigned" {
      assigned-to "agent/node.one"
      depends-on { step "inherited" completed }
    }
    finally { step "cleanup" { agentless } }
  }

"#,
            "node",
        )
        .unwrap();
        let mission = &intent.missions["pool"];
        assert_eq!(
            mission.work_selector,
            Some(crate::model::WorkSelector::Available {
                agents: vec!["agent/node.one".into(), "agent/node.two".into()]
            })
        );
        assert_eq!(mission.steps["inherited"].work_selector, None);
        assert_eq!(
            mission.steps["assigned"].work_selector,
            Some(crate::model::WorkSelector::Assigned {
                agent: "agent/node.one".into()
            })
        );
        assert!(mission.steps["cleanup"].finally);
        assert_eq!(
            mission.steps["cleanup"].work_selector,
            Some(crate::model::WorkSelector::Agentless)
        );
        assert!(matches!(
            mission.completion,
            Some(crate::model::CompletionSpec::Dependencies { .. })
        ));

        for source in [
            r#"version 2
 mission "bad" state="ready" { goal "Reject selectors."; assigned-to "agent/node.one"; agentless; step "work" { } } "#,
            r#"version 2
 mission "bad" state="ready" { goal "Reject selectors."; available-to "agent/node.one"; available-to "agent/node.one"; step "work" { } } "#,
            r#"version 2
 mission "bad" state="ready" { goal "Reject completion."; completion { when "all-steps-exhausted"; depends-on { step "work" completed } }; step "work" { } } "#,
            r#"version 2
 mission "bad" state="ready" { goal "Reject a final completion dependency."; completion { depends-on { step "cleanup" completed } }; finally { step "cleanup" { } } } "#,
            r#"version 2
 mission "bad" state="ready" { goal "Reject a cross-phase dependency."; step "work" { }; finally { step "cleanup" { depends-on { step "work" completed } } } } "#,
        ] {
            let invalid = crate::graph::parse_intent(source, "node").and_then(|intent| {
                crate::graph::parse_execution_intent(
                    intent.missions["bad"].declarations_kdl.as_deref().unwrap(),
                    "node",
                    "bad",
                )
            });
            assert!(invalid.is_err());
        }
    }

    #[test]
    fn queues_retain_order_and_apply_selector_precedence() {
        let intent = crate::graph::parse_intent(
            r#"version 2
mission "queued" state="ready" {
  goal "Complete the queue."
  assigned-to "mission-worker"
  step "before" { agentless }
  queue "investigations" {
    available-to "pool.one"
    available-to "pool.two"
    step "first" { }
    step "second" {
      depends-on { step "first" completed; step "first" completed; step "before" completed }
    }
    step "third" {
      assigned-to "agent/external/run/worker"
    }
  }
  step "after" { }
}"#,
            "node",
        )
        .unwrap();
        let mission = &intent.missions["queued"];
        assert_eq!(
            mission.display_order,
            ["before", "first", "second", "third", "after"]
        );
        assert_eq!(
            mission.steps["first"].queue.as_deref(),
            Some("investigations")
        );
        assert_eq!(mission.steps["first"].queue_position, Some(1));
        assert_eq!(mission.steps["second"].queue_position, Some(2));
        assert_eq!(mission.steps["third"].queue_position, Some(3));
        assert_eq!(
            mission.steps["first"].work_selector,
            Some(crate::model::WorkSelector::Available {
                agents: vec!["agent/pool.one".into(), "agent/pool.two".into()]
            })
        );
        assert_eq!(
            mission.steps["third"].work_selector,
            Some(crate::model::WorkSelector::Assigned {
                agent: "agent/external/run/worker".into()
            })
        );
        assert_eq!(
            mission.steps["second"]
                .dependencies
                .iter()
                .filter(|dependency| matches!(
                    dependency,
                    crate::model::DependencySpec::Step { step, state }
                        if step == "first" && state == "completed"
                ))
                .count(),
            1
        );
        assert!(
            mission.steps["second"]
                .dependencies
                .iter()
                .any(|dependency| matches!(
                    dependency,
                    crate::model::DependencySpec::Step { step, state }
                        if step == "before" && state == "completed"
                ))
        );
        assert!(mission.steps["after"].queue.is_none());
        assert!(mission.steps["first"].definition_hash != mission.steps["second"].definition_hash);
    }

    #[test]
    fn queues_reject_ambiguous_or_unsupported_shapes() {
        for (source, code) in [
            (
                r#"version 2
mission "bad" state="ready" { goal "Reject an empty queue."; queue "empty" { agentless } }"#,
                "empty-queue",
            ),
            (
                r#"version 2
mission "bad" state="ready" { goal "Reject queue selector conflict."; queue "q" { agentless; assigned-to "one"; step "work" { } } }"#,
                "conflicting-work-selector",
            ),
            (
                r#"version 2
mission "bad" state="ready" { goal "Reject nested queues."; queue "outer" { queue "inner" { step "work" { } } } }"#,
                "invalid-queue-child",
            ),
            (
                r#"version 2
mission "bad" state="ready" { goal "Reject a final queue."; finally { queue "q" { step "work" { } } } }"#,
                "invalid-finally-child",
            ),
            (
                r#"version 2
mission "bad" state="ready" { goal "Reject duplicate steps."; step "same" { }; queue "q" { step "same" { } } }"#,
                "duplicate-step",
            ),
            (
                r#"version 2
mission "bad" state="ready" { goal "Reject a conflicting queue dependency."; queue "q" { step "one" { }; step "two" { depends-on { step "one" failed } } } }"#,
                "conflicting-queue-dependency",
            ),
        ] {
            assert_eq!(
                crate::graph::parse_intent(source, "node").unwrap_err().code,
                code
            );
        }
    }

    #[test]
    fn agent_mission_authority_uses_exact_and_terminal_namespace_rules() {
        let intent = crate::graph::parse_intent(
            r#"version 2
mission "standing" state="ready" {
  goal "Keep the planner available."
  agent "planner" {
    workspace "."
    command "true"
    mission-authority {
      publish "project/generated"
      publish "fleet/fabric/*"
      start "fleet/fabric/*"
      revise "fleet/fabric/queue"
    }
  }
}"#,
            "node",
        )
        .unwrap();
        let runtime = crate::graph::parse_execution_intent(
            intent.missions["standing"]
                .declarations_kdl
                .as_deref()
                .unwrap(),
            "node",
            "standing",
        )
        .unwrap();
        let desired = &runtime.subjects["agent/standing/planner"].desired;
        let authority = crate::graph::agent_mission_authority(desired);
        assert!(authority.allows("publish", "project/generated"));
        assert!(authority.allows("publish", "fleet/fabric/cycle"));
        assert!(!authority.allows("publish", "fleet/fabrics/cycle"));
        assert!(authority.allows("start", "fleet/fabric/cycle"));
        assert!(authority.allows("revise", "fleet/fabric/queue"));
        assert!(!authority.allows("revise", "fleet/fabric/other"));

        for source in [
            r#"version 2
mission "bad" state="ready" { goal "Reject empty authority."; agent "bad" { workspace "."; command "true"; mission-authority { } } }"#,
            r#"version 2
mission "bad" state="ready" { goal "Reject a prefixed mission."; agent "bad" { workspace "."; command "true"; mission-authority { publish "mission/work" } } }"#,
            r#"version 2
mission "bad" state="ready" { goal "Reject an internal wildcard."; agent "bad" { workspace "."; command "true"; mission-authority { publish "work/*/bad" } } }"#,
            r#"version 2
mission "bad" state="ready" { goal "Reject a duplicate rule."; agent "bad" { workspace "."; command "true"; mission-authority { publish "work/*"; publish "work/*" } } }"#,
        ] {
            assert!(crate::graph::parse_intent(source, "node").is_err());
        }
    }

    #[test]
    fn nested_declarations_are_fully_validated_before_publication() {
        let declarations = [
            r#"agent "worker" { command "true"; unexpected "value" }"#,
            r#"exec "worker" { command "true"; unexpected "value" }"#,
            r#"pty "worker" { command "true"; unexpected "value" }"#,
            r#"host "node" { unexpected "value" }"#,
            r#"doc "proof" { hash "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"; unexpected "value" }"#,
            r#"resource "source" { kind "vcs.repository"; unexpected "value" }"#,
            r#"observer "source" { resource "resource/source"; provider "local.file"; locator "/tmp/source"; field "state"; unexpected "value" }"#,
            r#"subscription "source" { observer "observer/source"; to "agent/worker"; on "state"; delivery "message"; unexpected "value" }"#,
            r#"person "reviewer" unexpected=#true"#,
            r#"message "review" { to "agent/worker"; subject "wrong"; body "wrong" }"#,
            r#"schedule "daily" { every "1d"; work { mission "mission/work@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"; workspace "/tmp/work" }; unexpected "value" }"#,
            r#"stop "agent/worker" unexpected=#true"#,
        ];

        for declaration in declarations {
            let source = format!(
                "version 2\nmission \"bad\" state=\"ready\" {{ goal \"Reject an invalid nested declaration.\"; step \"work\" {{ agentless; {declaration} }} }}"
            );
            let error = crate::graph::parse_intent(&source, "node")
                .expect_err("a nested declaration must use its full validator");
            assert!(
                matches!(
                    error.code,
                    "unknown-child" | "unknown-property" | "invalid-host-child"
                ),
                "{declaration}: {error:?}"
            );
        }
    }

    #[test]
    fn nested_validation_uses_typed_input_placeholders() {
        crate::graph::parse_intent(
            r#"version 2
mission "observed" state="ready" {
  input "source" kind="resource"
  goal "Observe the supplied resource."
  step "observe" {
    agentless
    observer "source" {
      resource "${input.source}"
      provider "local.file"
      locator "/tmp/source"
      field "state"
    }
  }
}"#,
            "node",
        )
        .expect("a resource input must remain valid during nested declaration validation");
    }

    #[test]
    fn a_zero_step_mission_is_valid_and_completes_by_default() {
        let intent = crate::graph::parse_intent(
            r#"version 2
 mission "standing" state="ready" { goal "Keep the agent available." } "#,
            "node",
        )
        .unwrap();
        let mission = &intent.missions["standing"];
        assert!(mission.steps.is_empty());
        assert_eq!(
            mission.completion,
            Some(crate::model::CompletionSpec::AllStepsExhausted)
        );
    }

    #[test]
    fn a_mission_timeout_is_a_positive_run_deadline() {
        let intent = crate::graph::parse_intent(
            r#"version 2
 mission "bounded" state="ready" timeout="20m" {
   goal "Finish before the run deadline."
 } "#,
            "node",
        )
        .unwrap();
        assert_eq!(intent.missions["bounded"].timeout_ms, Some(1_200_000));

        let error = crate::graph::parse_intent(
            r#"version 2
 mission "unbounded" state="ready" timeout="0ms" {
   goal "Reject a disabled deadline."
 } "#,
            "node",
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid-mission-timeout");
    }

    #[test]
    fn goal_limits_and_removed_mission_language_are_strict() {
        for source in [
            r#"version 2
 mission "none" state="ready" { step "work" { } } "#,
            r#"version 2
 mission "four" state="ready" { goal "1"; goal "2"; goal "3"; goal "4"; step "work" { } } "#,
            r#"version 2
 mission "step-four" state="ready" { goal "Run."; step "work" { goal "1"; goal "2"; goal "3"; goal "4" } } "#,
        ] {
            assert_eq!(
                crate::graph::parse_intent(source, "node").unwrap_err().code,
                "invalid-goal-count"
            );
        }
        for removed in [
            r#"version 2
 mission "old" state="ready" { goal "Run."; outcome { } ; step "work" { } } "#,
            r#"version 2
 mission "old" state="ready" { goal "Run."; judges { } ; step "work" { } } "#,
            r#"version 2
 mission "old" state="ready" { goal "Run."; step "work" { judge "old" { exec "true" } } } "#,
            r#"version 2
 mission "old" state="ready" change-policy="agent" { goal "Run."; step "work" { } } "#,
            r#"version 2
 mission "old" state="ready" change-authority="agent/worker" { goal "Run."; step "work" { } } "#,
        ] {
            assert!(crate::graph::parse_intent(removed, "node").is_err());
        }
    }

    #[test]
    fn revision_authority_comes_from_graph_placement() {
        let intent = crate::graph::parse_intent(
            r#"
version 2

  mission "placed" state="ready" revisions="human-only" revision-reviewer="person/mission" revision-cutover="when-idle" {
    goal "Test revision placement."
     agent "mission-owner" { workspace "."; command "true" }
    step "work" revisions="human-only" revision-reviewer="person/step" {
      assigned-to "agent/assignee"
       agent "step-owner" { workspace "."; command "true" }
    }
  }

"#,
            "node",
        )
        .unwrap();
        let mission = &intent.missions["placed"];
        assert_eq!(
            mission.revision_owners,
            vec!["agent/${ST_MISSION_RUN}/mission-owner"]
        );
        assert!(mission.revisions_human_only);
        assert_eq!(mission.revision_reviewer.as_deref(), Some("person/mission"));
        assert_eq!(
            mission.revision_cutover,
            crate::model::RevisionCutover::WhenIdle
        );
        let step = &mission.steps["work"];
        assert_eq!(
            step.revision_owners,
            vec!["agent/${ST_MISSION_RUN}/step-owner"]
        );
        assert!(step.revisions_human_only);
        assert_eq!(step.revision_reviewer.as_deref(), Some("person/step"));
        assert!(!step.revision_owners.contains(&"agent/assignee".into()));

        let separate_seat = crate::graph::parse_intent(
            r#"
version 2
agent "fleet/reviewer" { workspace "."; command "true" }
mission "finite" state="ready" {
  goal "Keep root seats separate from mission authority."
  step "work" { assigned-to "agent/fleet/reviewer" }
}
"#,
            "node",
        )
        .unwrap();
        assert!(separate_seat.missions["finite"].revision_owners.is_empty());
    }

    #[test]
    fn reserved_context_variables_cannot_be_authored() {
        let reserved = crate::graph::parse_intent(
            r#"
version 2

  mission "reserved" state="ready" {
    goal "Reject a context override."
    step "work" {
       exec "task" { command "true"; env { ST_MISSION_RUN "forged" } }
    }
  }

"#,
            "node",
        )
        .unwrap_err();
        assert_eq!(reserved.code, "reserved-context-variable");

        crate::graph::parse_intent(
            r#"
version 2

  mission "allowed" state="ready" {
    goal "Allow an application variable."
    step "work" {
       exec "task" { command "true"; env { ST_ROOT "allowed" } }
    }
  }

"#,
            "node",
        )
        .unwrap();
    }

    #[test]
    fn mission_inputs_and_run_limits_use_the_explicit_language() {
        let intent = crate::graph::parse_intent(
            r#"
version 2

  mission "parameterized" state="ready" {
    input "message" kind="text"
    input "source" kind="resource"
    concurrent-runs max=4
    goal "Process ${input.message}."
    step "work" {
      agentless
       exec "task" { command "printf '%s' '${input.message}'"; restart "never" }
      gate "the source is ready" { field "state" "${input.source}" is "ready" }
    }
  }
  mission "unbounded" state="ready" {
    concurrent-runs
    goal "Allow concurrent runs."
  }

"#,
            "node",
        )
        .unwrap();
        let parameterized = &intent.missions["parameterized"];
        assert_eq!(parameterized.inputs.len(), 2);
        assert_eq!(
            parameterized.inputs["message"].kind,
            crate::model::MissionInputKind::Text
        );
        assert_eq!(
            parameterized.inputs["source"].kind,
            crate::model::MissionInputKind::Resource
        );
        assert_eq!(parameterized.max_active_runs, Some(4));
        assert_eq!(intent.missions["unbounded"].max_active_runs, None);

        for source in [
            r#"version 2
 mission "bad" state="ready" { input "x" kind="text"; input "x" kind="text"; goal "Reject duplicate input." } "#,
            r#"version 2
 mission "bad" state="ready" { input "x" kind="secret"; goal "Reject the input kind." } "#,
            r#"version 2
 mission "bad" state="ready" { concurrent-runs max=0; goal "Reject the run limit." } "#,
            r#"version 2
 mission "bad" state="ready" { goal "Use ${input.missing}." } "#,
            r#"version 2
 mission "bad" state="ready" { agentless; goal "Reject a mission selector." } "#,
        ] {
            assert!(crate::graph::parse_intent(source, "node").is_err());
        }
    }

    #[test]
    fn kdl_interpolation_preserves_arbitrary_text_input() {
        let source = r#"version 2

  exec "task" { command "printf '%s' '${input.message}'" }

"#;
        let message = "a \"quoted\" line\nand another line";
        let variables =
            std::collections::BTreeMap::from([("input.message".into(), message.into())]);
        let interpolated = super::interpolate_kdl(source, &variables).unwrap();
        let document = interpolated.parse::<kdl::KdlDocument>().unwrap();
        let command = document
            .get("exec")
            .and_then(kdl::KdlNode::children)
            .and_then(|exec| exec.get("command"))
            .and_then(|command| command.entries().first())
            .and_then(|entry| entry.value().as_string())
            .unwrap();
        assert_eq!(command, format!("printf '%s' '{message}'"));
    }
}
