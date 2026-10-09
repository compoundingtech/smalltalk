//! Dormant, test-registered negative operator. This module is absent from non-test builds.
//! Inputs are eight fixture-supplied selected facts, NOT a native authority certificate.
//! No production extractor, trigger, registration, seed, flush or Doctor call exists here.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Deserialize;
use serde_json::Value;
use smallclaims::ivm::install::{Mutation, Namespace, Operator};

pub(crate) const VIEW: &str = "test.terminal-exec-gates.v1";
pub(crate) const SOURCE: &str = "test.selected-terminal-facts.v1";
const MEMBERS: usize = 64;
const INPUT_BYTES: usize = 16 * 1024;
// These kinds participate in actual-state folding but do not select the runtime observation.
// No wildcard is accepted: a new runtime.action kind requires a source-policy review.
const NON_SELECTING: &[&str] = &[
    "runtime.action.requested",
    "runtime.action.succeeded",
    "runtime.action.failed",
    "runtime.action.deadline-reached",
    "runtime.reconcile-decision",
    "runtime.restart-window-reset",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Facts {
    policy: Policy,
    gate: Gate,
    run: Run,
    generation: Generation,
    step: Step,
    desired: Desired,
    observed: Observed,
    domain: Domain,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    version: String,
    complete: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Gate {
    revision: String,
    name: String,
    subject: String,
    path: String,
    operator: String,
    expected: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Run {
    id: String,
    mission: String,
    revision: String,
    generation: String,
    status: String,
    phase: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Generation {
    id: String,
    run: String,
    revision: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    id: String,
    generation: String,
    name: String,
    status: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Desired {
    claim: String,
    subject: String,
    kind: String,
    host: String,
    lifecycle: String,
    restart: String,
    run: String,
    generation: String,
    step: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Observed {
    claim: String,
    origin: String,
    status: String,
    exit_code: i64,
    incarnation: String,
    evidence: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Domain {
    complete: bool,
    origin: String,
    kinds: Vec<String>,
}

// Byte, depth and allocation-marker limits precede serde's allocation. Count only syntax,
// including object/array openings and value separators, outside escaped string contents.
fn preflight(raw: &str) -> Result<()> {
    ensure!(raw.len() <= INPUT_BYTES, "terminal input byte cap");
    let (mut quoted, mut escaped, mut depth, mut markers) = (false, false, 0usize, 0usize);
    for byte in raw.bytes() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => quoted = true,
            b'{' | b'[' => {
                depth += 1;
                markers += 1;
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1).context("terminal input syntax")?;
            }
            b',' => markers += 1,
            _ => (),
        }
        ensure!(depth <= 8 && markers <= 64, "terminal input structure cap");
    }
    ensure!(!quoted && depth == 0, "terminal input syntax");
    Ok(())
}

fn id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control),
        "terminal identifier refused"
    );
    Ok(())
}

fn expanded_subject(f: &Facts) -> Result<String> {
    let mut subject = f.gate.subject.clone();
    // Fixed run bindings, no assignee/attempt, inputs, parent, workspace or environment.
    for (variable, value) in [
        ("ST_MISSION_RUN", f.run.id.as_str()),
        (
            "ST_RUN_GENERATION",
            f.generation
                .id
                .strip_prefix("run-generation/")
                .unwrap_or(&f.generation.id),
        ),
        ("ST_STEP", f.step.name.as_str()),
        ("ST_STEP_RUN", f.step.id.as_str()),
        ("ST_MISSION", f.run.mission.as_str()),
        ("ST_MISSION_REVISION", f.run.revision.as_str()),
    ] {
        ensure!(!value.contains('$'), "nested terminal binding refused");
        subject = subject.replace(&format!("${{{variable}}}"), value);
        ensure!(subject.len() <= 512, "expanded terminal subject cap");
    }
    ensure!(
        !subject.contains('$') && subject.starts_with("exec/"),
        "terminal subject expansion refused"
    );
    Ok(subject)
}

/// None means no proven negative, never healthy. Unsupported inputs fence the namespace.
pub(crate) fn witness(value: &Value) -> Result<Option<String>> {
    let raw = value
        .as_str()
        .context("terminal input must be bounded encoded facts")?;
    preflight(raw)?;
    let f: Facts = serde_json::from_str(raw).context("terminal facts decode")?;
    for value in [
        &f.policy.version,
        &f.gate.revision,
        &f.gate.subject,
        &f.gate.path,
        &f.gate.operator,
        &f.run.id,
        &f.run.mission,
        &f.run.revision,
        &f.run.generation,
        &f.run.status,
        &f.run.phase,
        &f.generation.id,
        &f.generation.run,
        &f.generation.revision,
        &f.step.id,
        &f.step.generation,
        &f.step.name,
        &f.step.status,
        &f.desired.claim,
        &f.desired.subject,
        &f.desired.kind,
        &f.desired.host,
        &f.desired.lifecycle,
        &f.desired.restart,
        &f.desired.run,
        &f.desired.generation,
        &f.desired.step,
        &f.observed.claim,
        &f.observed.origin,
        &f.observed.status,
        &f.observed.incarnation,
        &f.domain.origin,
    ] {
        id(value)?;
    }
    id(&f.gate.name)?;
    ensure!(f.gate.name.len() <= 128, "terminal gate name cap");
    ensure!(
        f.policy.version == "fixture-selected.v1" && f.policy.complete && f.domain.complete,
        "terminal source selection incomplete"
    );
    ensure!(
        f.gate.path == "exit_code" && f.gate.operator == "is",
        "terminal gate unsupported"
    );
    ensure!(
        f.gate.revision == f.run.revision
            && f.generation.revision == f.run.revision
            && f.generation.run == f.run.id
            && f.run.generation == f.generation.id
            && f.step.generation == f.generation.id
            && f.desired.run == f.run.id
            && f.desired.generation == f.generation.id
            && f.desired.step == f.step.id,
        "terminal selected binding mismatch"
    );
    ensure!(
        expanded_subject(&f)? == f.desired.subject,
        "terminal selected subject mismatch"
    );
    ensure!(
        f.desired.kind == "exec"
            && f.desired.lifecycle == "service"
            && f.desired.restart == "never",
        "terminal declaration unsupported"
    );
    ensure!(
        f.domain.origin == f.observed.origin
            && f.domain.kinds.len() <= 7
            && f.domain.kinds.iter().any(|kind| kind == "runtime.observed")
            && f.domain
                .kinds
                .iter()
                .all(|kind| kind == "runtime.observed" || NON_SELECTING.contains(&kind.as_str())),
        "terminal actual domain unsupported"
    );
    ensure!(
        f.observed.evidence.len() <= 16,
        "terminal evidence token cap"
    );
    for token in &f.observed.evidence {
        id(token)?;
    }
    ensure!(
        f.observed.evidence.contains(&f.desired.claim),
        "terminal launch evidence incomplete"
    );
    if !matches!(f.run.status.as_str(), "running" | "standing" | "blocked")
        || !matches!(f.step.status.as_str(), "claimed" | "working" | "verifying")
        || !matches!(
            f.observed.status.as_str(),
            "exited" | "vanished" | "stopped"
        )
        || f.observed.exit_code == f.gate.expected
    {
        return Ok(None);
    }
    let text = format!(
        "field gate `{}` cannot pass: exec `{}` is {} with exit code {} and will not restart; expected `exit_code` is {}",
        f.gate.name, f.desired.subject, f.observed.status, f.observed.exit_code, f.gate.expected
    );
    ensure!(text.len() <= 1024, "terminal witness byte cap");
    Ok(Some(text))
}

pub(crate) struct TerminalGates;
impl Operator for TerminalGates {
    fn name(&self) -> &'static str {
        VIEW
    }
    fn fingerprint(&self) -> &'static str {
        "dormant.v1;eight-fixture-facts;runtime-observed-single-origin+six-nonselecting;64-members;16-rendered;no-native-authority"
    }
    fn source(&self) -> &'static str {
        SOURCE
    }
    fn create_schema(&self, c: &Connection) -> Result<()> {
        c.execute_batch("CREATE TABLE test_terminal_meta(namespace TEXT PRIMARY KEY, members INTEGER NOT NULL CHECK(members BETWEEN 0 AND 64), pending INTEGER NOT NULL DEFAULT 0) WITHOUT ROWID;
            CREATE TABLE test_terminal_members(namespace TEXT NOT NULL,key TEXT NOT NULL,body TEXT NOT NULL CHECK(length(CAST(body AS BLOB))<=1024),PRIMARY KEY(namespace,key)) WITHOUT ROWID;")?;
        Ok(())
    }
    fn apply(&self, tx: &Transaction<'_>, ns: &Namespace, rows: &[Mutation]) -> Result<bool> {
        ensure!(rows.len() <= 16, "terminal changed-key cap");
        let mut members: Option<usize> = tx
            .query_row(
                "SELECT members FROM test_terminal_meta WHERE namespace=?1",
                [ns.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        if members.is_none() {
            tx.execute(
                "INSERT INTO test_terminal_meta(namespace,members) VALUES(?1,0)",
                [ns.as_str()],
            )?;
            members = Some(0);
        }
        let mut count = members.context("terminal membership header")?;
        let mut changed = false;
        for row in rows {
            id(&row.key)?;
            let next = row.new.as_ref().map(witness).transpose()?.flatten();
            let old: Option<String> = tx
                .query_row(
                    "SELECT body FROM test_terminal_members WHERE namespace=?1 AND key=?2",
                    params![ns.as_str(), row.key],
                    |r| r.get(0),
                )
                .optional()?;
            if old == next {
                continue;
            }
            if old.is_none() && next.is_some() {
                ensure!(
                    count < MEMBERS,
                    "terminal membership cap; current evidence fenced"
                );
                count += 1;
            } else if old.is_some() && next.is_none() {
                count = count
                    .checked_sub(1)
                    .context("terminal membership underflow")?;
            }
            if let Some(body) = next {
                tx.execute("INSERT INTO test_terminal_members VALUES(?1,?2,?3) ON CONFLICT(namespace,key) DO UPDATE SET body=excluded.body", params![ns.as_str(), row.key, body])?;
            } else {
                tx.execute(
                    "DELETE FROM test_terminal_members WHERE namespace=?1 AND key=?2",
                    params![ns.as_str(), row.key],
                )?;
            }
            changed = true;
        }
        if changed {
            tx.execute(
                "UPDATE test_terminal_meta SET members=?2 WHERE namespace=?1",
                params![ns.as_str(), count],
            )?;
        }
        Ok(changed)
    }
    fn validate_publication(&self, tx: &Transaction<'_>, ns: &Namespace) -> Result<()> {
        let (members, pending): (usize, bool) = tx.query_row(
            "SELECT members,pending FROM test_terminal_meta WHERE namespace=?1",
            [ns.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            members <= MEMBERS && !pending,
            "terminal publication incomplete"
        );
        Ok(())
    }
    fn reclaim(&self, tx: &Transaction<'_>, ns: &Namespace, rows: usize) -> Result<bool> {
        ensure!(rows > 0 && rows <= MEMBERS, "terminal reclaim bound");
        let removed = tx.execute("DELETE FROM test_terminal_members WHERE namespace=?1 AND key IN (SELECT key FROM test_terminal_members WHERE namespace=?1 ORDER BY key LIMIT ?2)", params![ns.as_str(), rows])?;
        if removed == rows {
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM test_terminal_meta WHERE namespace=?1",
            [ns.as_str()],
        )?;
        Ok(true)
    }
}

#[cfg(test)]
pub(crate) mod tests;
