//! Host-local, rebuildable source membership, not an authorization projection.
//! Root aggregates and closed radix-16 blocks bound fenced reads. Normal appends
//! update only the three root selectors; blocks are sealed as positions advance.
use super::*;

const VERSION_KEY: &str = "native_source_ranges_v2";

// HMAC key for native cursor fingerprints. Retained counts and extrema are private
// membership; the secret keeps them unguessable offline by readers holding a cursor.
const CURSOR_SECRET_KEY: &str = "native_cursor_secret";
const ROOT_LEVEL: u32 = 16;
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS native_source_ranges_v2 (
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
CREATE INDEX IF NOT EXISTS native_source_ranges_v2_subject
ON native_source_ranges_v2(source,actor_scope,actor,kind,subject,level,node);
CREATE INDEX IF NOT EXISTS native_source_ranges_v2_position
ON native_source_ranges_v2(source,level,node);
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

fn source_table(source: u8) -> (&'static str, &'static str) {
    if source == 0 {
        ("claims", "store_index")
    } else {
        ("local_observations", "id")
    }
}

fn maximum(source: u8, predicate: &str) -> String {
    let (table, position) = source_table(source);
    format!("(SELECT COALESCE(MAX({position}),0) FROM {table} {predicate})")
}

fn admitted(source: u8, record: &str) -> String {
    if source == 0 {
        format!(
            "NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims repaired
             WHERE repaired.id={record}.id)"
        )
    } else {
        "true".into()
    }
}

fn insert_sql(
    source: u8,
    record: &str,
    position: &str,
    from: &str,
    levels: std::ops::RangeInclusive<u32>,
) -> String {
    let mut sql = String::new();
    let scopes = scopes(record);
    for level in levels {
        let shift = level * 4;
        // Only complete blocks are materialized below the root. Readmission
        // updates those ancestors, including when their subject was absent.
        let closed = if level == ROOT_LEVEL {
            "true".to_owned()
        } else {
            format!(
                "({position} >> {shift}) < ({} >> {shift})",
                maximum(source, "")
            )
        };
        for (actor_scope, actor, kind, predicate) in &scopes {
            sql.push_str(&format!(
                "INSERT INTO native_source_ranges_v2
                 SELECT {source},{actor_scope},{actor},{kind},{level},
                        {position} >> {shift},{record}.subject,1,{position},{position}
                 {from} {predicate} AND {closed}
                 ON CONFLICT(source,actor_scope,actor,kind,level,node,subject)
                 DO UPDATE SET count=count+1,
                     first_position=MIN(first_position,excluded.first_position),
                     last_position=MAX(last_position,excluded.last_position);\n"
            ));
        }
    }
    sql
}

fn seal_sql(source: u8, position: &str) -> String {
    let (table, column) = source_table(source);
    let previous = maximum(source, &format!("WHERE {column}<{position}"));
    let mut sql = String::new();
    for level in 1..ROOT_LEVEL {
        let shift = level * 4;
        let node = format!("({previous} >> {shift})");
        let crossed = format!("{node} < ({position} >> {shift})");
        if level == 1 {
            // At most sixteen source positions, including sparse/pruned blocks.
            // Replacing, rather than incrementing, also makes re-sealing a block
            // after pruning the newest positions harmless.
            for (actor_scope, actor, kind, predicate) in scopes("record") {
                sql.push_str(&format!(
                    "INSERT INTO native_source_ranges_v2
                     SELECT {source},{actor_scope},{actor},{kind},1,{node},
                            record.subject,COUNT(*),MIN(record.{column}),MAX(record.{column})
                     FROM {table} record NOT INDEXED
                     WHERE record.{column} BETWEEN ({node} << 4) AND ({node} << 4)+15
                       AND {predicate} AND {} AND {crossed}
                     GROUP BY {actor},{kind},record.subject
                     ON CONFLICT(source,actor_scope,actor,kind,level,node,subject)
                     DO UPDATE SET count=excluded.count,
                         first_position=excluded.first_position,
                         last_position=excluded.last_position;\n",
                    admitted(source, "record")
                ));
            }
        } else {
            let child_level = level - 1;
            sql.push_str(&format!(
                "INSERT INTO native_source_ranges_v2
                 SELECT source,actor_scope,actor,kind,{level},{node},subject,
                        SUM(count),MIN(first_position),MAX(last_position)
                 FROM native_source_ranges_v2 INDEXED BY native_source_ranges_v2_position
                 WHERE source={source} AND level={child_level}
                   AND node BETWEEN {node}*16 AND {node}*16+15 AND {crossed}
                 GROUP BY actor_scope,actor,kind,subject
                 ON CONFLICT(source,actor_scope,actor,kind,level,node,subject)
                 DO UPDATE SET count=excluded.count,
                     first_position=excluded.first_position,
                     last_position=excluded.last_position;\n"
            ));
        }
    }
    sql
}

// Exact retained extrema for one selector, without scanning its history. Closed
// blocks partition the prefix below the current open block; its tail has <=16
// positions. This also works after pruning lowers the greatest retained position.
fn root_extrema_sql(source: u8, actor_scope: u8, actor: &str, kind: &str, subject: &str) -> String {
    let (table, position) = source_table(source);
    let maximum = maximum(source, "");
    let mut parts = Vec::with_capacity(ROOT_LEVEL as usize);
    for level in 1..ROOT_LEVEL {
        let shift = level * 4;
        parts.push(format!(
            "SELECT first_position,last_position
             FROM native_source_ranges_v2 INDEXED BY native_source_ranges_v2_subject
             WHERE source={source} AND actor_scope={actor_scope} AND actor={actor}
               AND kind={kind} AND subject={subject} AND level={level}
               AND node BETWEEN (({maximum} >> {shift}) & ~15)
                            AND ({maximum} >> {shift})-1"
        ));
    }
    let actor_predicate = if actor_scope == 0 {
        "true".to_owned()
    } else {
        format!("record.actor={actor}")
    };
    parts.push(format!(
        "SELECT record.{position},record.{position} FROM {table} record NOT INDEXED
         WHERE record.{position} BETWEEN ({maximum} & ~15) AND {maximum}
           AND record.subject={subject} AND ({kind}='' OR record.kind={kind})
           AND {actor_predicate} AND {}",
        admitted(source, "record")
    ));
    parts.join(" UNION ALL ")
}

fn delete_sql(source: u8, record: &str, repaired_id: Option<&str>) -> String {
    let field = |column: &str| {
        repaired_id.map_or_else(
            || format!("{record}.{column}"),
            |id| format!("(SELECT {column} FROM claims WHERE id={id})"),
        )
    };
    let (table, column) = source_table(source);
    let position = field(column);
    let subject = field("subject");
    let selectors = [
        (0, "''".to_owned(), "''".to_owned()),
        (1, field("actor"), "''".to_owned()),
        (0, "''".to_owned(), field("kind")),
    ];
    let mut sql = String::new();
    for level in 1..=ROOT_LEVEL {
        let shift = level * 4;
        for (actor_scope, actor, kind) in &selectors {
            let matching = format!(
                "source={source} AND actor_scope={actor_scope} AND actor={actor}
                 AND kind={kind} AND level={level} AND node=({position} >> {shift})
                 AND subject={subject}"
            );
            sql.push_str(&format!(
                "DELETE FROM native_source_ranges_v2 WHERE {matching} AND count=1;\n"
            ));
            let retained = if level == ROOT_LEVEL {
                root_extrema_sql(source, *actor_scope, actor, kind, &subject)
            } else if level == 1 {
                format!(
                    "SELECT record.{column} AS first_position,record.{column} AS last_position
                     FROM {table} record NOT INDEXED
                     WHERE record.{column} BETWEEN (({position} >> 4) << 4)
                                               AND (({position} >> 4) << 4)+15
                       AND record.subject={subject} AND ({kind}='' OR record.kind={kind})
                       AND ({actor_scope}=0 OR record.actor={actor}) AND {}",
                    admitted(source, "record")
                )
            } else {
                let child_level = level - 1;
                format!(
                    "SELECT first_position,last_position
                     FROM native_source_ranges_v2 INDEXED BY native_source_ranges_v2_subject
                     WHERE source={source} AND actor_scope={actor_scope} AND actor={actor}
                       AND kind={kind} AND subject={subject} AND level={child_level}
                       AND node BETWEEN ({position} >> {shift})*16
                                    AND ({position} >> {shift})*16+15"
                )
            };
            // Children are corrected bottom-up. Extrema only need a seek when
            // the removed position was a boundary; interior prune just decrements.
            sql.push_str(&format!(
                "UPDATE native_source_ranges_v2 SET count=count-1,
                 first_position=CASE WHEN first_position={position}
                     THEN (SELECT MIN(first_position) FROM ({retained})) ELSE first_position END,
                 last_position=CASE WHEN last_position={position}
                     THEN (SELECT MAX(last_position) FROM ({retained})) ELSE last_position END
                 WHERE {matching};\n"
            ));
        }
    }
    sql
}

fn install_triggers(transaction: &Transaction<'_>) -> Result<()> {
    for (source, table, position) in [
        (0, "claims", "store_index"),
        (1, "local_observations", "id"),
    ] {
        let new_position = format!("NEW.{position}");
        let previous = maximum(source, &format!("WHERE {position}<{new_position}"));
        transaction.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS native_source_v2_{table}_seal AFTER INSERT ON {table}
             WHEN {new_position}={} AND ({previous} >> 4)<({new_position} >> 4)
             BEGIN {} END;
             CREATE TRIGGER IF NOT EXISTS native_source_v2_{table}_insert AFTER INSERT ON {table}
             WHEN {} BEGIN {} END;
             CREATE TRIGGER IF NOT EXISTS native_source_v2_{table}_historical AFTER INSERT ON {table}
             WHEN {} AND {new_position}<{} BEGIN {} END;
             CREATE TRIGGER IF NOT EXISTS native_source_v2_{table}_delete AFTER DELETE ON {table}
             WHEN {} BEGIN {} END;",
            maximum(source, ""),
            seal_sql(source, &new_position),
            admitted(source, "NEW"),
            insert_sql(source, "NEW", &new_position, "WHERE", ROOT_LEVEL..=ROOT_LEVEL),
            admitted(source, "NEW"),
            maximum(source, ""),
            insert_sql(source, "NEW", &new_position, "WHERE", 1..=ROOT_LEVEL - 1),
            admitted(source, "OLD"),
            delete_sql(source, "OLD", None),
        ))?;
    }
    transaction.execute_batch(&format!(
        "CREATE TRIGGER IF NOT EXISTS native_source_v2_repair_insert
         AFTER INSERT ON projection_digest_repaired_claims BEGIN {} END;
         CREATE TRIGGER IF NOT EXISTS native_source_v2_repair_delete
         AFTER DELETE ON projection_digest_repaired_claims BEGIN {} END;",
        delete_sql(0, "NEW", Some("NEW.id")),
        insert_sql(
            0,
            "record",
            "record.store_index",
            "FROM claims record WHERE record.id=OLD.id AND",
            1..=ROOT_LEVEL
        ),
    ))?;
    Ok(())
}

pub(super) fn open(transaction: &Transaction<'_>) -> Result<()> {
    let secret: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key=?1)",
        [CURSOR_SECRET_KEY],
        |row| row.get(0),
    )?;
    if !secret {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes)?;
        transaction.execute(
            "INSERT INTO meta(key,value) VALUES(?1,?2)",
            params![CURSOR_SECRET_KEY, hex::encode(bytes)],
        )?;
    }
    let filled: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key=?1)",
        [VERSION_KEY],
        |row| row.get(0),
    )?;
    if !filled {
        // The unmerged predecessor cache holds no authority. Its secret is
        // retained; table/trigger cutover and rebuild share the open transaction.
        transaction.execute_batch(
            "DROP TRIGGER IF EXISTS native_source_claims_insert;
             DROP TRIGGER IF EXISTS native_source_claims_delete;
             DROP TRIGGER IF EXISTS native_source_local_observations_insert;
             DROP TRIGGER IF EXISTS native_source_local_observations_delete;
             DROP TRIGGER IF EXISTS native_source_repair_insert;
             DROP TRIGGER IF EXISTS native_source_repair_delete;
             DROP TABLE IF EXISTS native_source_ranges;
             DELETE FROM meta WHERE key='native_source_ranges_v1';",
        )?;
        transaction.execute_batch(SCHEMA)?;
        transaction.execute("DELETE FROM native_source_ranges_v2", [])?;
        for (source, table, position) in [
            (0, "claims", "store_index"),
            (1, "local_observations", "id"),
        ] {
            let frontier: u64 = transaction.query_row(
                &format!("SELECT COALESCE(MAX({position}),0) FROM {table}"),
                [],
                |row| row.get(0),
            )?;
            // Open blocks have no cached rows; older complete blocks are grouped
            // from authority. Skip empty high levels rather than scanning for them.
            for level in (1..ROOT_LEVEL)
                .filter(|level| frontier >> (level * 4) != 0)
                .chain(std::iter::once(ROOT_LEVEL))
            {
                let shift = level * 4;
                let closed = if level == ROOT_LEVEL {
                    "true".to_owned()
                } else {
                    format!("(record.{position} >> {shift}) < ({frontier} >> {shift})")
                };
                for (actor_scope, actor, kind, predicate) in scopes("record") {
                    transaction.execute_batch(&format!(
                        "INSERT INTO native_source_ranges_v2
                         SELECT {source},{actor_scope},{actor},{kind},{level},
                                record.{position} >> {shift},record.subject,COUNT(*),
                                MIN(record.{position}),MAX(record.{position})
                         FROM {table} record WHERE {predicate} AND {} AND {closed}
                         GROUP BY {actor},{kind},record.{position} >> {shift},record.subject;",
                        admitted(source, "record")
                    ))?;
                }
            }
        }
        transaction.execute("INSERT INTO meta(key,value) VALUES(?1,'1')", [VERSION_KEY])?;
    }
    install_triggers(transaction)?;
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

/// The per-store cursor fingerprint key written by [`open`]. A store whose projections never
/// opened cannot mint cursors; that is an internal error, not a degraded fingerprint.
pub(super) fn cursor_secret(connection: &Connection) -> Result<Vec<u8>> {
    let secret: Option<String> = connection
        .query_row(
            "SELECT value FROM meta WHERE key=?1",
            [CURSOR_SECRET_KEY],
            |row| row.get(0),
        )
        .optional()?;
    let secret = secret.ok_or_else(|| anyhow::anyhow!("native cursor secret is missing"))?;
    Ok(hex::decode(secret)?)
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
    // key's node/subject order. The uncached leaf tail has at most fifteen rows.
    let query = if subject.is_some() {
        "SELECT COALESCE(SUM(count),0),MIN(first_position),MAX(last_position)
         FROM native_source_ranges_v2 INDEXED BY native_source_ranges_v2_subject
         WHERE source=?1 AND actor_scope=?2 AND actor=?3
           AND kind=?4 AND level=?5 AND node BETWEEN ?6 AND ?9 AND subject=?7"
    } else {
        "SELECT COALESCE(SUM(count),0),MIN(first_position),MAX(last_position)
         FROM native_source_ranges_v2 NOT INDEXED
         WHERE source=?1 AND actor_scope=?2 AND actor=?3
           AND kind=?4 AND level=?5 AND node=?6 AND subject>=?7 AND subject<?8"
    };
    let mut statement = connection.prepare_cached(query)?;
    for (level, first, last) in prefix_nodes(upper, maximum) {
        if level == 0 {
            // Force the primary-position seek: other selector indexes must not
            // turn this bounded tail into a scan of the selected subject's history.
            let query = if source == 0 {
                "SELECT COUNT(*),MIN(record.store_index),MAX(record.store_index)
                 FROM claims record NOT INDEXED
                 WHERE record.store_index BETWEEN ?1 AND ?2
                   AND (?3 IS NULL OR record.subject=?3)
                   AND (?3 IS NOT NULL OR (record.subject>=?4 AND record.subject<?5))
                   AND (?6 IS NULL OR record.kind=?6)
                   AND (?7 IS NULL OR record.actor=?7)
                   AND (?6 IS NULL OR ?7 IS NULL)
                   AND NOT EXISTS(SELECT 1 FROM projection_digest_repaired_claims repaired
                                  WHERE repaired.id=record.id)"
            } else {
                "SELECT COUNT(*),MIN(record.id),MAX(record.id)
                 FROM local_observations record NOT INDEXED
                 WHERE record.id BETWEEN ?1 AND ?2
                   AND (?3 IS NULL OR record.subject=?3)
                   AND (?3 IS NOT NULL OR (record.subject>=?4 AND record.subject<?5))
                   AND (?6 IS NULL OR record.kind=?6)
                   AND (?7 IS NULL OR record.actor=?7)
                   AND (?6 IS NULL OR ?7 IS NULL)"
            };
            membership.include(connection.prepare_cached(query)?.query_row(
                params![first, last, subject, lower, end, kind, recorded_actor],
                |row| {
                    Ok(Membership {
                        count: row.get(0)?,
                        first: row.get(1)?,
                        last: row.get(2)?,
                    })
                },
            )?);
            continue;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn source_connection() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
             CREATE TABLE claims(store_index INTEGER PRIMARY KEY AUTOINCREMENT,
                 id TEXT NOT NULL UNIQUE,subject TEXT NOT NULL,kind TEXT NOT NULL,actor TEXT);
             CREATE TABLE projection_digest_repaired_claims(id TEXT PRIMARY KEY);
             CREATE TABLE local_observations(id INTEGER PRIMARY KEY AUTOINCREMENT,
                 after_store_index INTEGER NOT NULL,subject TEXT NOT NULL,kind TEXT NOT NULL,actor TEXT);",
        ).unwrap();
        connection
    }

    fn initialize(connection: &mut Connection) {
        let transaction = connection.transaction().unwrap();
        open(&transaction).unwrap();
        transaction.commit().unwrap();
    }

    fn insert(connection: &Connection, position: u64) {
        let subject = if position % 2 == 0 {
            "resource/selected/a"
        } else {
            "resource/selected/z"
        };
        let kind = if position % 3 == 0 {
            "resource.observed"
        } else {
            "other.kind"
        };
        let actor = (position % 5 != 0).then_some(if position % 2 == 0 {
            "person/a"
        } else {
            "person/z"
        });
        connection
            .execute(
                "INSERT INTO claims(store_index,id,subject,kind,actor) VALUES(?1,?2,?3,?4,?5)",
                params![position, format!("claim-{position}"), subject, kind, actor],
            )
            .unwrap();
        connection.execute(
            "INSERT INTO local_observations(id,after_store_index,subject,kind,actor) VALUES(?1,?1,?2,?3,?4)",
            params![position, subject, kind, actor],
        ).unwrap();
    }

    fn assert_direct_membership(connection: &Connection) {
        for source in 0..=1 {
            let (table, position) = source_table(source);
            for upper in [
                0,
                1,
                14,
                15,
                16,
                17,
                254,
                255,
                256,
                269,
                499,
                (1_u64 << 40) - 1,
                1_u64 << 40,
                (1_u64 << 40) + 1,
                i64::MAX as u64 - 1,
                i64::MAX as u64,
            ] {
                for subject in [
                    None,
                    Some("resource/selected/a"),
                    Some("resource/selected/z"),
                    Some("resource/missing"),
                ] {
                    for (kind, recorded_actor) in [
                        (None, None),
                        (Some("resource.observed"), None),
                        (Some("missing.kind"), None),
                        (None, Some("person/a")),
                        (None, Some("person/z")),
                        (None, Some("person/missing")),
                    ] {
                        let expected = connection
                            .query_row(
                                &format!(
                                    "SELECT COUNT(*),MIN(record.{position}),MAX(record.{position})
                                 FROM {table} record WHERE record.{position}<=?1
                                   AND record.subject>='resource/' AND record.subject<'resource0'
                                   AND (?2 IS NULL OR record.subject=?2)
                                   AND (?3 IS NULL OR record.kind=?3)
                                   AND (?4 IS NULL OR record.actor=?4) AND {}",
                                    admitted(source, "record")
                                ),
                                params![upper, subject, kind, recorded_actor],
                                |row| {
                                    Ok(Membership {
                                        count: row.get(0)?,
                                        first: row.get(1)?,
                                        last: row.get(2)?,
                                    })
                                },
                            )
                            .unwrap();
                        let actual = membership(
                            connection,
                            source,
                            upper,
                            &SourceSelection {
                                subject,
                                kind,
                                lower: if subject.is_some() { "" } else { "resource/" },
                                end: if subject.is_some() { "" } else { "resource0" },
                                recorded_actor,
                            },
                        )
                        .unwrap();
                        assert_eq!(
                            actual, expected,
                            "source={source} fence={upper} subject={subject:?} kind={kind:?} actor={recorded_actor:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn closed_blocks_preserve_sparse_fences_prune_repair_and_readmission() {
        let mut connection = source_connection();
        initialize(&mut connection);
        for position in 1..=270 {
            insert(&connection, position);
        }
        for position in [1_u64 << 40, (1_u64 << 40) + 1, i64::MAX as u64] {
            insert(&connection, position);
        }
        assert_direct_membership(&connection);
        // Removing both a complete node boundary and the newest open-tail row
        // must keep every old fence and all three selector scopes exact.
        for position in [1, 15, 16, 255, 269, i64::MAX as u64] {
            connection
                .execute("DELETE FROM claims WHERE store_index=?1", [position])
                .unwrap();
            connection
                .execute("DELETE FROM local_observations WHERE id=?1", [position])
                .unwrap();
        }
        assert_direct_membership(&connection);
        for position in [17, 256, (1_u64 << 40) + 1] {
            connection
                .execute(
                    "INSERT INTO projection_digest_repaired_claims(id) VALUES(?1)",
                    [format!("claim-{position}")],
                )
                .unwrap();
        }
        assert_direct_membership(&connection);
        for position in [(1_u64 << 40) + 1, 256, 17] {
            connection
                .execute(
                    "DELETE FROM projection_digest_repaired_claims WHERE id=?1",
                    [format!("claim-{position}")],
                )
                .unwrap();
        }
        assert_direct_membership(&connection);
        connection
            .execute(
                "INSERT INTO projection_digest_repaired_claims(id) VALUES('claim-14')",
                [],
            )
            .unwrap();
        connection
            .execute("DELETE FROM claims WHERE store_index=14", [])
            .unwrap();
        assert_direct_membership(&connection);
        connection
            .execute("DELETE FROM local_observations", [])
            .unwrap();
        assert_direct_membership(&connection);
        let roots: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM native_source_ranges_v2 WHERE source=1 AND level=16",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            roots, 0,
            "clearing local authority leaves no enumerated ghost"
        );
    }

    #[test]
    fn normal_append_writes_only_root_selectors_between_block_boundaries() {
        let mut connection = source_connection();
        initialize(&mut connection);
        for position in 1..=14 {
            let before = connection.total_changes();
            insert(&connection, position);
            let scope_count = if position % 5 == 0 { 2 } else { 3 };
            assert_eq!(
                connection.total_changes() - before,
                2 * (1 + scope_count),
                "two source rows and only their root selectors are written"
            );
        }
        let leaf_rows: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM native_source_ranges_v2 WHERE level<16",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            leaf_rows, 0,
            "an open block has no eagerly updated ancestors"
        );
        insert(&connection, 15);
        insert(&connection, 16);
        let sealed: u64 = connection
            .query_row(
                "SELECT SUM(count) FROM native_source_ranges_v2
             WHERE source=0 AND actor_scope=0 AND actor='' AND kind='' AND level=1 AND node=0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sealed, 15, "positions start at one, not zero");
        assert_direct_membership(&connection);
    }

    #[test]
    fn unrelated_append_keeps_exact_subject_membership_when_root_becomes_an_old_fence() {
        let mut connection = source_connection();
        initialize(&mut connection);
        for position in 1..=17 {
            insert(&connection, position);
        }
        let selection = SourceSelection {
            subject: Some("resource/selected/z"),
            kind: None,
            lower: "",
            end: "",
            recorded_actor: None,
        };
        let before = [
            membership(&connection, 0, 17, &selection).unwrap(),
            membership(&connection, 1, 17, &selection).unwrap(),
        ];
        // Position 18 belongs to a different subject. The pinned membership
        // now reads closed block zero plus raw positions 16..17, not the root.
        insert(&connection, 18);
        assert_eq!(
            before,
            [
                membership(&connection, 0, 17, &selection).unwrap(),
                membership(&connection, 1, 17, &selection).unwrap(),
            ]
        );
    }

    #[test]
    fn main_and_predecessor_cache_upgrades_match_a_fresh_build_and_keep_the_secret() {
        for predecessor in [false, true] {
            let mut connection = source_connection();
            for position in 1..=270 {
                insert(&connection, position);
            }
            connection
                .execute(
                    "INSERT INTO projection_digest_repaired_claims(id) VALUES('claim-256')",
                    [],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO meta(key,value) VALUES('native_cursor_secret',?1)",
                    ["ab".repeat(32)],
                )
                .unwrap();
            if predecessor {
                // Exact predecessor layout, populated from authority at every
                // level; its live trigger names must all be removed on cutover.
                connection.execute_batch(
                    "CREATE TABLE native_source_ranges(
                         source INTEGER NOT NULL,actor_scope INTEGER NOT NULL,actor TEXT NOT NULL,
                         kind TEXT NOT NULL,level INTEGER NOT NULL,node INTEGER NOT NULL,subject TEXT NOT NULL,
                         count INTEGER NOT NULL CHECK(count>0),first_position INTEGER NOT NULL,last_position INTEGER NOT NULL,
                         PRIMARY KEY(source,actor_scope,actor,kind,level,node,subject)) WITHOUT ROWID;
                     CREATE INDEX native_source_ranges_subject ON native_source_ranges(source,actor_scope,actor,kind,subject,level,node);
                     INSERT INTO meta(key,value) VALUES('native_source_ranges_v1','1');",
                ).unwrap();
                for source in 0..=1 {
                    let (table, position) = source_table(source);
                    for level in 0..=ROOT_LEVEL {
                        for (scope, actor, kind, predicate) in scopes("record") {
                            connection.execute_batch(&format!(
                                "INSERT INTO native_source_ranges SELECT {source},{scope},{actor},{kind},{level},
                                 record.{position} >> {},record.subject,COUNT(*),MIN(record.{position}),MAX(record.{position})
                                 FROM {table} record WHERE {predicate} AND {}
                                 GROUP BY {actor},{kind},record.{position} >> {},record.subject;",
                                level*4, admitted(source, "record"), level*4,
                            )).unwrap();
                        }
                    }
                }
                for (name, table, event) in [
                    ("native_source_claims_insert", "claims", "INSERT"),
                    ("native_source_claims_delete", "claims", "DELETE"),
                    (
                        "native_source_local_observations_insert",
                        "local_observations",
                        "INSERT",
                    ),
                    (
                        "native_source_local_observations_delete",
                        "local_observations",
                        "DELETE",
                    ),
                    (
                        "native_source_repair_insert",
                        "projection_digest_repaired_claims",
                        "INSERT",
                    ),
                    (
                        "native_source_repair_delete",
                        "projection_digest_repaired_claims",
                        "DELETE",
                    ),
                ] {
                    connection.execute_batch(&format!(
                        "CREATE TRIGGER {name} AFTER {event} ON {table} BEGIN SELECT count FROM native_source_ranges; END;",
                    )).unwrap();
                }
            }
            initialize(&mut connection);
            assert_eq!(cursor_secret(&connection).unwrap(), vec![0xab; 32]);
            assert_direct_membership(&connection);
            let obsolete: u64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='native_source_ranges'
                 OR name IN ('native_source_claims_insert','native_source_claims_delete',
                 'native_source_local_observations_insert','native_source_local_observations_delete',
                 'native_source_repair_insert','native_source_repair_delete')",
                [], |row| row.get(0),
            ).unwrap();
            assert_eq!(obsolete, 0);
            let mut fresh = source_connection();
            initialize(&mut fresh);
            for position in 1..=270 {
                insert(&fresh, position);
            }
            fresh
                .execute(
                    "INSERT INTO projection_digest_repaired_claims(id) VALUES('claim-256')",
                    [],
                )
                .unwrap();
            let rows = |connection: &Connection| {
                connection.prepare(
                    "SELECT source,actor_scope,actor,kind,level,node,subject,count,first_position,last_position
                     FROM native_source_ranges_v2 ORDER BY source,actor_scope,actor,kind,level,node,subject"
                ).unwrap().query_map([], |row| Ok((
                    row.get::<_,u8>(0)?,row.get::<_,u8>(1)?,row.get::<_,String>(2)?,
                    row.get::<_,String>(3)?,row.get::<_,u32>(4)?,row.get::<_,u64>(5)?,
                    row.get::<_,String>(6)?,row.get::<_,u64>(7)?,row.get::<_,u64>(8)?,row.get::<_,u64>(9)?,
                ))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
            };
            assert_eq!(rows(&connection), rows(&fresh), "predecessor={predecessor}");
            // Rebuild is deterministic and does not churn the fingerprint key.
            connection
                .execute("DELETE FROM meta WHERE key=?1", [VERSION_KEY])
                .unwrap();
            initialize(&mut connection);
            assert_eq!(rows(&connection), rows(&fresh));
            assert_eq!(cursor_secret(&connection).unwrap(), vec![0xab; 32]);
        }
    }
}
