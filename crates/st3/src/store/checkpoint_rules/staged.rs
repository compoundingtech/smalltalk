//! `plan_drops` over a staged set (#2492): the same decisions, in the same order, reading the
//! staging file one slot or seat at a time instead of holding the set.
use super::*;
use smallclaims::store::checkpoint_staging::StagedPlanner;

/// Each distinct value of `column` in the staging claims, in order. The column's index seeks
/// each next value, so this holds one value at a time.
fn for_each_group(
    staging: &Connection,
    column: &str,
    mut visit: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let next = format!(
        "SELECT {column} FROM claims WHERE {column} IS NOT NULL AND {column} > ?1
         ORDER BY {column} LIMIT 1"
    );
    let mut after = String::new();
    // An empty value sorts first; take it before seeking past it.
    let mut first = true;
    loop {
        let value: Option<String> = if first {
            staging
                .query_row(
                    /* sqlite-large-value: allow-for-now lv-hc76eaa2b0347 owner=st3::store::checkpoint_rules::staged TODO=trace-statement-and-value-representation */ &format!("SELECT {column} FROM claims WHERE {column} IS NOT NULL ORDER BY {column} LIMIT 1"),
                    [],
                    |row| row.get(0),
                )
                .optional()?
        } else {
            staging.query_row(/* sqlite-large-value: allow-for-now lv-h91b29d9f2f8e owner=st3::store::checkpoint_rules::staged TODO=trace-statement-and-value-representation */ &next, [&after], |row| row.get(0)).optional()?
        };
        first = false;
        let Some(value) = value else {
            return Ok(());
        };
        visit(&value)?;
        after = value;
    }
}

/// One staged slot member: its position, kind, field names, acceptance time and marks.
struct Member {
    pos: i64,
    id: String,
    kind: String,
    fields: Vec<String>,
    accepted_at: u128,
    status_keep: bool,
    dropped: bool,
}

fn slot_members(staging: &Connection, slot: &str) -> Result<Vec<Member>> {
    Ok(staging
        .prepare_cached(
            "SELECT pos, id, kind, fields, accepted_at, status_keep, dropped
             FROM claims WHERE slot=?1 ORDER BY pos",
        )?
        .query_map([slot], |row| {
            Ok(Member {
                pos: row.get(0)?,
                id: row.get(1)?,
                kind: row.get(2)?,
                fields: serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or_default(),
                accepted_at: u128::try_from(row.get::<_, i64>(4)?).unwrap_or(0),
                status_keep: row.get(5)?,
                dropped: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// See `plan_drops`; each step names the step there it reproduces.
pub(crate) fn plan_staged(planner: &StagedPlanner<'_>) -> Result<()> {
    let staging = planner.staging();
    let cut = planner.cut_unix_ms();

    // The status-history rule, one seat at a time.
    let mut keep_status = staging.prepare_cached(/* sqlite-large-value: allow-for-now lv-ha290bafdf8dc owner=st3::store::checkpoint_rules::staged TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET status_keep=1 WHERE pos=?1")?;
    for_each_group(staging, "seat", |seat| {
        let members = staging
            .prepare_cached("SELECT pos, id FROM claims WHERE seat=?1 ORDER BY pos")?
            .query_map([seat], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let ids = members.iter().map(|(_, id)| id.clone()).collect::<Vec<_>>();
        let claims = planner.claims(&ids)?;
        let sources = claims.iter().collect::<Vec<_>>();
        for position in status_keep_positions(&sources, cut) {
            keep_status.execute(/* sqlite-large-value: allow-for-now lv-hdac18e56a1d6 owner=st3::store::checkpoint_rules::staged TODO=trace-statement-and-value-representation */ [members[position].0])?;
        }
        Ok(())
    })?;
    drop(keep_status);

    // The newest measurement of each account group, across every seat, for the limits rule.
    let mut limits_newest = BTreeMap::new();
    {
        let mut statement = staging.prepare(
            "SELECT limits_group, MAX(limits_measured) FROM claims
             WHERE first=1 AND kind='harness.limits' AND limits_group IS NOT NULL
             GROUP BY limits_group",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let group: (String, String, String, bool) = serde_json::from_str(&row.get::<_, String>(0)?)?;
            limits_newest.insert(group, u64::try_from(row.get::<_, i64>(1)?)?);
        }
    }

    // The slot rules, one slot at a time.
    let mut drop_claim = staging.prepare_cached(/* sqlite-large-value: allow-for-now lv-ha15a45ac783a owner=st3::store::checkpoint_rules::staged TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET dropped=1 WHERE pos=?1")?;
    let mut closed = staging.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM closed WHERE subject=?1 AND request=?2)",
    )?;
    for_each_group(staging, "slot", |slot_text| {
        let (rule, slot) = slot_from_text(slot_text)
            .ok_or_else(|| anyhow::anyhow!("staged checkpoint slot {slot_text} is not a slot"))?;
        let members = slot_members(staging, slot_text)?;
        let claims = if rule.reads_bodies() {
            planner.claims(&members.iter().map(|member| member.id.clone()).collect::<Vec<_>>())?
        } else {
            Vec::new()
        };
        let slot_claims = claims.iter().collect::<Vec<_>>();
        let is_closed = rule == Rule::Deferral && {
            let request = slot.last().cloned().unwrap_or_default();
            closed.query_row(/* sqlite-large-value: allow-for-now lv-hcda3dd34b22c owner=st3::store::checkpoint_rules::staged TODO=trace-statement-and-value-representation */ params![slot[0], request], |row| row.get::<_, bool>(0))?
        };
        let mut keep = rule_keep(rule, members.len(), &slot_claims, is_closed, &limits_newest, cut);
        if rule != Rule::LoopState {
            keep.extend(carriers(&members.iter().map(|member| member.fields.clone()).collect::<Vec<_>>()));
        }
        for (position, member) in members.iter().enumerate() {
            if !keep.contains(&position) && !member.status_keep && rule.old_enough(member.accepted_at, cut) {
                drop_claim.execute(/* sqlite-large-value: allow-for-now lv-h912c32c784af owner=st3::store::checkpoint_rules::staged TODO=trace-statement-and-value-representation */ [member.pos])?;
            }
        }
        Ok(())
    })?;
    drop((drop_claim, closed));

    // D4: guards.
    staging.execute_batch(
        /* sqlite-large-value: allow-for-now lv-h6fcd14eec7cb owner=st3::store::checkpoint_rules::staged TODO=chunk-or-reference-variable-columns */ "CREATE TEMP TABLE IF NOT EXISTS guarded_kinds(kind TEXT PRIMARY KEY);
         DELETE FROM temp.guarded_kinds;",
    )?;
    {
        let kinds = staging
            .prepare("SELECT DISTINCT kind FROM claims")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut insert = staging.prepare(/* sqlite-large-value: allow-for-now lv-h0122510ad671 owner=st3::store::checkpoint_rules::staged TODO=bound-or-reference-values-and-reclamation */ "INSERT INTO temp.guarded_kinds(kind) VALUES (?1)")?;
        for kind in kinds.iter().filter(|kind| guarded_kind(kind)) {
            insert.execute(/* sqlite-large-value: allow-for-now lv-h51d35d15f113 owner=st3::store::checkpoint_rules::staged TODO=trace-statement-and-value-representation */ [kind])?;
        }
    }
    staging.execute_batch(
        /* sqlite-large-value: allow-for-now lv-h77dcc015a478 owner=st3::store::checkpoint_rules::staged TODO=chunk-or-reference-exact-wire-values */ "UPDATE claims SET dropped=0 WHERE dropped=1 AND (
             substr(actor, 1, 7)='person/'
             OR kind IN (SELECT kind FROM temp.guarded_kinds)
             OR valid=0
             OR protected=1
             OR id IN (SELECT id FROM claims GROUP BY id HAVING COUNT(*) > 1)
             OR id IN (SELECT id FROM cited)
             OR sequence=(SELECT MAX(envelopes.sequence) FROM envelopes
                          WHERE envelopes.writer=claims.writer AND envelopes.included=1));",
    )?;

    // D3, shared operations and whole envelopes, until nothing changes. Each step only keeps
    // more, so the loop ends, and every witness is a kept claim. Slots, operations and
    // envelopes are each disjoint, so a step applied to all of its groups at once decides as
    // the sequential loop does.
    loop {
        let mut changed = false;
        // Newest first, so a claim kept back here can witness the ones before it.
        let mut keep = staging.prepare_cached(/* sqlite-large-value: allow-for-now lv-hc1d162fa261c owner=st3::store::checkpoint_rules::staged TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET dropped=0 WHERE pos=?1")?;
        for_each_group(staging, "slot", |slot_text| {
            let members = slot_members(staging, slot_text)?;
            let mut later = BTreeSet::new();
            let mut later_claims = 0;
            for member in members.iter().rev() {
                let mut dropped = member.dropped;
                if dropped && !witnessed_names(&member.kind, &member.fields, &later, later_claims) {
                    keep.execute(/* sqlite-large-value: allow-for-now lv-h4011aa1a60e5 owner=st3::store::checkpoint_rules::staged TODO=trace-statement-and-value-representation */ [member.pos])?;
                    dropped = false;
                    changed = true;
                }
                if !dropped {
                    later.extend(member.fields.iter().cloned());
                    later_claims += 1;
                }
            }
            Ok(())
        })?;
        drop(keep);
        let kept = staging.execute(
            /* sqlite-large-value: allow-for-now lv-hd9ce75725a5f owner=st3::store::checkpoint_rules::staged TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET dropped=0 WHERE dropped=1 AND operation_id IS NOT NULL
               AND EXISTS(SELECT 1 FROM claims other
                          WHERE other.operation_id=claims.operation_id AND other.dropped=0)",
            [],
        )?;
        let kept = kept
            + staging.execute(
                /* sqlite-large-value: allow-for-now lv-h8a1494867aac owner=st3::store::checkpoint_rules::staged TODO=chunk-or-reference-exact-wire-values */ "UPDATE claims SET dropped=0 WHERE dropped=1
                   AND EXISTS(SELECT 1 FROM claims other
                              WHERE other.writer=claims.writer AND other.sequence=claims.sequence
                                AND other.envelope_hash=claims.envelope_hash AND other.dropped=0)",
                [],
            )?;
        let kept = kept
            + staging.execute(
                /* sqlite-large-value: allow-for-now lv-ha55bdedefa1d owner=st3::store::checkpoint_rules::staged TODO=chunk-or-reference-exact-wire-values */ "UPDATE claims SET dropped=0 WHERE dropped=1
                   AND (SELECT records FROM envelopes
                        WHERE envelopes.writer=claims.writer AND envelopes.sequence=claims.sequence
                          AND envelopes.envelope_hash=claims.envelope_hash)
                     <> (SELECT COUNT(*) FROM claims other
                         WHERE other.writer=claims.writer AND other.sequence=claims.sequence
                           AND other.envelope_hash=claims.envelope_hash)",
                [],
            )?;
        if !changed && kept == 0 {
            return Ok(());
        }
    }
}
