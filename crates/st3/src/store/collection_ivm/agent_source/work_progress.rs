//! Scheduling evidence only: completed finite work never grants source coverage or Ready.
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use smallclaims::ivm::install::Namespace;
use std::collections::BTreeMap;

pub fn create_schema(c: &Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS st3_agent_work_progress(namespace TEXT PRIMARY KEY,counter INTEGER NOT NULL)")?;
    for table in [
        "local_agent_source_dirty",
        "local_agent_source_fanout",
        "local_agent_card_source_work",
        "local_agent_authority_dirty",
        "local_agent_lifecycle_dirty",
        "local_agent_launch_dirty",
        "local_agent_card_source_local_cut",
        "local_agent_card_usage_dirty",
        "local_agent_queue_run_work",
        "local_agent_queue_join_work",
        "local_agent_queue_step_work",
        "local_agent_queue_flag_work",
        "local_agent_queue_scope_work",
        "local_agent_queue_rank_work",
        "local_agent_queue_dirty",
    ] {
        c.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS st3_work_done_{table} AFTER DELETE ON {table} WHEN NOT EXISTS(SELECT 1 FROM local_agent_card_source_reclaim WHERE namespace=OLD.namespace) BEGIN
        INSERT INTO st3_agent_work_progress VALUES(OLD.namespace,1) ON CONFLICT(namespace) DO UPDATE SET counter=counter+1; END"))?;
    }
    for (table, condition) in [
        (
            "local_agent_source_fanout",
            "NEW.after_index>OLD.after_index OR (NEW.after_index=OLD.after_index AND NEW.after_id>OLD.after_id)",
        ),
        ("local_agent_queue_run_work", "NEW.cursor IS NOT OLD.cursor"),
        (
            "local_agent_queue_scope_work",
            "NEW.cursor_path IS NOT OLD.cursor_path OR NEW.cursor_subject IS NOT OLD.cursor_subject",
        ),
        (
            "local_agent_queue_rank_work",
            "NEW.cursor IS NOT OLD.cursor",
        ),
        (
            "local_agent_card_source_local_cut",
            "NEW.after_index>OLD.after_index OR (NEW.after_index=OLD.after_index AND NEW.after_claim>OLD.after_claim)",
        ),
    ] {
        c.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS st3_work_advance_{table} AFTER UPDATE ON {table} WHEN {condition} BEGIN
        INSERT INTO st3_agent_work_progress VALUES(NEW.namespace,1) ON CONFLICT(namespace) DO UPDATE SET counter=counter+1; END"))?;
    }
    c.execute_batch(
        "DROP TRIGGER IF EXISTS st3_work_advance_local_agent_card_source_clock;
        DROP TRIGGER IF EXISTS st3_work_advance_local_agent_card_source_cursor;
        DROP TRIGGER IF EXISTS st3_work_begin_local_agent_card_source_cursor;",
    )?;
    Ok(())
}

pub fn counter(c: &Connection, ns: &Namespace) -> Result<u64> {
    Ok(c.query_row(
        "SELECT counter FROM st3_agent_work_progress WHERE namespace=?1",
        [ns.as_str()],
        |r| r.get(0),
    )
    .optional()?
    .unwrap_or(0))
}

#[derive(Clone)]
pub struct Seeks {
    authority: (String, String),
    lifecycle: String,
    stages: BTreeMap<String, String>,
}

/// Read only the fixed set of indexed maintenance cursors. A pending family can advance
/// and wrap within one callback; only net forward movement justifies another immediate page.
pub fn seeks(c: &Connection, ns: &Namespace) -> Result<Seeks> {
    let (agent, id, lifecycle) = c.query_row(
        "SELECT authority_agent,authority_id,lifecycle_after FROM local_agent_card_source_clock WHERE namespace=?1",
        [ns.as_str()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    )?;
    let stages = c.prepare_cached("SELECT kind,after_key FROM local_agent_card_source_cursor WHERE namespace=?1 AND kind IN ('normalize','physical','select','operation','owned-reference','card','launch') ORDER BY kind")?
        .query_map([ns.as_str()], |r| Ok((r.get(0)?,r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Seeks {
        authority: (agent, id),
        lifecycle,
        stages,
    })
}

pub fn advanced(before: &Seeks, after: &Seeks) -> bool {
    after.authority > before.authority
        || after.lifecycle > before.lifecycle
        || after.stages.iter().any(|(kind, key)| {
            key.as_str() > before.stages.get(kind).map(String::as_str).unwrap_or("")
        })
}

/// Share the caller's aggregate deletion budget with Kernel reclamation. These private
/// tables have namespace indexes; neither source roots nor another namespace are changed.
pub fn reclaim(tx: &Transaction<'_>, ns: &Namespace, limit: usize) -> Result<usize> {
    let mut used = 0;
    for table in [
        "st3_agent_row_changes",
        "st3_agent_row_change_state",
        "st3_agent_work_progress",
    ] {
        if used == limit {
            break;
        }
        used += tx.execute(&format!("DELETE FROM {table} WHERE namespace=?1 AND rowid IN (SELECT rowid FROM {table} WHERE namespace=?1 LIMIT ?2)"),params![ns.as_str(),limit-used])?;
    }
    Ok(used)
}
