//! Host-local, rebuildable source membership, not an authorization projection.
//! Radix-16 range aggregates keep fenced counts and extrema independent of claim
//! history. Triggers run in the source transaction, including prune and repair.
use super::*;

const VERSION_KEY: &str = "native_source_ranges_v1";
const ROOT_LEVEL: u32 = 16;
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS native_source_ranges (
    source INTEGER NOT NULL,
    actor_scope INTEGER NOT NULL,
    actor TEXT NOT NULL,
    kind TEXT NOT NULL,
    level INTEGER NOT NULL,
    node INTEGER NOT NULL,
    subject TEXT NOT NULL,
    count INTEGER NOT NULL CHECK(count > 0),
    first_position INTEGER NOT NULL,
    last_position INTEGER NOT NULL,
    PRIMARY KEY(source,actor_scope,actor,kind,level,node,subject)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS native_source_ranges_subject
ON native_source_ranges(source,actor_scope,actor,kind,subject,level,node);
CREATE INDEX IF NOT EXISTS local_observations_graph_position
ON local_observations(after_store_index,id);
"#;

// Scope 0 covers every actor. Scope 1 covers exactly one recorded actor. Kind
// "" covers every kind; concrete-kind counts need only the all-actor scope.
fn scopes(record: &str) -> [(u8, String, String, String); 3] {
    [
        (0, "''".into(), "''".into(), "true".into()),
        (
            1,
            format!("{record}.actor"),
            "''".into(),
            format!("{record}.actor IS NOT NULL"),
        ),
        (0, "''".into(), format!("{record}.kind"), "true".into()),
    ]
}

fn insert_sql(source: u8, record: &str, position: &str, from: &str) -> String {
    let mut sql = String::new();
    let scopes = scopes(record);
    for level in 0..=ROOT_LEVEL {
        let shift = level * 4;
        for (actor_scope, actor, kind, predicate) in &scopes {
            sql.push_str(&format!(
                "INSERT INTO native_source_ranges
                 SELECT {source},{actor_scope},{actor},{kind},{level},
                        {position} >> {shift},{record}.subject,1,{position},{position}
                 {from} {predicate}
                 ON CONFLICT(source,actor_scope,actor,kind,level,node,subject)
                 DO UPDATE SET count=count+1,
                     first_position=MIN(first_position,excluded.first_position),
                     last_position=MAX(last_position,excluded.last_position);\n"
            ));
        }
    }
    sql
}

fn delete_sql(source: u8, record: &str, repaired_id: Option<&str>) -> String {
    let field = |column: &str| {
        repaired_id.map_or_else(
            || format!("{record}.{column}"),
            |id| format!("(SELECT {column} FROM claims WHERE id={id})"),
        )
    };
    let position = field(if source == 0 { "store_index" } else { "id" });
    let subject = field("subject");
    let selectors = [
        (0, "''".to_owned(), "''".to_owned()),
        (1, field("actor"), "''".to_owned()),
        (0, "''".to_owned(), field("kind")),
    ];
    let mut sql = String::new();
    for level in 0..=ROOT_LEVEL {
        let shift = level * 4;
        for (actor_scope, actor, kind) in &selectors {
            // Equality on the full primary key, including for repair markers.
            // Do not hide these bounds inside a correlated EXISTS: that would
            // scan the read model once for every source being pruned.
            let matching = format!(
                "source={source} AND actor_scope={actor_scope} AND actor={actor}
                 AND kind={kind} AND level={level} AND node=({position} >> {shift})
                 AND subject={subject}"
            );
            // Child extrema are already corrected because levels run bottom-up.
            sql.push_str(&format!(
                "DELETE FROM native_source_ranges WHERE {matching} AND count=1;\n"
            ));
            if level > 0 {
                let child_level = level - 1;
                sql.push_str(&format!(
                    "UPDATE native_source_ranges SET count=count-1,
                     first_position=(SELECT MIN(child.first_position)
                       FROM native_source_ranges child INDEXED BY native_source_ranges_subject
                       WHERE child.source=native_source_ranges.source AND child.actor_scope=native_source_ranges.actor_scope
                         AND child.actor=native_source_ranges.actor AND child.kind=native_source_ranges.kind
                         AND child.subject=native_source_ranges.subject AND child.level={child_level}
                         AND child.node BETWEEN native_source_ranges.node*16 AND native_source_ranges.node*16+15),
                     last_position=(SELECT MAX(child.last_position)
                       FROM native_source_ranges child INDEXED BY native_source_ranges_subject
                       WHERE child.source=native_source_ranges.source AND child.actor_scope=native_source_ranges.actor_scope
                         AND child.actor=native_source_ranges.actor AND child.kind=native_source_ranges.kind
                         AND child.subject=native_source_ranges.subject AND child.level={child_level}
                         AND child.node BETWEEN native_source_ranges.node*16 AND native_source_ranges.node*16+15)
                     WHERE {matching};\n"
                ));
            }
        }
    }
    sql
}

pub(super) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    for (source, table, position) in [
        (0, "claims", "store_index"),
        (1, "local_observations", "id"),
    ] {
        let admitted_new = if source == 0 {
            "WHEN NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=NEW.id)"
        } else {
            ""
        };
        let admitted_old = if source == 0 {
            "WHEN NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims WHERE id=OLD.id)"
        } else {
            ""
        };
        connection.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS native_source_{table}_insert AFTER INSERT ON {table}
             {admitted_new} BEGIN {} END;
             CREATE TRIGGER IF NOT EXISTS native_source_{table}_delete AFTER DELETE ON {table}
             {admitted_old} BEGIN {} END;",
            insert_sql(source, "NEW", &format!("NEW.{position}"), "WHERE"),
            delete_sql(source, "OLD", None),
        ))?;
    }
    Ok(())
}

pub(super) fn open(transaction: &Transaction<'_>) -> Result<()> {
    // The repair triggers name projection_digest_repaired_claims as their target
    // table, which the base schema owns; it exists by the time projections open.
    transaction.execute_batch(&format!(
        "CREATE TRIGGER IF NOT EXISTS native_source_repair_insert
         AFTER INSERT ON projection_digest_repaired_claims BEGIN {} END;
         CREATE TRIGGER IF NOT EXISTS native_source_repair_delete
         AFTER DELETE ON projection_digest_repaired_claims BEGIN {} END;",
        delete_sql(0, "NEW", Some("NEW.id")),
        insert_sql(
            0,
            "record",
            "record.store_index",
            "FROM claims record WHERE record.id=OLD.id AND"
        ),
    ))?;

    let filled: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key=?1)",
        [VERSION_KEY],
        |row| row.get(0),
    )?;
    if !filled {
        transaction.execute("DELETE FROM native_source_ranges", [])?;
        // One grouped backfill per level, rather than replaying per-claim trigger
        // work. Repaired durable sources are never admitted into this read model.
        for (source, table, position, admitted) in [
            (
                0,
                "claims",
                "store_index",
                "WHERE NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims repaired WHERE repaired.id=record.id)",
            ),
            (1, "local_observations", "id", ""),
        ] {
            for level in 0..=ROOT_LEVEL {
                let shift = level * 4;
                for (actor_scope, actor, kind, extra) in [
                    (0, "''", "''", ""),
                    (1, "record.actor", "''", "record.actor IS NOT NULL"),
                    (0, "''", "record.kind", ""),
                ] {
                    let predicate = if extra.is_empty() {
                        admitted.to_owned()
                    } else if admitted.is_empty() {
                        format!("WHERE {extra}")
                    } else {
                        format!("{admitted} AND {extra}")
                    };
                    transaction.execute_batch(&format!(
                        "INSERT INTO native_source_ranges
                         SELECT {source},{actor_scope},{actor},{kind},{level},
                                record.{position} >> {shift},record.subject,COUNT(*),
                                MIN(record.{position}),MAX(record.{position})
                         FROM {table} record {predicate}
                         GROUP BY {actor},{kind},record.{position} >> {shift},record.subject;"
                    ))?;
                }
            }
        }
        transaction.execute("INSERT INTO meta(key,value) VALUES(?1,'1')", [VERSION_KEY])?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Membership {
    pub count: u64,
    pub first: Option<u64>,
    pub last: Option<u64>,
}

impl Membership {
    fn include(&mut self, other: Self) {
        self.count += other.count;
        self.first = match (self.first, other.first) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.last = match (self.last, other.last) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }
}

// Local observations are appended under the writer lock at current_index(),
// which never decreases even after checkpoint pruning. Thus id order also
// orders after_store_index. Intersect the two original fences by an indexed
// graph-position seek, then all subsequent reads use one exact id prefix.
pub(super) fn local_cutoff(connection: &Connection, fence: &NativeSourceFence) -> Result<u64> {
    let graph_cutoff: u64 = connection.query_row(
        "SELECT COALESCE((SELECT id FROM local_observations
         WHERE after_store_index<=?1 ORDER BY after_store_index DESC,id DESC LIMIT 1),0)",
        [fence.graph_index],
        |row| row.get(0),
    )?;
    Ok(graph_cutoff.min(fence.local_position))
}

fn prefix_nodes(upper: u64, maximum: u64) -> impl Iterator<Item = (u32, u64, u64)> {
    // Decompose [0, upper] into complete radix subtrees without allocating.
    // At most sixteen ranges of fifteen nodes each, irrespective of history.
    (0..=ROOT_LEVEL).rev().filter_map(move |level| {
        if upper >= maximum {
            return (level == ROOT_LEVEL).then_some((ROOT_LEVEL, 0, 0));
        }
        if level == ROOT_LEVEL {
            return None;
        }
        let end = upper + 1;
        let shift = level * 4;
        let digit = (end >> shift) & 15;
        let first = (end >> shift) & !15;
        (digit > 0).then(|| (level, first, first + digit - 1))
    })
}

/// One selector/fence query against the retained-source aggregates: a concrete
/// subject or a literal family range, narrowed by kind and recorded actor.
pub(super) struct SourceSelection<'a> {
    pub subject: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub lower: &'a str,
    pub end: &'a str,
    pub recorded_actor: Option<&'a str>,
}

pub(super) fn membership(
    connection: &Connection,
    source: u8,
    upper: u64,
    selection: &SourceSelection<'_>,
) -> Result<Membership> {
    let maximum: u64 = connection.query_row(
        if source == 0 {
            "SELECT COALESCE(MAX(store_index),0) FROM claims"
        } else {
            "SELECT COALESCE(MAX(id),0) FROM local_observations"
        },
        [],
        |row| row.get(0),
    )?;
    let SourceSelection {
        subject,
        kind,
        lower,
        end,
        recorded_actor,
    } = selection;
    let (actor_scope, actor) = recorded_actor.map_or((0, ""), |actor| (1, actor));
    let mut membership = Membership::default();
    // Equality on subject uses the subject index; family ranges use the primary
    // key's node/subject order. Neither path enumerates source records.
    let query = if subject.is_some() {
        "SELECT COALESCE(SUM(count),0),MIN(first_position),MAX(last_position)
         FROM native_source_ranges INDEXED BY native_source_ranges_subject
         WHERE source=?1 AND actor_scope=?2 AND actor=?3
           AND kind=?4 AND level=?5 AND node BETWEEN ?6 AND ?9 AND subject=?7"
    } else {
        "SELECT COALESCE(SUM(count),0),MIN(first_position),MAX(last_position)
         FROM native_source_ranges NOT INDEXED
         WHERE source=?1 AND actor_scope=?2 AND actor=?3
           AND kind=?4 AND level=?5 AND node=?6 AND subject>=?7 AND subject<?8"
    };
    let mut statement = connection.prepare_cached(query)?;
    for (level, first, last) in prefix_nodes(upper, maximum) {
        if let Some(subject) = subject {
            membership.include(statement.query_row(
                params![
                    source,
                    actor_scope,
                    actor,
                    kind.unwrap_or(""),
                    level,
                    first,
                    subject,
                    end,
                    last
                ],
                |row| {
                    Ok(Membership {
                        count: row.get(0)?,
                        first: row.get(1)?,
                        last: row.get(2)?,
                    })
                },
            )?);
        } else {
            // Seek each node separately: a node BETWEEN range followed by a
            // subject range cannot use both bounds in a B-tree. NOT INDEXED on
            // this WITHOUT ROWID table selects its primary key, never the
            // subject index that would enumerate every historical node.
            for node in first..=last {
                membership.include(statement.query_row(
                    params![
                        source,
                        actor_scope,
                        actor,
                        kind.unwrap_or(""),
                        level,
                        node,
                        lower,
                        end
                    ],
                    |row| {
                        Ok(Membership {
                            count: row.get(0)?,
                            first: row.get(1)?,
                            last: row.get(2)?,
                        })
                    },
                )?);
            }
        }
    }
    Ok(membership)
}
