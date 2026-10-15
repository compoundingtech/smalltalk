//! Fresh document metadata pages. SQL shapes depend on filters, never filter values.
use super::*;

#[cfg(test)]
mod tests;

const LATEST: &str = "SELECT n.hash FROM documents n INDEXED BY document_canonical_latest WHERE n.name=d.name ORDER BY n.binding_key DESC LIMIT 1";

// Core documents.name and document_canonical_latest use BINARY. Compare Unicode scalar
// successors in that same UTF-8 byte order; skip the surrogate gap and carry past a maximal
// trailing scalar. An empty/all-maximal prefix has no finite upper bound.
fn prefix_end(prefix: &str) -> Option<String> {
    for (offset, value) in prefix.char_indices().rev() {
        let next = match u32::from(value) {
            0x10ffff => continue,
            0xd7ff => 0xe000,
            scalar => scalar + 1,
        };
        let mut end = prefix[..offset].to_owned();
        end.push(char::from_u32(next).expect("successor is a Unicode scalar"));
        return Some(end);
    }
    None
}

fn query(
    named: bool,
    prefixed: bool,
    lower: bool,
    upper: bool,
    history: bool,
    after: bool,
) -> String {
    // Required order index: document_index::initialize installs it before every normal
    // Store reader pool (including upgrades). A missing index is a prepare error. Pin it
    // because named/current queries otherwise choose the (name,hash) PK and add a sort.
    // The normal blobs.hash PK is also required: ANALYZE of a small blob table can
    // otherwise choose a table scan for every selected document.
    // normal_and_upgraded_document_read_indexes_match_the_required_layout guards both
    // names and key layouts on fresh and in-place-upgraded stores before a schema edit lands.
    // Current-only rows already satisfy the latest predicate. Avoid computing it a second
    // time for the returned boolean. History still reports each row's actual latest flag.
    let latest = if history {
        format!("d.hash=({LATEST})")
    } else {
        "1".to_owned()
    };
    // Keep the document index as the only outer loop. With metadata joins, low ANALYZE
    // estimates can lose that loop's order proof and add sorts to both the page and latest
    // lookup. Indexed scalar metadata lookups run only for returned rows. The existence
    // predicates preserve the old inner joins' row eligibility before the page limit.
    let mut sql = format!(
        "SELECT d.name,d.hash,
        (SELECT b.size FROM blobs b INDEXED BY sqlite_autoindex_blobs_1 WHERE b.hash=d.hash) AS size,
        d.created_index,{latest},d.binding_claim_id,
        (SELECT c.accepted_at_unix_ms FROM claims c WHERE c.id=d.binding_claim_id) AS accepted_at_unix_ms,
        (SELECT c.actor FROM claims c WHERE c.id=d.binding_claim_id) AS actor,
        (SELECT c.origin FROM claims c WHERE c.id=d.binding_claim_id) AS origin
        FROM documents d INDEXED BY document_canonical_latest
        WHERE EXISTS (SELECT 1 FROM blobs b INDEXED BY sqlite_autoindex_blobs_1 WHERE b.hash=d.hash)
        AND EXISTS (SELECT 1 FROM claims c WHERE c.id=d.binding_claim_id)"
    );
    if named {
        // Equal inclusive BINARY bounds select exactly this name. Retaining a range
        // loop lets the planner prove the full index order even when ANALYZE estimates
        // one version per name and a correlated cursor predicate reduces that estimate.
        // Do not simplify back to equality: actual_store_document_plans_seek_before_and_after_population_and_analyze
        // guards this SQLite planner dependency on the real Store and bundled version.
        sql.push_str(" AND d.name>=?1 AND d.name<=?1");
    }
    if lower {
        // A named page already seeks its exact name. Keep redundant range bounds on that
        // constant too: ranges on the column can spoil the binding-key order proof,
        // including the correlated latest lookup, under analyzed statistics.
        sql.push_str(if named {
            " AND ?1 COLLATE BINARY>=?5"
        } else {
            " AND d.name COLLATE BINARY>=?5"
        });
    }
    if upper {
        sql.push_str(if named {
            " AND ?1 COLLATE BINARY<?6"
        } else {
            " AND d.name COLLATE BINARY<?6"
        });
    }
    if prefixed {
        // Retain the exact old predicate, including length()/substr() treatment of NUL.
        // The range only bounds work; the limit is applied after this residual filter.
        sql.push_str(" AND substr(d.name,1,length(?2))=?2");
    }
    if !history {
        sql.push_str(&format!(" AND d.hash=({LATEST})"));
    }
    if after {
        // A named page's name equals its bound TEXT parameter on the BINARY column.
        // Compare that constant to the cursor directly, avoiding an OR over the indexed
        // name. Unnamed pages still compare each actual document name to the cursor.
        sql.push_str(if named {
            " AND (?1 COLLATE BINARY>?3 OR (?1 COLLATE BINARY=?3 AND EXISTS (
            SELECT 1 FROM documents cursor_document
            WHERE cursor_document.name=?3 AND cursor_document.created_index=?4
              AND cursor_document.binding_key>d.binding_key)))"
        } else {
            " AND (d.name>?3 OR (d.name=?3 AND EXISTS (
            SELECT 1 FROM documents cursor_document
            WHERE cursor_document.name=?3 AND cursor_document.created_index=?4
              AND cursor_document.binding_key>d.binding_key)))"
        });
    }
    // Fixed slot 7 keeps the binding arity fixed even when earlier slots are unused. Values
    // are always bound: the statement cache has a finite set of shapes, not one per prefix.
    sql.push_str(" ORDER BY d.name,d.binding_key DESC LIMIT ?7");
    sql
}

fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocumentVersion> {
    Ok(DocumentVersion {
        name: row.get(0)?,
        hash: row.get(1)?,
        size: row.get(2)?,
        created_index: row.get(3)?,
        latest: row.get::<_, i64>(4)? != 0,
        binding_claim_id: row.get(5)?,
        created_at_unix_ms: row.get::<_, String>(6)?.parse().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        owner: row.get::<_, Option<String>>(7)?.or(row.get(8)?),
    })
}

pub(super) fn read(
    store: &Store,
    name: Option<&str>,
    prefix: Option<&str>,
    history: bool,
    after: Option<(&str, u64)>,
    limit: usize,
) -> Result<Vec<DocumentVersion>> {
    let cursor_name = after.map(|value| value.0);
    let cursor_index = after.map(|value| value.1);
    let prefix_lower = prefix.filter(|value| !value.is_empty());
    // Intersect the prefix and cursor bounds, rather than scanning between them and only
    // then rejecting rows. Core name/cursor ordering is BINARY, as is this comparison.
    let lower = match (prefix_lower, cursor_name) {
        (Some(prefix), Some(cursor)) => Some(prefix.max(cursor)),
        (prefix, cursor) => prefix.or(cursor),
    };
    let upper = prefix.and_then(prefix_end);
    let sql = query(
        name.is_some(),
        prefix.is_some(),
        lower.is_some(),
        upper.is_some(),
        history,
        after.is_some(),
    );
    let connection = store.readers.get();
    let mut statement = connection.prepare_cached(/* sqlite-large-value: allow-for-now lv-h2f4c6110e554 owner=smallclaims::store::document_pages TODO=trace-statement-and-value-representation */ &sql)?;
    statement
        .query_map(
            params![name, prefix, cursor_name, cursor_index, lower, upper, limit],
            decode,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}
