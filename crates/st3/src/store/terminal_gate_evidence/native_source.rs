//! Private, explicitly attached native-source fixture. Never built outside cfg(test).
//! No Runtime hook, production schema, populated bootstrap or Doctor wiring.
//! Selection is deliberately local, unmanaged and bounded; it is not graph certification.
use super::*;
use crate::model::{DesiredSubject, GateSpec, MemberKind, MissionSpec};
use smallclaims::ivm::install::{Installer, Limits, Outcome, ScanPage};
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

const FINGERPRINT: &str = "private-native.v1;one-literal-gate;empty-local-unmanaged;octet-header;16-subject+16-batch-rows;no-repair;native-pending-revision";
const GUARD: &str = "test_native_terminal_guard";
const RAW_BYTES: usize = 16 * 1024;
const SOURCE_BYTES: usize = 256 * 1024;
const ROWS: usize = 16;

#[derive(Clone, serde::Serialize)]
struct Binding {
    origin: String,
    mission: String,
    run: String,
    generation: String,
    step: String,
    path: String,
    gate: String,
    exec: String,
}
impl Binding {
    fn validate(&self) -> Result<()> {
        for value in [
            &self.origin,
            &self.mission,
            &self.run,
            &self.generation,
            &self.step,
            &self.path,
            &self.gate,
            &self.exec,
        ] {
            id(value)?;
        }
        ensure!(
            self.exec.starts_with("exec/") && self.gate.len() <= 128,
            "native binding refused"
        );
        Ok(())
    }
    fn identity(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }
}

struct NativeSource {
    binding: Binding,
    installer: Installer,
}

/// All table/column names below are fixed source identifiers, never caller input.
fn bounded_row(
    tx: &Transaction<'_>,
    table: &str,
    rowid: i64,
    columns: &[(&str, usize)],
    bytes: &mut usize,
) -> Result<Vec<Option<String>>> {
    let headers = columns
        .iter()
        .map(|(c, _)| format!("typeof({c}),octet_length({c})"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT {headers} FROM {table} WHERE rowid=?1");
    let sizes = tx.query_row(&sql, [rowid], |row| {
        columns
            .iter()
            .enumerate()
            .map(|(n, _)| {
                Ok((
                    row.get::<_, String>(2 * n)?,
                    row.get::<_, Option<usize>>(2 * n + 1)?,
                ))
            })
            .collect::<rusqlite::Result<Vec<_>>>()
    })?;
    for ((kind, size), (_, cap)) in sizes.iter().zip(columns) {
        ensure!(kind == "text" || kind == "null", "native text type refused");
        if let Some(size) = size {
            ensure!(*size <= *cap, "native raw byte cap");
            *bytes = bytes.checked_add(*size).context("native byte counter")?;
            ensure!(*bytes <= SOURCE_BYTES, "native total source byte cap");
        }
    }
    let guard = columns
        .iter()
        .map(|(c, cap)| {
            format!("({c} IS NULL OR (typeof({c})='text' AND octet_length({c})<={cap}))")
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let names = columns
        .iter()
        .map(|(c, _)| *c)
        .collect::<Vec<_>>()
        .join(",");
    tx.query_row(
        &format!("SELECT {names} FROM {table} WHERE rowid=?1 AND {guard}"),
        [rowid],
        |row| (0..columns.len()).map(|n| row.get(n)).collect(),
    )
    .map_err(Into::into)
}
fn point(tx: &Transaction<'_>, table: &str, column: &str, key: &str) -> Result<Option<i64>> {
    Ok(tx
        .query_row(
            &format!("SELECT rowid FROM {table} WHERE {column}=?1"),
            [key],
            |r| r.get(0),
        )
        .optional()?)
}
fn required(values: &[Option<String>], n: usize) -> Result<&str> {
    values[n].as_deref().context("native required field absent")
}
fn decode(raw: &str) -> Result<Value> {
    ensure!(raw.len() <= RAW_BYTES, "native raw byte cap");
    let (mut quoted, mut escaped, mut depth, mut markers) = (false, false, 0usize, 0usize);
    for b in raw.bytes() {
        if quoted {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                quoted = false;
            }
            continue;
        }
        match b {
            b'"' => quoted = true,
            b'{' | b'[' => {
                depth += 1;
                markers += 1;
            }
            b'}' | b']' => depth = depth.checked_sub(1).context("native JSON structure")?,
            b',' => markers += 1,
            _ => (),
        }
        ensure!(depth <= 16 && markers <= 256, "native JSON allocation cap");
    }
    ensure!(!quoted && depth == 0, "native JSON structure");
    Ok(serde_json::from_str(raw)?)
}

impl NativeSource {
    fn attach(tx: &Transaction<'_>, binding: Binding) -> Result<Self> {
        binding.validate()?;
        // No scan/seed of an existing source, including existing operational projections.
        for table in [
            "claims",
            "batches",
            "checkpoint_claims",
            "checkpoint_envelopes",
            "replica_records",
            "desired",
            "mission_revisions",
            "mission_definitions",
            "mission_runs",
            "run_generations",
            "step_runs",
        ] {
            let any: bool = tx.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {table} LIMIT 1)"),
                [],
                |r| r.get(0),
            )?;
            ensure!(!any, "native source is nonempty");
        }
        tx.execute_batch(
            "CREATE TABLE test_native_terminal_guard(
            id INTEGER PRIMARY KEY CHECK(id=1),identity TEXT NOT NULL,
            origin TEXT NOT NULL,mission TEXT NOT NULL,run_id TEXT NOT NULL,
            generation_id TEXT NOT NULL,step TEXT NOT NULL,exec TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK(revision>=0),
            pending INTEGER NOT NULL CHECK(pending IN (0,1)),gap TEXT,payload TEXT);",
        )?;
        tx.execute(
            "INSERT INTO test_native_terminal_guard VALUES(1,?1,?2,?3,?4,?5,?6,?7,0,0,NULL,NULL)",
            params![
                binding.identity()?,
                binding.origin,
                binding.mission,
                binding.run.trim_start_matches("mission-run/"),
                binding.generation.trim_start_matches("run-generation/"),
                binding.step,
                binding.exec
            ],
        )?;
        let installer = Installer::new(vec![Box::new(TerminalGates)])?;
        installer.create_schema(tx)?;
        installer.register_source(tx, SOURCE, FINGERPRINT, 1)?;
        let job = installer.start(
            tx,
            VIEW,
            Limits {
                page_rows: 16,
                page_bytes: 16 * 1024,
                pending_rows: 64,
                pending_bytes: 256 * 1024,
                total_rows: 128,
                callback_ms: 1000,
                lifetime_ms: 60_000,
            },
            0,
        )?;
        let position = installer.position(tx, SOURCE)?;
        ensure!(
            installer.scan(
                tx,
                &ScanPage {
                    job: job.clone(),
                    expected_cursor: vec![],
                    next_cursor: vec![],
                    position,
                    rows: vec![],
                    finished: true,
                },
                1
            )? == Outcome::Progress,
            "native empty scan"
        );
        ensure!(
            installer.catch_up(tx, &job, 2)? == Outcome::Published,
            "native empty publication"
        );
        let source = Self { binding, installer };
        source.install_triggers(tx)?;
        Ok(source)
    }

    fn install_triggers(&self, tx: &Transaction<'_>) -> Result<()> {
        // Table predicates are generated from fixed source names. INSERT includes BEFORE:
        // SQLite REPLACE may delete a colliding old row without its DELETE trigger.
        for (table, column, guard) in [
            ("desired", "subject", "exec"),
            ("mission_revisions", "mission_id", "mission"),
            ("mission_definitions", "mission_id", "mission"),
            ("mission_runs", "id", "run_id"),
            ("run_generations", "id", "generation_id"),
            ("step_runs", "subject", "step"),
            ("claims", "subject", "exec"),
        ] {
            for (event, timing, prefixes) in [
                ("INSERT", "BEFORE", vec!["NEW"]),
                ("INSERT", "AFTER", vec!["NEW"]),
                ("UPDATE", "AFTER", vec!["OLD", "NEW"]),
                ("DELETE", "AFTER", vec!["OLD"]),
            ] {
                let mut predicates = prefixes
                    .iter()
                    .map(|p| format!("{p}.{column}=(SELECT {guard} FROM {GUARD} WHERE id=1)"))
                    .collect::<Vec<_>>();
                if event == "INSERT" {
                    predicates.push(format!("EXISTS(SELECT 1 FROM {table} WHERE rowid=NEW.rowid AND {column}=(SELECT {guard} FROM {GUARD} WHERE id=1))"));
                    if table == "claims" {
                        predicates.push(format!("EXISTS(SELECT 1 FROM claims WHERE id=NEW.id AND subject=(SELECT exec FROM {GUARD} WHERE id=1))"));
                    }
                }
                if table == "claims" {
                    for p in &prefixes {
                        predicates.push(format!("EXISTS(SELECT 1 FROM claims existing INDEXED BY claims_subject_index WHERE existing.subject=(SELECT exec FROM {GUARD} WHERE id=1) AND existing.batch_id={p}.batch_id LIMIT 1)"));
                        predicates.push(format!("{p}.subject IN (
                            SELECT 'mission-run/'||run_id FROM {GUARD} WHERE id=1
                            UNION ALL SELECT 'run-generation/'||generation_id FROM {GUARD} WHERE id=1
                            UNION ALL SELECT step FROM {GUARD} WHERE id=1
                            UNION ALL SELECT 'mission/'||mission FROM {GUARD} WHERE id=1)"));
                        predicates.push(format!("{p}.kind='owned-set.revised'"));
                    }
                }
                let when = predicates.join(" OR ");
                let gap = if table == "claims" {
                    let receipt = prefixes
                        .iter()
                        .map(|p| format!("{p}.kind='owned-set.revised'"))
                        .collect::<Vec<_>>()
                        .join(" OR ");
                    format!(
                        "gap=CASE WHEN {receipt} THEN 'unsupported owned-set authority' ELSE gap END,"
                    )
                } else {
                    String::new()
                };
                tx.execute_batch(&format!(
                    "CREATE TRIGGER test_native_{table}_{timing}_{event}
                    {timing} {event} ON {table} WHEN {when} BEGIN
                    UPDATE {GUARD} SET {gap} revision=revision+1,pending=1 WHERE id=1;
                    UPDATE ivm_install_sources SET revision=revision+1 WHERE name='{SOURCE}';
                    END;"
                ))?;
            }
        }
        // Rank/membership dependencies. Unsupported persisted repair/checkpoint input fences.
        for table in [
            "batches",
            "replica_records",
            "checkpoint_claims",
            "checkpoint_envelopes",
        ] {
            for (event, timing, prefixes) in [
                ("INSERT", "BEFORE", vec!["NEW"]),
                ("INSERT", "AFTER", vec!["NEW"]),
                ("UPDATE", "AFTER", vec!["OLD", "NEW"]),
                ("DELETE", "AFTER", vec!["OLD"]),
            ] {
                let predicates=prefixes.iter().map(|p| {
                    if table=="batches" {
                        format!("EXISTS(SELECT 1 FROM claims WHERE batch_id={p}.id AND subject=(SELECT exec FROM {GUARD} WHERE id=1)) OR EXISTS(SELECT 1 FROM batches old JOIN claims ON claims.batch_id=old.id WHERE old.rowid={p}.rowid AND claims.subject=(SELECT exec FROM {GUARD} WHERE id=1))")
                    } else { "1".to_owned() }
                }).collect::<Vec<_>>().join(" OR ");
                tx.execute_batch(&format!("CREATE TRIGGER test_native_{table}_{timing}_{event}
                    {timing} {event} ON {table} WHEN {predicates} BEGIN
                    UPDATE {GUARD} SET revision=revision+1,pending=1,gap='unsupported native rank/repair/checkpoint mutation' WHERE id=1;
                    UPDATE ivm_install_sources SET revision=revision+1 WHERE name='{SOURCE}';
                    END;"))?;
            }
        }
        // The binding is fixed for the entire fixture lifetime. Metadata changes cannot
        // redirect the capture predicates while leaving the old witness eligible.
        tx.execute_batch(&format!("CREATE TRIGGER test_native_guard_binding
            AFTER UPDATE OF identity,origin,mission,run_id,generation_id,step,exec ON {GUARD}
            BEGIN
            UPDATE {GUARD} SET revision=revision+1,pending=1,gap='native binding replaced' WHERE id=1;
            UPDATE ivm_install_sources SET revision=revision+1 WHERE name='{SOURCE}';
            END;
            CREATE TRIGGER test_native_guard_replacement BEFORE INSERT ON {GUARD}
            BEGIN SELECT RAISE(ABORT,'native binding replacement refused'); END;"))?;
        Ok(())
    }

    fn state(&self, tx: &Transaction<'_>) -> Result<(i64, bool, Option<String>)> {
        let (revision,pending,gap,identity):(i64,bool,Option<String>,String)=tx.query_row(
            "SELECT revision,pending,gap,identity FROM test_native_terminal_guard WHERE id=1 AND typeof(identity)='text' AND octet_length(identity)<=16384 AND (gap IS NULL OR (typeof(gap)='text' AND octet_length(gap)<=512))",
            [],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        ensure!(
            identity == self.binding.identity()?,
            "native binding replaced"
        );
        Ok((revision, pending, gap))
    }

    /// The fixture calls this explicitly in its writer transaction, after ALL native writes.
    /// No existing Store method/Runtime/reader calls this. A gap is sticky, never auto-seeded.
    fn finish(&self, tx: &Transaction<'_>) -> Result<()> {
        let (revision, pending, gap) = self.state(tx)?;
        if !pending && gap.is_none() {
            return Ok(());
        }
        if let Some(gap) = gap {
            self.installer.source_gap(tx, SOURCE, &gap)?;
            return Ok(());
        }
        let result = (|| {
            let next = self.extract(tx)?;
            let (_, _, payload) = {
                let v = bounded_row(
                    tx,
                    GUARD,
                    1,
                    &[
                        ("identity", RAW_BYTES),
                        ("gap", 512),
                        ("payload", RAW_BYTES),
                    ],
                    &mut 0,
                )?;
                (v[0].clone(), v[1].clone(), v[2].clone())
            };
            ensure!(
                self.state(tx)?.0 == revision,
                "native capture changed during extraction"
            );
            self.installer.record(
                tx,
                SOURCE,
                &Mutation {
                    key: "gate/native".into(),
                    old: payload.map(Value::String),
                    new: next.as_ref().map(|v| Value::String(v.to_string())),
                },
            )?;
            ensure!(
                self.state(tx)?.0 == revision,
                "native capture changed during dispatch"
            );
            // A malformed/over-cap operator may have fenced the namespace without SQL error.
            self.installer.root(tx, VIEW)?;
            tx.execute("UPDATE test_native_terminal_guard SET pending=0,payload=?1 WHERE id=1 AND revision=?2",
                params![next.map(|v|v.to_string()),revision])?;
            Ok::<_, anyhow::Error>(())
        })();
        if let Err(error) = result {
            let reason = error.to_string().chars().take(256).collect::<String>();
            tx.execute(
                "UPDATE test_native_terminal_guard SET gap=?1,pending=1 WHERE id=1",
                [&reason],
            )?;
            self.installer.source_gap(tx, SOURCE, &reason)?;
        }
        Ok(())
    }

    fn extract(&self, tx: &Transaction<'_>) -> Result<Option<Value>> {
        let b = &self.binding;
        let mut bytes = 0;
        let Some(run) = point(
            tx,
            "mission_runs",
            "id",
            b.run.trim_start_matches("mission-run/"),
        )?
        else {
            return Ok(None);
        };
        let r = bounded_row(
            tx,
            "mission_runs",
            run,
            &[
                ("mission_id", 512),
                ("current_generation_id", 512),
                ("status", 512),
                ("phase", 512),
            ],
            &mut bytes,
        )?;
        if required(&r, 0)? != b.mission
            || required(&r, 1)? != b.generation.trim_start_matches("run-generation/")
        {
            return Ok(None);
        }
        let Some(generation) = point(tx, "run_generations", "id", required(&r, 1)?)? else {
            return Ok(None);
        };
        let g = bounded_row(
            tx,
            "run_generations",
            generation,
            &[("run_id", 512), ("revision", 512)],
            &mut bytes,
        )?;
        if required(&g, 0)? != b.run.trim_start_matches("mission-run/") {
            return Ok(None);
        }
        let Some(step) = point(tx, "step_runs", "subject", &b.step)? else {
            return Ok(None);
        };
        let s = bounded_row(
            tx,
            "step_runs",
            step,
            &[
                ("run_id", 512),
                ("generation_id", 512),
                ("step_path", 512),
                ("status", 512),
            ],
            &mut bytes,
        )?;
        if required(&s, 0)? != required(&g, 0)?
            || required(&s, 1)? != required(&r, 1)?
            || required(&s, 2)? != b.path
        {
            return Ok(None);
        }
        let revision: Option<i64> = tx
            .query_row(
                "SELECT rowid FROM mission_revisions WHERE mission_id=?1 AND revision=?2",
                params![b.mission, required(&g, 1)?],
                |r| r.get(0),
            )
            .optional()?;
        let Some(revision) = revision else {
            return Ok(None);
        };
        let m = bounded_row(
            tx,
            "mission_revisions",
            revision,
            &[("body", RAW_BYTES)],
            &mut bytes,
        )?;
        let mission: MissionSpec = serde_json::from_value(decode(required(&m, 0)?)?)?;
        if mission.id != b.mission || mission.revision != required(&g, 1)? {
            return Ok(None);
        }
        let Some(spec) = mission.steps.get(&b.path) else {
            return Ok(None);
        };
        let gates = spec
            .gates
            .iter()
            .filter_map(|gate| match gate {
                GateSpec::Field {
                    name,
                    path,
                    subject,
                    operator,
                    value,
                } if name == &b.gate => Some((name, path, subject, operator, value)),
                _ => None,
            })
            .collect::<Vec<_>>();
        if gates.len() != 1 {
            return Ok(None);
        }
        let (name, path, subject, operator, value) = gates[0];
        // Native expansion semantics are not certified here: the first slice is literal-only.
        if subject.contains('$') || subject != &b.exec || path != "exit_code" || operator != "is" {
            return Ok(None);
        }
        let Some(expected) = value.as_i64() else {
            return Ok(None);
        };
        let Some(desired) = point(tx, "desired", "subject", &b.exec)? else {
            return Ok(None);
        };
        let d = bounded_row(
            tx,
            "desired",
            desired,
            &[
                ("kind", 512),
                ("claim_id", 512),
                ("body", RAW_BYTES),
                ("member", RAW_BYTES),
                ("owner_run", 512),
                ("owner_generation", 512),
                ("owner_step", 512),
            ],
            &mut bytes,
        )?;
        if d[4].as_deref() != Some(b.run.as_str())
            || d[5].as_deref() != Some(b.generation.as_str())
            || d[6].as_deref() != Some(b.step.as_str())
        {
            return Ok(None);
        }
        let Some(declaration) = point(tx, "claims", "id", required(&d, 1)?)? else {
            return Ok(None);
        };
        let declaration_fields = bounded_row(
            tx,
            "claims",
            declaration,
            &[
                ("subject", 512),
                ("kind", 512),
                ("origin", 512),
                ("body", RAW_BYTES),
            ],
            &mut bytes,
        )?;
        if required(&declaration_fields, 0)? != b.exec
            || required(&declaration_fields, 1)? != "intent.desired"
            || required(&declaration_fields, 2)? != b.origin
        {
            return Ok(None);
        }
        let raw = decode(required(&declaration_fields, 3)?)?;
        if raw.get("owned_set").is_some() {
            return Ok(None);
        }
        let desired_claim: DesiredSubject = serde_json::from_value(raw)?;
        let member: crate::model::MemberSpec = serde_json::from_value(decode(required(&d, 3)?)?)?;
        if desired_claim.subject != b.exec
            || desired_claim.kind != required(&d, 0)?
            || desired_claim.desired != decode(required(&d, 2)?)?
            || desired_claim.member.as_ref() != Some(&member)
            || desired_claim.owner_run.as_deref() != Some(b.run.as_str())
            || desired_claim.owner_generation.as_deref() != Some(b.generation.as_str())
            || desired_claim.owner_step.as_deref() != Some(b.step.as_str())
            || member.kind != MemberKind::Exec
            || member.host != b.origin
        {
            return Ok(None);
        }
        let mut stmt = tx.prepare(
            "SELECT rowid FROM claims INDEXED BY claims_subject_index WHERE subject=?1 LIMIT 17",
        )?;
        let ids = stmt
            .query_map([&b.exec], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(ids.len() <= ROWS, "native subject row cap");
        drop(stmt);
        let mut domain = BTreeSet::new();
        let mut observations = Vec::new();
        let mut batches = BTreeMap::new();
        for rowid in ids {
            let c = bounded_row(
                tx,
                "claims",
                rowid,
                &[
                    ("id", 512),
                    ("batch_id", 512),
                    ("kind", 512),
                    ("origin", 512),
                    ("accepted_at_unix_ms", 512),
                ],
                &mut bytes,
            )?;
            let kind = required(&c, 2)?;
            // Exact current ACTUAL_STATE_CLAIM exclusions; no closed-list pass for unknown kinds.
            if kind.starts_with("harness.")
                || [
                    "intent.desired",
                    "runtime.readiness-deadline-reached",
                    "reconcile.fault",
                ]
                .contains(&kind)
            {
                continue;
            }
            if required(&c, 3)? != b.origin
                || !(kind == "runtime.observed" || NON_SELECTING.contains(&kind))
            {
                return Ok(None);
            }
            domain.insert(kind.to_owned());
            if kind != "runtime.observed" {
                continue;
            }
            let batch = required(&c, 1)?.to_owned();
            if let Entry::Vacant(entry) = batches.entry(batch.clone()) {
                let Some(batch_row) = point(tx, "batches", "id", &batch)? else {
                    return Ok(None);
                };
                let fields = bounded_row(tx, "batches", batch_row, &[("origin", 512)], &mut bytes)?;
                if required(&fields, 0)? != b.origin {
                    return Ok(None);
                }
                let sequence: u64 = tx.query_row(
                    "SELECT replica_sequence FROM batches WHERE rowid=?1",
                    [batch_row],
                    |r| r.get(0),
                )?;
                let mut st=tx.prepare("SELECT store_index FROM claims INDEXED BY claims_batch_index WHERE batch_id=?1 ORDER BY store_index LIMIT 17")?;
                let positions = st
                    .query_map([&batch], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                ensure!(positions.len() <= ROWS, "native batch row cap");
                entry.insert((sequence, positions));
            }
            let replica: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM replica_records WHERE claim_id=?1 LIMIT 1)",
                [required(&c, 0)?],
                |r| r.get(0),
            )?;
            if replica {
                return Ok(None);
            }
            let (sequence, positions) = &batches[&batch];
            let position = positions
                .iter()
                .position(|v| *v == rowid)
                .context("native batch membership missing")? as u64;
            let time = required(&c, 4)?.parse::<u128>()?;
            ensure!(
                time.to_string() == required(&c, 4)?,
                "native accepted time noncanonical"
            );
            observations.push((
                (
                    time,
                    b.origin.clone(),
                    *sequence,
                    batch,
                    position,
                    required(&c, 0)?.to_owned(),
                ),
                rowid,
            ));
        }
        let Some((key, rowid)) = observations.into_iter().max_by(|a, b| a.0.cmp(&b.0)) else {
            return Ok(None);
        };
        let o = bounded_row(tx, "claims", rowid, &[("body", RAW_BYTES)], &mut bytes)?;
        let observed = decode(required(&o, 0)?)?;
        let fields = observed.get("fields").unwrap_or(&observed);
        let evidence = observed.get("evidence").and_then(Value::as_array);
        let Some(evidence) = evidence else {
            return Ok(None);
        };
        ensure!(evidence.len() <= 16, "native evidence cap");
        if evidence.iter().any(|v| v.as_str().is_none()) {
            return Ok(None);
        }
        let Some(status) = fields["status"].as_str() else {
            return Ok(None);
        };
        let Some(incarnation) = fields["incarnation_id"].as_str() else {
            return Ok(None);
        };
        let facts = serde_json::json!({
            "policy":{"version":"fixture-selected.v1","complete":true},
            "gate":{"revision":required(&g,1)?,"name":name,"subject":subject,"path":path,"operator":operator,"expected":expected},
            "run":{"id":b.run,"mission":format!("mission/{}",b.mission),"revision":required(&g,1)?,"generation":b.generation,"status":required(&r,2)?,"phase":required(&r,3)?},
            "generation":{"id":b.generation,"run":b.run,"revision":required(&g,1)?},
            "step":{"id":b.step,"generation":b.generation,"name":required(&s,2)?,"status":required(&s,3)?},
            "desired":{"claim":required(&d,1)?,"subject":b.exec,"kind":required(&d,0)?,"host":member.host,"lifecycle":member.lifecycle,"restart":member.restart,"run":b.run,"generation":b.generation,"step":b.step},
            "observed":{"claim":key.5,"origin":b.origin,"status":status,"exit_code":fields["exit_code"],"incarnation":incarnation,"evidence":evidence},
            "domain":{"complete":true,"origin":b.origin,"kinds":domain},
        });
        preflight(&facts.to_string())?;
        Ok(Some(facts))
    }

    /// Fixture-only Doctor line. Public Doctor is deliberately NOT wired to this adapter.
    fn doctor_line(&self, tx: &Transaction<'_>) -> Value {
        let evidence = (|| {
            let (_, pending, gap) = self.state(tx)?;
            if let Some(gap) = gap {
                anyhow::bail!("native evidence incomplete: {gap}");
            }
            ensure!(!pending, "native evidence incomplete/pending");
            let root = self.installer.root(tx, VIEW)?;
            let body:Option<String>=tx.query_row("SELECT body FROM test_terminal_members WHERE namespace=?1 AND key='gate/native'",
                [root.namespace.as_str()],|r|r.get(0)).optional()?;
            Ok::<_, anyhow::Error>(body)
        })();
        match evidence {
            Ok(Some(body)) => {
                serde_json::json!({"name":"terminal-exec-gates","status":"warn","message":body})
            }
            Ok(None) => {
                serde_json::json!({"name":"terminal-exec-gates","status":"unknown","message":"no certified negative witness"})
            }
            Err(error) => {
                serde_json::json!({"name":"terminal-exec-gates","status":"unknown","message":error.to_string()})
            }
        }
    }
}

#[cfg(test)]
mod tests;
