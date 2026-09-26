use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::IsTerminal as _;

use serde_json::Value;
#[cfg(test)]
use st3::model::SubjectStatus;
use st3::model::{
    AttentionItemView, MissionInputKind, MissionRunView, RevisionCutover, RevisionProposalView,
    RunGenerationView, StepRunView,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OutputStyle {
    color: bool,
}

impl OutputStyle {
    pub(crate) fn stdout() -> Self {
        Self::from_terminal(
            std::io::stdout().is_terminal(),
            std::env::var_os("NO_COLOR").is_some(),
        )
    }

    fn from_terminal(terminal: bool, no_color: bool) -> Self {
        Self {
            color: terminal && !no_color,
        }
    }

    #[cfg(test)]
    fn plain() -> Self {
        Self { color: false }
    }

    #[cfg(test)]
    fn colored() -> Self {
        Self { color: true }
    }

    fn paint(self, code: &str, value: impl std::fmt::Display) -> String {
        if self.color {
            format!("\x1b[{code}m{value}\x1b[0m")
        } else {
            value.to_string()
        }
    }

    fn heading(self, value: impl std::fmt::Display) -> String {
        self.paint("1;36", value)
    }

    fn muted(self, value: impl std::fmt::Display) -> String {
        self.paint("2", value)
    }

    fn status(self, value: &str) -> String {
        let code = match value {
            "completed" | "standing" | "available" => "1;32",
            "ready" | "claimed" => "1;36",
            "working" | "verifying" | "running" | "busy" => "1;33",
            "blocked" | "failed" | "unreachable" => "1;31",
            "cancelled" | "pending" => "2",
            _ => "1",
        };
        self.paint(code, value)
    }

    fn status_value(self, status: &str, value: impl std::fmt::Display) -> String {
        let code = match status {
            "completed" | "standing" | "available" => "1;32",
            "ready" | "claimed" => "1;36",
            "working" | "verifying" | "running" | "busy" => "1;33",
            "blocked" | "failed" | "unreachable" => "1;31",
            "cancelled" | "pending" => "2",
            _ => "1",
        };
        self.paint(code, value)
    }
}

pub(crate) fn render_mission_run(
    selected: &MissionRunView,
    runs: &[MissionRunView],
    style: OutputStyle,
    now_unix_ms: u128,
) -> String {
    let mut output = String::new();
    let children = mission_children(runs);
    let visible_runs = mission_subtree(selected, &children);
    let steps = visible_runs
        .iter()
        .flat_map(|run| run.steps.iter())
        .collect::<Vec<_>>();
    let completed = steps
        .iter()
        .filter(|step| step.status == "completed")
        .count();
    let ready = steps.iter().filter(|step| step.status == "ready").count();
    let active = steps
        .iter()
        .filter(|step| is_executing_state(&step.status))
        .count();
    let blocked = steps.iter().filter(|step| step.status == "blocked").count();

    let _ = writeln!(
        output,
        "{}  {}",
        style.heading("MISSION"),
        short_subject(&selected.mission, "mission/")
    );
    let _ = writeln!(output, "RUN       {}", selected.subject);
    let _ = writeln!(
        output,
        "STATE     {} · {}",
        style.status(&selected.status),
        selected.phase
    );
    let _ = writeln!(output, "REVISION  {}", selected.revision);
    let _ = writeln!(output, "GENERATION {}", selected.generation);
    let _ = writeln!(output, "WORKSPACE {}", selected.workspace);
    let _ = writeln!(output, "REQUESTER {}", selected.requester);
    let _ = writeln!(
        output,
        "UPDATED   {}",
        relative_time(selected.updated_at_unix_ms, now_unix_ms)
    );
    let _ = writeln!(
        output,
        "PROGRESS  {completed}/{} completed · {ready} ready · {active} active · {blocked} blocked",
        steps.len()
    );
    if let Some(deadline) = selected.deadline_at_unix_ms {
        let _ = writeln!(output, "DEADLINE  {}", relative_time(deadline, now_unix_ms));
    }
    if !selected.inputs.is_empty() {
        let _ = writeln!(output);
        let _ = writeln!(output, "{}", style.heading("INPUTS"));
        for (name, input) in &selected.inputs {
            let kind = match input.kind {
                MissionInputKind::Text => "text",
                MissionInputKind::Resource => "resource",
            };
            let _ = writeln!(output, "  {name}  {kind}  {}", input.value);
        }
    }
    let _ = writeln!(output);
    let _ = writeln!(output, "{}", style.heading("WORK"));
    if selected.steps.is_empty() {
        let _ = writeln!(output, "  {}", style.muted("No steps."));
    } else {
        render_run_steps(&mut output, selected, &children, "  ", style);
    }
    output
}

#[cfg(test)]
pub(crate) fn render_work_list(
    actor: Option<&str>,
    work: &[StepRunView],
    show_all: bool,
    style: OutputStyle,
) -> String {
    let mut output = String::new();
    let title = actor.map_or_else(|| "WORK".to_owned(), |actor| format!("WORK FOR {actor}"));
    let active = work
        .iter()
        .filter(|step| matches!(step.status.as_str(), "claimed" | "working" | "verifying"))
        .count();
    let ready = work.iter().filter(|step| step.status == "ready").count();
    let blocked = work.iter().filter(|step| step.status == "blocked").count();
    let pending = work.iter().filter(|step| step.status == "pending").count();
    let terminal = work
        .iter()
        .filter(|step| is_terminal_state(&step.status))
        .count();
    let _ = writeln!(output, "{}", style.heading(title));
    let _ = writeln!(
        output,
        "{active} active · {ready} ready · {blocked} blocked · {pending} waiting · {terminal} terminal"
    );

    let states = if show_all {
        vec![
            "working",
            "verifying",
            "claimed",
            "ready",
            "blocked",
            "pending",
            "failed",
            "completed",
            "cancelled",
        ]
    } else {
        vec!["working", "verifying", "claimed", "ready", "blocked"]
    };
    let mut shown = BTreeSet::new();
    for state in states {
        let matching = work
            .iter()
            .filter(|step| step.status == state)
            .collect::<Vec<_>>();
        if matching.is_empty() {
            continue;
        }
        let _ = writeln!(output);
        let _ = writeln!(
            output,
            "{}",
            style.status_value(state, state.to_uppercase())
        );
        for step in matching {
            shown.insert(step.subject.as_str());
            render_work_list_item(&mut output, step, style);
        }
    }
    let other = work
        .iter()
        .filter(|step| !shown.contains(step.subject.as_str()))
        .filter(|step| step.status != "pending" && !is_terminal_state(&step.status))
        .collect::<Vec<_>>();
    if !other.is_empty() {
        let _ = writeln!(output);
        let _ = writeln!(output, "{}", style.heading("OTHER"));
        for step in &other {
            render_work_list_item(&mut output, step, style);
        }
    }
    if !show_all && pending + terminal > 0 {
        let _ = writeln!(output);
        let _ = writeln!(
            output,
            "{}",
            style.muted(format!(
                "{pending} waiting items and {terminal} terminal items hidden. Use --all to show them."
            ))
        );
    }
    if shown.is_empty() && other.is_empty() {
        let _ = writeln!(output);
        let _ = writeln!(output, "{}", style.muted("No ready or active work."));
    }
    output
}

#[cfg(test)]
pub(crate) fn render_pty_list(sessions: &[SubjectStatus], style: OutputStyle) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "{}", style.heading("TERMINALS"));
    if sessions.is_empty() {
        let _ = writeln!(
            output,
            "{}",
            style.muted("No terminal sessions are declared.")
        );
        return output;
    }
    let _ = writeln!(
        output,
        "{} {}",
        sessions.len(),
        if sessions.len() == 1 {
            "session"
        } else {
            "sessions"
        }
    );
    for session in sessions {
        let fields = session
            .actual
            .as_ref()
            .map(|actual| actual.get("fields").unwrap_or(actual));
        let state = fields
            .and_then(|fields| fields.get("state").and_then(serde_json::Value::as_str))
            .or_else(|| {
                fields.and_then(|fields| fields.get("status").and_then(serde_json::Value::as_str))
            })
            .unwrap_or_else(|| {
                if session.gap.is_some() {
                    "pending"
                } else {
                    session.reachability.as_str()
                }
            });
        let host = fields
            .and_then(|fields| fields.get("host").and_then(serde_json::Value::as_str))
            .unwrap_or("unknown host");
        let runtime = fields
            .and_then(|fields| fields.get("runtime_id").and_then(serde_json::Value::as_str))
            .unwrap_or("no runtime");
        let _ = writeln!(output);
        let _ = writeln!(
            output,
            "  {} {}",
            style.status_value(state, state_mark(state)),
            session.subject
        );
        let _ = write!(
            output,
            "    {} · {} · {}",
            style.status(state),
            host,
            runtime
        );
        if let Some(owner) = &session.owner_run {
            let _ = write!(output, " · {owner}");
        }
        let _ = writeln!(output);
    }
    output
}

pub(crate) fn render_human_value(value: &Value, style: OutputStyle) -> String {
    let mut output = String::new();
    match value {
        Value::Object(fields) => {
            if let Some(title) = object_identity(fields) {
                let _ = writeln!(output, "{}", style.heading(title));
                render_object_fields(&mut output, fields, 0, style, Some(title));
            } else {
                render_object_fields(&mut output, fields, 0, style, None);
            }
        }
        Value::Array(items) => {
            let _ = writeln!(output, "{}", style.heading("RESULTS"));
            let _ = writeln!(
                output,
                "{} {}",
                items.len(),
                if items.len() == 1 { "item" } else { "items" }
            );
            if items.is_empty() {
                let _ = writeln!(output, "{}", style.muted("No items."));
            }
            for item in items {
                render_array_item(&mut output, item, style);
            }
        }
        scalar => {
            let _ = writeln!(output, "{}", scalar_text(scalar));
        }
    }
    output
}

fn render_array_item(output: &mut String, value: &Value, style: OutputStyle) {
    match value {
        Value::Object(fields) => {
            let identity = object_identity(fields).unwrap_or("item");
            let _ = writeln!(output);
            let _ = writeln!(output, "  {}", style.heading(identity));
            render_object_fields(output, fields, 2, style, Some(identity));
        }
        Value::Array(items) => {
            let _ = writeln!(output, "  • {} items", items.len());
            for item in items {
                render_nested_value(output, item, 4, style);
            }
        }
        scalar => {
            let _ = writeln!(output, "  • {}", scalar_text(scalar));
        }
    }
}

fn render_object_fields(
    output: &mut String,
    fields: &serde_json::Map<String, Value>,
    indent: usize,
    style: OutputStyle,
    displayed_identity: Option<&str>,
) {
    for (key, value) in fields {
        if displayed_identity.is_some_and(|identity| scalar_text(value) == identity)
            && matches!(key.as_str(), "subject" | "id" | "name" | "reference")
        {
            continue;
        }
        let padding = " ".repeat(indent);
        let label = field_label(key);
        match value {
            Value::Object(nested) if nested.is_empty() => {
                let _ = writeln!(output, "{padding}{label:<14} {}", style.muted("none"));
            }
            Value::Object(nested) => {
                let _ = writeln!(output, "{padding}{}", style.heading(label));
                render_object_fields(output, nested, indent + 2, style, None);
            }
            Value::Array(items) if items.is_empty() => {
                let _ = writeln!(output, "{padding}{label:<14} {}", style.muted("none"));
            }
            Value::Array(items) if items.iter().all(is_scalar) => {
                let joined = items.iter().map(scalar_text).collect::<Vec<_>>().join(", ");
                let _ = writeln!(output, "{padding}{label:<14} {joined}");
            }
            Value::Array(items) => {
                let _ = writeln!(
                    output,
                    "{padding}{}  {} items",
                    style.heading(label),
                    items.len()
                );
                for item in items {
                    render_nested_value(output, item, indent + 2, style);
                }
            }
            scalar => {
                let text = scalar_text(scalar);
                let mut lines = text.lines();
                let first = lines.next().unwrap_or_default();
                let _ = writeln!(output, "{padding}{label:<14} {first}");
                for line in lines {
                    let _ = writeln!(output, "{padding}{:<14} {line}", "");
                }
            }
        }
    }
}

fn render_nested_value(output: &mut String, value: &Value, indent: usize, style: OutputStyle) {
    let padding = " ".repeat(indent);
    match value {
        Value::Object(fields) => {
            let identity = object_identity(fields).unwrap_or("item");
            let _ = writeln!(output, "{padding}• {identity}");
            render_object_fields(output, fields, indent + 2, style, Some(identity));
        }
        Value::Array(items) => {
            let _ = writeln!(output, "{padding}• {} items", items.len());
            for item in items {
                render_nested_value(output, item, indent + 2, style);
            }
        }
        scalar => {
            let _ = writeln!(output, "{padding}• {}", scalar_text(scalar));
        }
    }
}

fn object_identity(fields: &serde_json::Map<String, Value>) -> Option<&str> {
    ["subject", "reference", "name", "id", "status"]
        .into_iter()
        .find_map(|key| fields.get(key).and_then(Value::as_str))
}

fn is_scalar(value: &Value) -> bool {
    !matches!(value, Value::Array(_) | Value::Object(_))
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::Null => "—".into(),
        Value::Bool(true) => "yes".into(),
        Value::Bool(false) => "no".into(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        Value::Array(_) | Value::Object(_) => "—".into(),
    }
}

fn field_label(key: &str) -> String {
    key.replace(['_', '-'], " ").to_uppercase()
}

#[cfg(test)]
pub(crate) fn render_attention_list(
    person: Option<&str>,
    items: &[AttentionItemView],
    style: OutputStyle,
    now_unix_ms: u128,
) -> String {
    let mut output = String::new();
    let title = person.map_or_else(
        || "HUMAN ATTENTION".to_owned(),
        |person| format!("HUMAN ATTENTION FOR {person}"),
    );
    let _ = writeln!(output, "{}", style.heading(title));
    if items.is_empty() {
        let _ = writeln!(output, "{}", style.muted("No human attention is waiting."));
        return output;
    }
    let _ = writeln!(output, "{} waiting · oldest first", items.len());
    for item in items {
        let _ = writeln!(output);
        let _ = writeln!(
            output,
            "  {} [{}] {}",
            style.status_value("ready", "?"),
            item.kind,
            item.title
        );
        let _ = write!(output, "    {}", item.person);
        if let Some(mission) = &item.mission {
            let _ = write!(output, " · {mission}");
        }
        if let Some(run) = &item.mission_run {
            let _ = write!(output, " · {run}");
        }
        if let Some(step) = &item.step {
            let _ = write!(output, " · step {step}");
        }
        let _ = writeln!(
            output,
            " · requested {}",
            relative_time(item.requested_at_unix_ms, now_unix_ms)
        );
        for target in &item.targets {
            let _ = writeln!(output, "    review: {target}");
        }
        let _ = writeln!(output, "    subject: {}", item.subject);
        let _ = writeln!(
            output,
            "    inspect: st3 attention show {} --as {}",
            item.subject, item.person
        );
        for action in &item.actions {
            let command = action
                .argv
                .iter()
                .map(|argument| shell_argument(argument))
                .collect::<Vec<_>>()
                .join(" ");
            let _ = writeln!(output, "    {}: {command}", action.label);
        }
    }
    output
}

pub(crate) fn render_attention_show(
    item: &AttentionItemView,
    style: OutputStyle,
    now_unix_ms: u128,
) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "{}  {}", style.heading("ATTENTION"), item.title);
    let _ = writeln!(output, "SUBJECT   {}", item.subject);
    let _ = writeln!(output, "KIND      {}", item.kind);
    let _ = writeln!(output, "PERSON    {}", item.person);
    let _ = writeln!(
        output,
        "REQUESTED {}",
        relative_time(item.requested_at_unix_ms, now_unix_ms)
    );
    if let Some(mission) = &item.mission {
        let _ = writeln!(output, "MISSION   {mission}");
    }
    if let Some(run) = &item.mission_run {
        let _ = writeln!(output, "RUN       {run}");
    }
    if let Some(step) = &item.step {
        let _ = writeln!(output, "STEP      {step}");
    }
    let _ = writeln!(output, "\n{}\n{}", style.heading("DETAIL"), item.detail);
    if !item.targets.is_empty() {
        let _ = writeln!(output, "\n{}", style.heading("TARGETS"));
        for target in &item.targets {
            let _ = writeln!(output, "  {target}");
        }
    }
    if !item.actions.is_empty() {
        let _ = writeln!(output, "\n{}", style.heading("ACTIONS"));
        for action in &item.actions {
            let command = action
                .argv
                .iter()
                .map(|argument| shell_argument(argument))
                .collect::<Vec<_>>()
                .join(" ");
            let _ = writeln!(output, "  {}: {command}", action.label);
        }
    }
    output
}

pub(crate) fn shell_argument(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b':' | b'@' | b'_' | b'-')
        })
    {
        return value.into();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(crate) fn render_step_run(step: &StepRunView, style: OutputStyle, now_unix_ms: u128) -> String {
    let mut output = String::new();
    let title = step.title.as_deref().unwrap_or(&step.step);
    let _ = writeln!(output, "{}  {title}", style.heading("STEP"));
    let _ = writeln!(output, "SUBJECT    {}", step.subject);
    let _ = writeln!(
        output,
        "STATE      {} · attempt {}",
        style.status(&step.status),
        step.attempt
    );
    let _ = writeln!(output, "MISSION RUN {}", step.run);
    let _ = writeln!(output, "GENERATION {}", step.generation);
    let _ = writeln!(output, "DEFINITION {}", step.definition_hash);
    let _ = writeln!(
        output,
        "UPDATED    {}",
        relative_time(step.updated_at_unix_ms, now_unix_ms)
    );
    if let Some((queue, position)) = step.queue.as_deref().zip(step.queue_position) {
        let _ = writeln!(output, "QUEUE      {queue} #{position}");
    }
    if let Some(not_before) = step.not_before_unix_ms {
        let _ = writeln!(
            output,
            "NOT BEFORE {}",
            relative_time(not_before, now_unix_ms)
        );
    }
    if let Some(reason) = &step.blocked_reason {
        let _ = writeln!(output, "REASON     {reason}");
    }
    for blocker in &step.blockers {
        let _ = writeln!(output, "BLOCKER    {blocker}");
    }
    if let Some(ready_age_ms) = step.ready_age_ms {
        let _ = writeln!(output, "READY AGE  {ready_age_ms}ms");
    }

    let _ = writeln!(output);
    let _ = writeln!(output, "{}", style.heading("WORKER"));
    if step.agentless {
        let _ = writeln!(output, "  Agentless");
    }
    if let Some(agent) = &step.assigned_to {
        let _ = writeln!(output, "  Assigned to  {agent}");
    }
    for agent in &step.available_to {
        let _ = writeln!(output, "  Available to {agent}");
    }
    if let Some(agent) = &step.claimant {
        let _ = writeln!(output, "  Claimant     {agent}");
    }
    if let Some(incarnation) = &step.claim_incarnation {
        let _ = writeln!(output, "  Incarnation  {incarnation}");
    }
    if let Some(expires) = step.claim_expires_at_unix_ms {
        let _ = writeln!(
            output,
            "  Lease        {}",
            relative_time(expires, now_unix_ms)
        );
    }
    if let Some(timeout_ms) = step.timeout_ms {
        let active = step
            .execution_started_at_unix_ms
            .map(|started| format!(" · active {}", relative_time(started, now_unix_ms)))
            .unwrap_or_default();
        let _ = writeln!(
            output,
            "  Execution    {}ms / {timeout_ms}ms{active}",
            step.execution_elapsed_ms
        );
    }
    if let Some(wake) = &step.wake {
        let acknowledgement = wake.acknowledged_by.as_deref().unwrap_or("pending");
        let _ = writeln!(
            output,
            "  Wake         {} attempt(s) · {acknowledgement} · assignee {}",
            wake.attempts, wake.assignee_state
        );
        let _ = writeln!(output, "  Wake epoch   {}", wake.incarnation_id);
        if let Some(last) = wake.last_attempt_at_unix_ms {
            let _ = writeln!(
                output,
                "  Wake last    {}",
                relative_time(last, now_unix_ms)
            );
        }
        if let Some(failure) = &wake.failure {
            let _ = writeln!(output, "  Wake failure {failure}");
        }
    }
    for grouping in &step.under {
        if let Some(reason) = &grouping.reason {
            let _ = writeln!(output, "  Under        {} ({reason})", grouping.agent);
        } else {
            let _ = writeln!(output, "  Under        {}", grouping.agent);
        }
    }
    if !step.goals.is_empty() {
        let _ = writeln!(output);
        let _ = writeln!(output, "{}", style.heading("GOALS"));
        for goal in &step.goals {
            let _ = writeln!(output, "  • {goal}");
        }
    }
    if !step.constraints.is_empty() {
        let _ = writeln!(output);
        let _ = writeln!(output, "{}", style.heading("CONSTRAINTS"));
        for constraint in &step.constraints {
            let _ = writeln!(output, "  • {constraint}");
        }
    }
    output
}

pub(crate) fn render_revision_proposal(
    proposal: &RevisionProposalView,
    style: OutputStyle,
    now_unix_ms: u128,
) -> String {
    let mut output = String::new();
    let cutover = match proposal.cutover {
        RevisionCutover::RestartActive => "restart-active",
        RevisionCutover::WhenIdle => "when-idle",
    };
    let _ = writeln!(
        output,
        "{}  {}",
        style.heading("REVISION PROPOSAL"),
        proposal.subject
    );
    let _ = writeln!(output, "STATE      {}", style.status(&proposal.status));
    let _ = writeln!(output, "RUN        {}", proposal.run);
    let _ = writeln!(output, "SOURCE     {}", proposal.source_generation);
    let _ = writeln!(output, "CANDIDATE  {}", proposal.candidate_revision);
    let _ = writeln!(output, "ACTOR      {}", proposal.actor);
    let _ = writeln!(output, "CUTOVER    {cutover}");
    let _ = writeln!(
        output,
        "UPDATED    {}",
        relative_time(proposal.updated_at_unix_ms, now_unix_ms)
    );
    let _ = writeln!(output, "REASON     {}", proposal.reason);
    if let Some(hash) = &proposal.preview_hash {
        let _ = writeln!(output, "APPROVAL HASH {hash}");
    }
    append_named_list(
        &mut output,
        "COMPATIBLE STEPS",
        &proposal.compatible_steps,
        style,
    );
    append_named_list(&mut output, "REVIEWERS", &proposal.reviewers, style);
    append_named_list(&mut output, "APPROVALS", &proposal.approvals, style);
    if let Some(successor) = &proposal.successor_generation {
        let _ = writeln!(output);
        let _ = writeln!(output, "SUCCESSOR  {successor}");
    }
    output
}

pub(crate) fn render_generations(
    run: &MissionRunView,
    generations: &[RunGenerationView],
    style: OutputStyle,
) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "{}  {}", style.heading("GENERATIONS"), run.subject);
    let _ = writeln!(output, "CURRENT  {}", run.generation);
    let mut ordered = generations.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|generation| generation.created_at_unix_ms);
    for generation in ordered {
        let mark = if generation.subject == run.generation {
            "●"
        } else {
            "○"
        };
        let _ = writeln!(
            output,
            "{mark} {}  {}",
            style.status(&generation.status),
            generation.subject
        );
        let _ = writeln!(output, "  revision {}", generation.revision);
        let _ = writeln!(
            output,
            "  actor {} · {}",
            generation.actor, generation.reason
        );
        if let Some(predecessor) = &generation.predecessor {
            let _ = writeln!(output, "  after {predecessor}");
        }
    }
    output
}

pub(crate) fn render_generation(generation: &RunGenerationView, style: OutputStyle) -> String {
    let mut output = String::new();
    let completed = generation
        .steps
        .iter()
        .filter(|step| step.status == "completed")
        .count();
    let ready = generation
        .steps
        .iter()
        .filter(|step| step.status == "ready")
        .count();
    let active = generation
        .steps
        .iter()
        .filter(|step| is_executing_state(&step.status))
        .count();
    let _ = writeln!(
        output,
        "{}  {}",
        style.heading("GENERATION"),
        generation.subject
    );
    let _ = writeln!(output, "STATE     {}", style.status(&generation.status));
    let _ = writeln!(output, "RUN       {}", generation.run);
    let _ = writeln!(output, "REVISION  {}", generation.revision);
    let _ = writeln!(output, "ACTOR     {}", generation.actor);
    let _ = writeln!(output, "REASON    {}", generation.reason);
    if let Some(predecessor) = &generation.predecessor {
        let _ = writeln!(output, "PREVIOUS  {predecessor}");
    }
    let _ = writeln!(
        output,
        "PROGRESS  {completed}/{} completed · {ready} ready · {active} active",
        generation.steps.len()
    );
    let _ = writeln!(output);
    let _ = writeln!(output, "{}", style.heading("WORK"));
    if generation.steps.is_empty() {
        let _ = writeln!(output, "  {}", style.muted("No steps."));
    } else {
        render_flat_step_tree(&mut output, &generation.steps, "  ", style);
    }
    output
}

pub(crate) fn mission_run_signature(runs: &[MissionRunView]) -> anyhow::Result<String> {
    let state = runs
        .iter()
        .map(|run| {
            serde_json::json!({
                "subject": run.subject,
                "revision": run.revision,
                "generation": run.generation,
                "status": run.status,
                "phase": run.phase,
                "loops": run.loops,
                "steps": run.steps.iter().map(|step| serde_json::json!({
                    "subject": step.subject,
                    "status": step.status,
                    "attempt": step.attempt,
                    "claimant": step.claimant,
                    "blocked_reason": step.blocked_reason,
                })).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&state).map_err(Into::into)
}

pub(crate) fn follow_snapshot(frame: &str, interactive: bool, has_prior: bool) -> String {
    if interactive {
        format!("\x1b[2J\x1b[H{frame}")
    } else if has_prior {
        format!("\n{frame}")
    } else {
        frame.to_owned()
    }
}

fn mission_children(runs: &[MissionRunView]) -> BTreeMap<&str, Vec<&MissionRunView>> {
    let mut children = BTreeMap::<_, Vec<_>>::new();
    for run in runs {
        if let Some(parent) = run.parent_step_run.as_deref() {
            children.entry(parent).or_default().push(run);
        }
    }
    children
}

fn mission_subtree<'a>(
    selected: &'a MissionRunView,
    children: &BTreeMap<&str, Vec<&'a MissionRunView>>,
) -> Vec<&'a MissionRunView> {
    fn collect<'a>(
        run: &'a MissionRunView,
        children: &BTreeMap<&str, Vec<&'a MissionRunView>>,
        output: &mut Vec<&'a MissionRunView>,
    ) {
        output.push(run);
        for step in &run.steps {
            for child in children.get(step.subject.as_str()).into_iter().flatten() {
                collect(child, children, output);
            }
        }
    }
    let mut output = Vec::new();
    collect(selected, children, &mut output);
    output
}

fn render_run_steps(
    output: &mut String,
    run: &MissionRunView,
    children: &BTreeMap<&str, Vec<&MissionRunView>>,
    indent: &str,
    style: OutputStyle,
) {
    let base_depth = run
        .steps
        .iter()
        .map(|step| step_depth(&step.step))
        .min()
        .unwrap_or_default();
    let mut roots = run
        .steps
        .iter()
        .filter(|step| step_depth(&step.step) == base_depth)
        .collect::<Vec<_>>();
    roots.sort_by(
        |left, right| match (left.queue_position, right.queue_position) {
            (Some(left), Some(right)) => left.cmp(&right),
            _ => std::cmp::Ordering::Equal,
        },
    );
    for step in roots {
        render_graph_step(output, step, indent, style);
        if let Some(loop_run) = run.loops.iter().find(|loop_run| loop_run.path == step.step) {
            let _ = write!(
                output,
                "{indent}  ↳ loop {} · {} · round {}/{}",
                loop_run.mode,
                style.status(&loop_run.status),
                loop_run.round,
                loop_run.max_rounds
            );
            if let Some(parallel) = loop_run.max_parallel {
                let _ = write!(output, " · {parallel} parallel");
            }
            if let Some(items) = loop_run.item_count {
                let _ = write!(output, " · {items} items");
            }
            if let Some(candidates) = loop_run.candidate_count {
                let _ = write!(output, " · {candidates} candidates");
            }
            if let Some(winner) = loop_run.winner {
                let _ = write!(output, " · winner {winner}");
            }
            let _ = writeln!(output);
            if !loop_run.best_metrics.is_empty() {
                let metrics = loop_run
                    .best_metrics
                    .iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(output, "{indent}    best {metrics}");
            }
            if let Some(reason) = &loop_run.reason {
                let _ = writeln!(output, "{indent}    {reason}");
            }
        }
        let prefix = format!("{}/", step.step);
        let nested = run
            .steps
            .iter()
            .filter(|candidate| candidate.step.starts_with(&prefix))
            .collect::<Vec<_>>();
        if !nested.is_empty() {
            let completed = nested
                .iter()
                .filter(|candidate| candidate.status == "completed")
                .count();
            if is_active_state(&step.status) || matches!(step.status.as_str(), "failed" | "blocked")
            {
                for nested_step in nested {
                    let relative = step_depth(&nested_step.step).saturating_sub(base_depth);
                    render_graph_step(
                        output,
                        nested_step,
                        &format!("{indent}{}", "  ".repeat(relative)),
                        style,
                    );
                }
            } else {
                let _ = writeln!(
                    output,
                    "{indent}  ↳ nested work · {completed}/{} completed",
                    nested.len()
                );
            }
        }
        for child in children.get(step.subject.as_str()).into_iter().flatten() {
            let child_completed = child
                .steps
                .iter()
                .filter(|nested| nested.status == "completed")
                .count();
            let _ = writeln!(
                output,
                "{indent}  ↳ {} · {} · {child_completed}/{} completed",
                short_subject(&child.mission, "mission/"),
                style.status(&child.status),
                child.steps.len()
            );
            if !matches!(child.status.as_str(), "completed" | "cancelled") {
                render_run_steps(output, child, children, &format!("{indent}    "), style);
            }
        }
    }
}

fn render_flat_step_tree(
    output: &mut String,
    steps: &[StepRunView],
    indent: &str,
    style: OutputStyle,
) {
    let base_depth = steps
        .iter()
        .map(|step| step_depth(&step.step))
        .min()
        .unwrap_or_default();
    for step in steps {
        let relative = step_depth(&step.step).saturating_sub(base_depth);
        render_graph_step(
            output,
            step,
            &format!("{indent}{}", "  ".repeat(relative)),
            style,
        );
    }
}

fn render_graph_step(output: &mut String, step: &StepRunView, indent: &str, style: OutputStyle) {
    let title = step.title.as_deref().unwrap_or(&step.step);
    let actor = work_actor(step)
        .map(|actor| format!(" · {}", short_subject(actor, "agent/")))
        .unwrap_or_default();
    let attempt = if step.attempt > 1 {
        format!(" · attempt {}", step.attempt)
    } else {
        String::new()
    };
    let queue = step
        .queue
        .as_deref()
        .zip(step.queue_position)
        .map(|(queue, position)| format!(" · queue {queue} #{position}"))
        .unwrap_or_default();
    let _ = writeln!(
        output,
        "{indent}{} {} {} — {title}{actor}{attempt}{queue}",
        style.status_value(&step.status, state_mark(&step.status)),
        style.status_value(&step.status, format!("{:<10}", step.status)),
        step.step.rsplit('/').next().unwrap_or(&step.step),
    );
    if let Some(reason) = &step.blocked_reason {
        let _ = writeln!(output, "{indent}  reason: {reason}");
    }
}

#[cfg(test)]
fn render_work_list_item(output: &mut String, step: &StepRunView, style: OutputStyle) {
    let title = step.title.as_deref().unwrap_or(&step.step);
    let _ = writeln!(
        output,
        "  {} {title}",
        style.status_value(&step.status, state_mark(&step.status))
    );
    let _ = write!(output, "    {}", step.subject);
    if let Some((queue, position)) = step.queue.as_deref().zip(step.queue_position) {
        let _ = write!(output, " · queue {queue} #{position}");
    }
    if let Some(actor) = work_actor(step) {
        let _ = write!(output, " · {}", short_subject(actor, "agent/"));
    }
    let _ = writeln!(output);
    let _ = writeln!(output, "    owner: {}", step.run);
    if let Some(timeout_ms) = step.timeout_ms {
        let _ = writeln!(
            output,
            "    execution: {}ms / {timeout_ms}ms{}",
            step.execution_elapsed_ms,
            step.execution_started_at_unix_ms
                .map(|started| format!(" · active since {started}"))
                .unwrap_or_default()
        );
    }
    if let Some(ready_age_ms) = step.ready_age_ms {
        let _ = writeln!(output, "    ready age: {ready_age_ms}ms");
    }
    if let Some(wake) = &step.wake {
        let _ = writeln!(
            output,
            "    wake: {} attempt(s) · {} · assignee {}{}",
            wake.attempts,
            wake.acknowledged_by.as_deref().unwrap_or("pending"),
            wake.assignee_state,
            wake.failure
                .as_ref()
                .map(|failure| format!(" · failed: {failure}"))
                .unwrap_or_default()
        );
    }
    if let Some(reason) = &step.blocked_reason {
        let _ = writeln!(output, "    reason: {reason}");
    }
}

fn append_named_list(output: &mut String, name: &str, values: &[String], style: OutputStyle) {
    if values.is_empty() {
        return;
    }
    let _ = writeln!(output);
    let _ = writeln!(output, "{}", style.heading(name));
    for value in values {
        let _ = writeln!(output, "  • {value}");
    }
}

fn short_subject<'a>(subject: &'a str, prefix: &str) -> &'a str {
    subject.strip_prefix(prefix).unwrap_or(subject)
}

fn step_depth(step: &str) -> usize {
    step.matches('/').count()
}

fn is_active_state(status: &str) -> bool {
    matches!(
        status,
        "ready" | "claimed" | "working" | "verifying" | "blocked"
    )
}

fn is_executing_state(status: &str) -> bool {
    matches!(status, "claimed" | "working" | "verifying")
}

#[cfg(test)]
fn is_terminal_state(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

fn state_mark(status: &str) -> &'static str {
    match status {
        "completed" => "✓",
        "working" | "verifying" => "▶",
        "claimed" => "◉",
        "ready" => "●",
        "blocked" => "!",
        "failed" => "✗",
        "cancelled" => "×",
        _ => "·",
    }
}

fn work_actor(step: &StepRunView) -> Option<&str> {
    step.claimant
        .as_deref()
        .or(step.assigned_to.as_deref())
        .or_else(|| (step.available_to.len() == 1).then(|| step.available_to[0].as_str()))
}

fn relative_time(value: u128, now: u128) -> String {
    let (future, delta) = if value >= now {
        (true, value - now)
    } else {
        (false, now - value)
    };
    let seconds = delta / 1_000;
    if seconds == 0 {
        return "now".into();
    }
    let amount = if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    };
    if future {
        format!("in {amount}")
    } else {
        format!("{amount} ago")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use st3::model::{LoopRunView, MissionRunInput, UnderSpec};

    fn step(subject: &str, path: &str, status: &str) -> StepRunView {
        StepRunView {
            subject: subject.into(),
            run: "mission-run/demo/run".into(),
            generation: "run-generation/demo-generation".into(),
            step: path.into(),
            queue: None,
            queue_position: None,
            definition_hash: "definition-hash".into(),
            status: status.into(),
            attempt: 1,
            assigned_to: Some("agent/demo/worker".into()),
            available_to: Vec::new(),
            agentless: false,
            title: Some(path.replace('/', " ")),
            goals: vec!["Complete the work.".into()],
            constraints: vec!["Keep the proof.".into()],
            under: Vec::<UnderSpec>::new(),
            worker_reported: false,
            claimant: None,
            claim_incarnation: None,
            claim_expires_at_unix_ms: None,
            execution_started_at_unix_ms: None,
            execution_elapsed_ms: 0,
            timeout_ms: None,
            ready_age_ms: None,
            wake: None,
            readiness_epoch: 1,
            blocked_reason: None,
            blockers: Vec::new(),
            not_before_unix_ms: None,
            created_at_unix_ms: 1_000,
            updated_at_unix_ms: 2_000,
        }
    }

    fn run(subject: &str, parent: Option<&str>, steps: Vec<StepRunView>) -> MissionRunView {
        MissionRunView {
            subject: subject.into(),
            id: subject
                .strip_prefix("mission-run/")
                .unwrap_or(subject)
                .into(),
            mission: "mission/demo".into(),
            generation: "run-generation/demo-generation".into(),
            initial_revision: "initial-revision".into(),
            revision: "current-revision".into(),
            root_revision: "root-revision".into(),
            root_mission_run: "mission-run/demo/run".into(),
            parent_step_run: parent.map(str::to_owned),
            workspace: "/workspace".into(),
            requester: "person/tester".into(),
            inputs: BTreeMap::from([(
                "branch".into(),
                MissionRunInput {
                    kind: MissionInputKind::Text,
                    value: "main".into(),
                    subject: None,
                    claim_id: None,
                },
            )]),
            mode: "run".into(),
            timeout_ms: None,
            deadline_at_unix_ms: None,
            status: "running".into(),
            phase: "normal".into(),
            created_at_unix_ms: 1_000,
            updated_at_unix_ms: 2_000,
            steps,
            loops: Vec::new(),
        }
    }

    fn terminal(subject: &str, status: &str) -> SubjectStatus {
        SubjectStatus {
            subject: subject.into(),
            kind: Some("agent".into()),
            desired_token: Some("claim/desired".into()),
            desired_revision: Some("revision".into()),
            desired: None,
            actual: Some(serde_json::json!({
                "fields": {
                    "status": status,
                    "host": "test-node",
                    "runtime_id": "runtime.test",
                    "terminal": true
                }
            })),
            actual_claim: Some("claim/actual".into()),
            actual_origin: Some("test-node".into()),
            harness: None,
            conflicts: Vec::new(),
            claims: vec!["claim/actual".into()],
            owner_run: Some("mission-run/test/run".into()),
            gap: None,
            reachability: "reachable".into(),
            reason: None,
            under: Vec::new(),
            projection: Default::default(),
        }
    }

    #[test]
    fn terminal_list_is_readable_and_keeps_exact_identifiers() {
        let rendered = render_pty_list(
            &[terminal("agent/test/worker", "running")],
            OutputStyle::plain(),
        );
        assert!(rendered.contains("TERMINALS"));
        assert!(rendered.contains("1 session"));
        assert!(rendered.contains("agent/test/worker"));
        assert!(rendered.contains("running · test-node · runtime.test"));
        assert!(rendered.contains("mission-run/test/run"));
        assert!(!rendered.contains('{'));
    }

    #[test]
    fn terminal_list_has_a_clear_empty_state() {
        assert_eq!(
            render_pty_list(&[], OutputStyle::plain()),
            "TERMINALS\nNo terminal sessions are declared.\n"
        );
    }

    #[test]
    fn generic_human_output_renders_objects_without_json_syntax() {
        let value = serde_json::json!({
            "subject": "claim/example",
            "ready": true,
            "optional": null,
            "tags": ["one", "two"],
            "details": {"exit_code": 0}
        });
        let rendered = render_human_value(&value, OutputStyle::plain());
        assert!(rendered.starts_with("claim/example\n"));
        assert!(rendered.contains("READY          yes"));
        assert!(rendered.contains("OPTIONAL       —"));
        assert!(rendered.contains("TAGS           one, two"));
        assert!(rendered.contains("EXIT CODE      0"));
        assert!(!rendered.contains('{'));
        assert!(!rendered.contains('"'));
    }

    #[test]
    fn generic_human_output_renders_nested_arrays_and_multiline_text() {
        let value = serde_json::json!([
            {"id": "first", "message": "line one\nline two"},
            {"id": "second", "values": [{"name": "nested", "ok": false}]}
        ]);
        let rendered = render_human_value(&value, OutputStyle::plain());
        assert!(rendered.contains("RESULTS\n2 items"));
        assert!(rendered.contains("first"));
        assert!(rendered.contains("line one"));
        assert!(rendered.contains("line two"));
        assert!(rendered.contains("nested"));
        assert!(rendered.contains("OK             no"));
    }

    #[test]
    fn generic_human_output_handles_empty_and_scalar_values() {
        assert_eq!(
            render_human_value(&serde_json::json!([]), OutputStyle::plain()),
            "RESULTS\n0 items\nNo items.\n"
        );
        assert_eq!(
            render_human_value(&serde_json::json!("plain"), OutputStyle::plain()),
            "plain\n"
        );
    }

    #[test]
    fn attention_list_shows_kind_age_targets_and_safe_commands() {
        let item = AttentionItemView {
            kind: "fault".into(),
            subject: "attention/fabric".into(),
            person: "person/nathan".into(),
            title: "Fabric needs review".into(),
            detail: "The queue did not recover.".into(),
            mission: Some("mission/fabric".into()),
            mission_run: Some("mission-run/fabric/one".into()),
            step: None,
            targets: vec!["doc/fabric/report@abc".into()],
            requested_at_unix_ms: 60_000,
            actions: vec![st3::model::AttentionActionView {
                label: "resolve".into(),
                argv: vec![
                    "st3".into(),
                    "attention".into(),
                    "resolve".into(),
                    "attention/fabric".into(),
                    "--reason".into(),
                    "It is fixed".into(),
                ],
            }],
        };
        let rendered = render_attention_list(
            Some("person/nathan"),
            std::slice::from_ref(&item),
            OutputStyle::plain(),
            180_000,
        );
        assert!(rendered.contains("HUMAN ATTENTION FOR person/nathan"));
        assert!(rendered.contains("1 waiting · oldest first"));
        assert!(rendered.contains("[fault] Fabric needs review"));
        assert!(rendered.contains("requested 2m ago"));
        assert!(rendered.contains("review: doc/fabric/report@abc"));
        assert!(rendered.contains("--reason 'It is fixed'"));
        assert!(!rendered.contains("The queue did not recover."));
        assert!(
            rendered.contains("inspect: st3 attention show attention/fabric --as person/nathan")
        );

        let shown = render_attention_show(&item, OutputStyle::plain(), 180_000);
        assert!(shown.contains("ATTENTION  Fabric needs review"));
        assert!(shown.contains("The queue did not recover."));
        assert!(shown.contains("TARGETS\n  doc/fabric/report@abc"));
    }

    #[test]
    fn attention_list_has_a_clear_empty_state() {
        let rendered = render_attention_list(None, &[], OutputStyle::plain(), 180_000);
        assert_eq!(
            rendered,
            "HUMAN ATTENTION\nNo human attention is waiting.\n"
        );
    }

    #[test]
    fn mission_view_expands_active_work_and_summarizes_completed_children() {
        let mut queue = step(
            "step-run/demo-generation/investigate",
            "investigate",
            "working",
        );
        queue.queue = Some("issues".into());
        queue.queue_position = Some(1);
        let nested = step(
            "step-run/demo-generation/investigate/research",
            "investigate/research",
            "completed",
        );
        let release = step("step-run/demo-generation/release", "release", "pending");
        let root = run("mission-run/demo/run", None, vec![queue, nested, release]);
        let mut child = run(
            "mission-run/demo/child",
            Some("step-run/demo-generation/release"),
            vec![step(
                "step-run/child-generation/publish",
                "publish",
                "completed",
            )],
        );
        child.status = "completed".into();

        let rendered =
            render_mission_run(&root, &[root.clone(), child], OutputStyle::plain(), 3_000);

        assert!(rendered.contains("MISSION  demo"));
        assert!(rendered.contains("PROGRESS  2/4 completed · 0 ready · 1 active"));
        assert!(rendered.contains("research — investigate research"));
        assert!(rendered.contains("queue issues #1"));
        assert!(rendered.contains("↳ demo · completed · 1/1 completed"));
        assert!(!rendered.contains("publish — publish"));
    }

    #[test]
    fn mission_view_shows_loop_progress_and_best_metrics() {
        let loop_step = step("step-run/demo-generation/improve", "improve", "working");
        let mut root = run("mission-run/demo/run", None, vec![loop_step]);
        root.loops.push(LoopRunView {
            subject: "loop-run/demo-generation/improve".into(),
            id: "improve".into(),
            path: "improve".into(),
            step_run: "step-run/demo-generation/improve".into(),
            mode: "best-of-n".into(),
            status: "running".into(),
            round: 2,
            max_rounds: 5,
            timeout_ms: Some(60_000),
            max_parallel: Some(2),
            item_count: None,
            candidate_count: Some(3),
            best_round: Some(1),
            best_metrics: BTreeMap::from([("quality".into(), 0.75)]),
            feedback: Some("doc/loop-feedback/example@hash".into()),
            winner: Some(2),
            reason: None,
            results: Vec::new(),
        });

        let rendered = render_mission_run(&root, &[root.clone()], OutputStyle::plain(), 3_000);

        assert!(rendered.contains("loop best-of-n · running · round 2/5"));
        assert!(rendered.contains("2 parallel · 3 candidates · winner 2"));
        assert!(rendered.contains("best quality=0.75"));
    }

    #[test]
    fn work_list_hides_waiting_and_terminal_items_by_default() {
        let ready = step("step-run/demo-generation/ready", "ready", "ready");
        let pending = step("step-run/demo-generation/later", "later", "pending");
        let completed = step("step-run/demo-generation/done", "done", "completed");

        let rendered = render_work_list(
            Some("agent/demo/worker"),
            &[ready, pending, completed],
            false,
            OutputStyle::plain(),
        );

        assert!(rendered.contains("READY"));
        assert!(rendered.contains("step-run/demo-generation/ready"));
        assert!(!rendered.contains("step-run/demo-generation/later"));
        assert!(rendered.contains("1 waiting items and 1 terminal items hidden"));
    }

    #[test]
    fn work_list_all_includes_waiting_and_terminal_items() {
        let pending = step("step-run/demo-generation/later", "later", "pending");
        let mut completed = step("step-run/demo-generation/done", "done", "completed");
        completed.blocked_reason = Some("the owning mission run is terminal".into());

        let rendered = render_work_list(None, &[pending, completed], true, OutputStyle::plain());

        assert!(rendered.contains("PENDING"));
        assert!(rendered.contains("COMPLETED"));
        assert!(rendered.contains("step-run/demo-generation/later"));
        assert!(rendered.contains("step-run/demo-generation/done"));
        assert!(rendered.contains("owner: mission-run/demo/run"));
        assert!(rendered.contains("reason: the owning mission run is terminal"));
    }

    #[test]
    fn step_view_keeps_exact_work_identifiers() {
        let mut claimed = step("step-run/demo-generation/build", "build", "claimed");
        claimed.claimant = Some("agent/demo/worker".into());
        claimed.claim_incarnation = Some("incarnation/exact".into());
        claimed.claim_expires_at_unix_ms = Some(62_000);
        claimed.execution_started_at_unix_ms = Some(1_000);
        claimed.execution_elapsed_ms = 1_000;
        claimed.timeout_ms = Some(60_000);

        let rendered = render_step_run(&claimed, OutputStyle::plain(), 2_000);

        assert!(rendered.contains("SUBJECT    step-run/demo-generation/build"));
        assert!(rendered.contains("GENERATION run-generation/demo-generation"));
        assert!(rendered.contains("Lease        in 1m"));
        assert!(rendered.contains("Execution    1000ms / 60000ms · active 1s ago"));
        assert!(rendered.contains("• Complete the work."));
    }

    #[test]
    fn colored_style_adds_ansi_and_plain_style_does_not() {
        let colored = OutputStyle::colored().status("working");
        let plain = OutputStyle::plain().status("working");

        assert!(colored.contains("\x1b["));
        assert!(!plain.contains("\x1b["));
    }

    #[test]
    fn terminal_and_no_color_select_the_output_style() {
        assert!(OutputStyle::from_terminal(true, false).color);
        assert!(!OutputStyle::from_terminal(false, false).color);
        assert!(!OutputStyle::from_terminal(true, true).color);
    }

    #[test]
    fn follow_snapshots_redraw_terminals_and_append_to_pipes() {
        assert_eq!(follow_snapshot("frame", true, false), "\x1b[2J\x1b[Hframe");
        assert_eq!(follow_snapshot("frame", false, false), "frame");
        assert_eq!(follow_snapshot("frame", false, true), "\nframe");
    }

    #[test]
    fn mission_signature_ignores_lease_renewal_times() {
        let first = run(
            "mission-run/demo/run",
            None,
            vec![step("step-run/demo-generation/work", "work", "working")],
        );
        let mut renewed = first.clone();
        renewed.updated_at_unix_ms += 60_000;
        renewed.steps[0].updated_at_unix_ms += 60_000;
        renewed.steps[0].claim_expires_at_unix_ms = Some(120_000);

        assert_eq!(
            mission_run_signature(&[first]).unwrap(),
            mission_run_signature(&[renewed]).unwrap()
        );
    }

    #[test]
    fn generation_lineage_marks_the_current_generation() {
        let root = run("mission-run/demo/run", None, Vec::new());
        let generations = vec![RunGenerationView {
            subject: root.generation.clone(),
            id: "demo-generation".into(),
            run: root.subject.clone(),
            revision: root.revision.clone(),
            predecessor: None,
            status: "current".into(),
            actor: "agent/demo/worker".into(),
            reason: "Initial run".into(),
            created_at_unix_ms: 1_000,
            updated_at_unix_ms: 2_000,
            steps: Vec::new(),
        }];

        let rendered = render_generations(&root, &generations, OutputStyle::plain());

        assert!(rendered.contains("● current  run-generation/demo-generation"));
        assert!(rendered.contains("revision current-revision"));
    }

    #[test]
    fn revision_proposal_keeps_the_exact_approval_token() {
        let proposal = RevisionProposalView {
            subject: "revision-proposal/exact".into(),
            id: "exact".into(),
            run: "mission-run/demo/run".into(),
            source_generation: "run-generation/old".into(),
            candidate_revision: "candidate-revision".into(),
            actor: "agent/demo/worker".into(),
            reason: "Add a verification step.".into(),
            status: "pending".into(),
            cutover: RevisionCutover::WhenIdle,
            compatible_steps: vec!["build".into()],
            reviewers: vec!["person/reviewer".into()],
            approvals: vec!["person/first-reviewer".into()],
            preview_hash: Some("full-preview-hash".into()),
            successor_generation: None,
            created_at_unix_ms: 1_000,
            updated_at_unix_ms: 2_000,
        };

        let rendered = render_revision_proposal(&proposal, OutputStyle::plain(), 3_000);

        assert!(rendered.contains("REVISION PROPOSAL  revision-proposal/exact"));
        assert!(rendered.contains("CUTOVER    when-idle"));
        assert!(rendered.contains("APPROVAL HASH full-preview-hash"));
        assert!(rendered.contains("• person/reviewer"));
    }

    #[test]
    fn generation_view_shows_blocked_retries_and_empty_work() {
        let mut blocked = step("step-run/new/build", "build", "blocked");
        blocked.attempt = 2;
        blocked.blocked_reason = Some("The review is pending.".into());
        let generation = RunGenerationView {
            subject: "run-generation/new".into(),
            id: "new".into(),
            run: "mission-run/demo/run".into(),
            revision: "revision".into(),
            predecessor: Some("run-generation/old".into()),
            status: "current".into(),
            actor: "agent/demo/worker".into(),
            reason: "Revise the work.".into(),
            created_at_unix_ms: 1_000,
            updated_at_unix_ms: 2_000,
            steps: vec![blocked],
        };

        let rendered = render_generation(&generation, OutputStyle::plain());
        assert!(rendered.contains("blocked"));
        assert!(rendered.contains("attempt 2"));
        assert!(rendered.contains("reason: The review is pending."));

        let mut empty = generation;
        empty.steps.clear();
        assert!(render_generation(&empty, OutputStyle::plain()).contains("No steps."));
    }
}
