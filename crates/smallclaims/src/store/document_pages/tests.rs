use super::*;
use crate::sqlite::work::{SqliteWork, SqliteWorkScope};

// Exact pre-specialization statement, with an independent copy of its row decoder.
const OLD: &str = "SELECT d.name, d.hash, b.size, d.created_index,
                     d.hash=(SELECT n.hash FROM documents n
                 WHERE n.name=d.name ORDER BY n.binding_key DESC LIMIT 1)
                     ,d.binding_claim_id, c.accepted_at_unix_ms, c.actor, c.origin
                     FROM documents d JOIN blobs b ON b.hash=d.hash
                     JOIN claims c ON c.id=d.binding_claim_id
                     WHERE (?1 IS NULL OR d.name=?1)
                       AND (?2 OR d.hash=(SELECT n.hash FROM documents n
                 WHERE n.name=d.name ORDER BY n.binding_key DESC LIMIT 1))
                       AND (?3 IS NULL OR substr(d.name,1,length(?3))=?3)
                       AND (?4 IS NULL OR d.name>?4 OR (d.name=?4 AND EXISTS (
                           SELECT 1 FROM documents cursor_document
                           WHERE cursor_document.name=?4 AND cursor_document.created_index=?5
                             AND cursor_document.binding_key>d.binding_key)))
                     ORDER BY d.name, d.binding_key DESC LIMIT ?6";

fn node() -> Store {
    Store::open_memory("documents-page", Arc::new(runtime::Plain)).unwrap()
}

fn old(
    store: &Store,
    name: Option<&str>,
    prefix: Option<&str>,
    history: bool,
    after: Option<(&str, u64)>,
    limit: usize,
) -> Result<Vec<DocumentVersion>> {
    store
        .readers
        .get()
        .prepare(/* sqlite-large-value: allow-for-now lv-hd4cf17ad362b owner=smallclaims::store::document_pages::tests TODO=trace-statement-and-value-representation */ OLD)?
        .query_map(
            params![
                name,
                history,
                prefix,
                after.map(|v| v.0),
                after.map(|v| v.1),
                limit
            ],
            |row| {
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
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn parity(
    store: &Store,
    name: Option<&str>,
    prefix: Option<&str>,
    history: bool,
    after: Option<(&str, u64)>,
    limit: usize,
) {
    let before = old(store, name, prefix, history, after, limit);
    let after_read = read(store, name, prefix, history, after, limit);
    let context =
        format!("name={name:?} prefix={prefix:?} history={history} cursor={after:?} limit={limit}");
    match (before, after_read) {
        (Ok(before), Ok(after)) => assert_eq!(
            serde_json::to_value(before).unwrap(),
            serde_json::to_value(after).unwrap(),
            "{context}"
        ),
        (Err(before), Err(after)) => {
            assert_eq!(before.to_string(), after.to_string(), "{context}");
        }
        (before, after) => panic!("{context}: old={before:?}, new={after:?}"),
    }
}

// Normal physical Store schema, graph claim append, canonical projection and fixed writer
// clocks. Distinct origins allow an older canonical binding to arrive after a newer one.
fn bind(
    store: &Store,
    name: &str,
    content: &str,
    time: u128,
    origin: &str,
    actor: Option<&str>,
) -> ClaimRecord {
    store.set_write_clock_at(time).unwrap();
    let hash = hex::encode(Sha256::digest(content.as_bytes()));
    store
        .connection
        .batched(|tx| {
            tx.execute(
                /* sqlite-large-value: allow-for-now lv-h1122a13a5a90 owner=smallclaims::store::document_pages::tests TODO=bound-or-reference-values-and-reclamation */ "INSERT OR IGNORE INTO blobs(hash,bytes,size) VALUES(?1,?2,?3)",
                params![hash, content.as_bytes(), content.len()],
            )?;
            let claim = store.runtime.append_claim_tx(
                tx,
                origin,
                name,
                "doc.bound",
                actor,
                &json!({"name":name,"hash":hash,"size":content.len()}),
                &[],
                None,
            )?;
            select_replicated_document(tx, &claim, claim.store_index)?;
            store.runtime.after_projection(tx)?;
            Ok::<_, anyhow::Error>(claim)
        })
        .unwrap()
        .unwrap()
}

#[test]
fn scalar_prefix_bounds_keep_binary_order_and_skip_the_surrogate_gap() {
    for (prefix, end) in [
        ("", None),
        ("\u{10ffff}", None),
        ("\u{10ffff}\u{10ffff}", None),
        ("a\u{10ffff}", Some("b")),
        ("\u{d7ff}", Some("\u{e000}")),
        ("😀", Some("😁")),
        ("doc/é", Some("doc/ê")),
        ("doc/\0", Some("doc/\u{1}")),
    ] {
        assert_eq!(prefix_end(prefix).as_deref(), end);
    }
}

#[test]
fn specialized_pages_match_old_full_metadata_filters_cursors_and_randomized_names() {
    let store = node();
    let mut names = vec![
        "doc/A",
        "doc/a",
        "doc/%_\\",
        "doc/é",
        "doc/中",
        "doc/😀",
        "doc/\u{d7ff}",
        "doc/\u{e000}",
        "doc/\u{10ffff}",
        "doc/x\u{10ffff}",
        "doc/nul\0tail",
        "doc/nul",
        "doc/aa",
        "doc/aaa",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let alphabet = [
        'A',
        'a',
        'Z',
        'z',
        '%',
        '_',
        '\\',
        'é',
        '中',
        '😀',
        '\0',
        '\u{10ffff}',
    ];
    let mut seed = 0x719_aa6_u64;
    for n in 0..48 {
        let mut name = format!("doc/r{n:02}-");
        for _ in 0..4 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            name.push(alphabet[(seed >> 32) as usize % alphabet.len()]);
        }
        names.push(name);
    }
    let mut cursors = vec![
        (String::new(), 0),
        ("doc/missing".into(), 9),
        ("doc/zzzz".into(), 9),
        ("doc/A".into(), u64::MAX),
    ];
    for (n, name) in names.iter().enumerate() {
        let first = bind(
            &store,
            name,
            &format!("first-{n}"),
            2000 + n as u128,
            &format!("late-{n}"),
            (n % 2 == 0).then_some("agent/author"),
        );
        let second = bind(
            &store,
            name,
            &format!("second-{n}"),
            4000 + n as u128,
            &format!("later-{n}"),
            None,
        );
        let earlier = bind(
            &store,
            name,
            &format!("first-{n}"),
            1000 + n as u128,
            &format!("early-{n}"),
            Some("agent/earlier"),
        );
        assert_eq!(earlier.accepted_at_unix_ms, 1000 + n as u128);
        let versions = read(&store, Some(name), None, true, None, 20).unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].binding_claim_id, second.id);
        assert!(versions[0].latest);
        assert_eq!(versions[1].binding_claim_id, earlier.id);
        assert!(!versions[1].latest);
        assert_eq!(versions[1].owner.as_deref(), Some("agent/earlier"));
        assert_eq!(versions[1].created_index, earlier.store_index);
        assert_ne!(first.store_index, earlier.store_index);
        cursors.push((name.clone(), second.store_index));
        cursors.push((name.clone(), earlier.store_index));
    }
    let fixed_prefixes = [
        None,
        Some(""),
        Some("doc/"),
        Some("doc/A"),
        Some("doc/a"),
        Some("doc/%_\\"),
        Some("doc/é"),
        Some("doc/中"),
        Some("doc/😀"),
        Some("doc/nul"),
        Some("doc/nul\0"),
        Some("doc/\u{d7ff}"),
        Some("doc/\u{10ffff}"),
        Some("\u{10ffff}"),
        Some("doc/x\u{10ffff}"),
    ];
    for prefix in fixed_prefixes {
        for history in [false, true] {
            for limit in [0, 1, 2, 200] {
                parity(&store, None, prefix, history, None, limit);
                for cursor in &cursors[..8] {
                    parity(
                        &store,
                        None,
                        prefix,
                        history,
                        Some((&cursor.0, cursor.1)),
                        limit,
                    );
                }
            }
        }
    }
    for _ in 0..512 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let name = &names[(seed >> 32) as usize % names.len()];
        let boundaries = name
            .char_indices()
            .map(|(i, _)| i)
            .chain([name.len()])
            .collect::<Vec<_>>();
        let prefix = &name[..boundaries[seed as usize % boundaries.len()]];
        let cursor = &cursors[(seed >> 16) as usize % cursors.len()];
        parity(
            &store,
            (seed & 1 == 0).then_some(name.as_str()),
            Some(prefix),
            seed & 2 == 0,
            (seed & 4 == 0).then_some((cursor.0.as_str(), cursor.1)),
            [0, 1, 3, 1000][seed as usize % 4],
        );
    }
    // Deterministic named-page cursor inside a three-version run, in addition to the
    // randomized named/cursor combinations above. Both history policies share the clock.
    let name = "doc/named-cursor";
    let oldest = bind(&store, name, "cursor-first", 10_000, "cursor-first", None);
    let middle = bind(&store, name, "cursor-middle", 20_000, "cursor-middle", None);
    let newest = bind(&store, name, "cursor-newest", 30_000, "cursor-newest", None);
    let below = bind(
        &store,
        "doc/a-cursor",
        "cursor-below",
        40_000,
        "cursor-below",
        None,
    );
    let above = bind(
        &store,
        "doc/z-cursor",
        "cursor-above",
        50_000,
        "cursor-above",
        None,
    );
    assert!(below.subject.as_str() < name && name < above.subject.as_str());
    for history in [false, true] {
        for limit in [0, 1, 2, 10] {
            for cursor in [&oldest, &middle, &newest] {
                parity(
                    &store,
                    Some(name),
                    None,
                    history,
                    Some((name, cursor.store_index)),
                    limit,
                );
            }
            // Each cursor name relation, paired with a real version's arrival index and
            // a nonexistent index. The named-page constant comparison must preserve both.
            for (cursor_name, index) in [
                (below.subject.as_str(), below.store_index),
                (name, middle.store_index),
                (above.subject.as_str(), above.store_index),
            ] {
                for cursor_index in [index, 0] {
                    parity(
                        &store,
                        Some(name),
                        None,
                        history,
                        Some((cursor_name, cursor_index)),
                        limit,
                    );
                }
            }
        }
        let before_page = read(
            &store,
            Some(name),
            None,
            history,
            Some((&below.subject, below.store_index)),
            10,
        )
        .unwrap();
        assert_eq!(before_page.len(), if history { 3 } else { 1 });
        assert!(
            read(
                &store,
                Some(name),
                None,
                history,
                Some((&above.subject, above.store_index)),
                10
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            read(&store, Some(name), None, history, Some((name, 0)), 10)
                .unwrap()
                .is_empty()
        );
    }
    let page = read(
        &store,
        Some(name),
        None,
        true,
        Some((name, middle.store_index)),
        1,
    )
    .unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].binding_claim_id, oldest.id);
    assert!(
        read(
            &store,
            Some(name),
            None,
            false,
            Some((name, middle.store_index)),
            10
        )
        .unwrap()
        .is_empty()
    );
    // Explicit named-prefix intersections: matching/nonmatching bounds and every
    // relation of max(prefix,cursor) to the name, with real and missing cursor rows.
    for prefix in [
        "",
        "doc/",
        "doc/named",
        name,
        "doc/named-cursor/",
        "doc/a",
        "doc/z",
        "\u{10ffff}",
    ] {
        for history in [false, true] {
            for limit in [0, 1, 2, 10] {
                parity(&store, Some(name), Some(prefix), history, None, limit);
                for (cursor_name, index) in [
                    (below.subject.as_str(), below.store_index),
                    (name, middle.store_index),
                    (above.subject.as_str(), above.store_index),
                ] {
                    for cursor_index in [index, 0] {
                        parity(
                            &store,
                            Some(name),
                            Some(prefix),
                            history,
                            Some((cursor_name, cursor_index)),
                            limit,
                        );
                    }
                }
            }
        }
    }
    // All-maximal prefixes have no upper bound but cannot match a valid doc/NAME.
    // Keep them as empty-page controls alongside a matching prefix with a maximal tail.
    let maximal_name = "doc/\u{10ffff}/tail";
    let maximal = bind(&store, maximal_name, "maximal", 60_000, "maximal", None);
    for prefix in [
        "",
        "\u{10ffff}",
        "\u{10ffff}\u{10ffff}",
        "doc/",
        "doc/\u{10ffff}",
    ] {
        for history in [false, true] {
            parity(&store, Some(maximal_name), Some(prefix), history, None, 10);
            for cursor in [
                (name, middle.store_index),
                (maximal_name, maximal.store_index),
                ("\u{10ffff}\u{10ffff}", 0),
            ] {
                parity(
                    &store,
                    Some(maximal_name),
                    Some(prefix),
                    history,
                    Some(cursor),
                    10,
                );
            }
        }
    }
    parity(&store, Some("doc/missing"), None, true, None, 5);
}

#[test]
fn corrupt_document_metadata_stays_an_error_without_a_partial_page() {
    let store = node();
    bind(&store, "doc/a", "good", 1000, "good", None);
    let bad = bind(&store, "doc/b", "bad", 1001, "bad", None);
    store
        .connection
        .write()
        .execute(
            /* sqlite-large-value: allow-for-now lv-h892f95ddc81a owner=smallclaims::store::document_pages::tests TODO=prepare-body-and-predecessor-values */ "UPDATE claims SET accepted_at_unix_ms='bad-time' WHERE id=?1",
            [&bad.id],
        )
        .unwrap();
    parity(&store, None, Some("doc/"), true, None, 10);
    assert!(read(&store, None, None, true, None, 10).is_err());
    // Cursor values retain rusqlite's checked u64 binding, not a wrapping cast.
    parity(&store, None, None, true, Some(("doc/a", u64::MAX)), 10);

    // rusqlite includes the SQLite column name in wrong-storage-class errors. Preserve
    // the former joined columns' names when scalar metadata expressions replace them.
    let typed = node();
    bind(&typed, "doc/typed", "typed", 1500, "typed", None);
    for (table, column, key, value) in [
        (
            "blobs",
            "size",
            "hash",
            rusqlite::types::Value::Text("bad-size".into()),
        ),
        (
            "claims",
            "accepted_at_unix_ms",
            "binding_claim_id",
            rusqlite::types::Value::Blob(vec![1, 2]),
        ),
        (
            "claims",
            "actor",
            "binding_claim_id",
            rusqlite::types::Value::Blob(vec![3, 4]),
        ),
        (
            "claims",
            "origin",
            "binding_claim_id",
            rusqlite::types::Value::Blob(vec![5, 6]),
        ),
    ] {
        let predicate = format!(
            "{}=(SELECT {key} FROM documents WHERE name='doc/typed')",
            if table == "blobs" { "hash" } else { "id" }
        );
        let saved: rusqlite::types::Value = typed
            .readers
            .get()
            .query_row(
                /* sqlite-large-value: allow-for-now lv-h42f1be3305c3 owner=smallclaims::store::document_pages::tests TODO=trace-statement-and-value-representation */ &format!("SELECT {column} FROM {table} WHERE {predicate}"),
                [],
                |row| row.get(0),
            )
            .unwrap();
        let sql = format!(/* sqlite-large-value: allow-for-now lv-h62d89f9f4d7f owner=smallclaims::store::document_pages::tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE {table} SET {column}=?1 WHERE {predicate}");
        assert_eq!(typed.connection.write().execute(/* sqlite-large-value: allow-for-now lv-hc0edd49ade2a owner=smallclaims::store::document_pages::tests TODO=trace-statement-and-value-representation */ &sql, [&value]).unwrap(), 1);
        for history in [false, true] {
            parity(&typed, Some("doc/typed"), None, history, None, 10);
            assert!(read(&typed, Some("doc/typed"), None, history, None, 10).is_err());
        }
        assert_eq!(typed.connection.write().execute(/* sqlite-large-value: allow-for-now lv-hd33aab22558e owner=smallclaims::store::document_pages::tests TODO=trace-statement-and-value-representation */ &sql, [&saved]).unwrap(), 1);
    }

    // The former inner joins excluded missing metadata before applying LIMIT. Keep
    // that eligibility even when indexed scalar lookups supply the returned fields.
    let missing = node();
    let missing_claim = bind(&missing, "doc/a", "claim-missing", 2000, "missing", None);
    let missing_blob = bind(&missing, "doc/b", "blob-missing", 2001, "missing", None);
    let good = bind(&missing, "doc/c", "present", 2002, "present", None);
    bind(&missing, "doc/d", "also-present", 2003, "present", None);
    {
        let conn = missing.connection.write();
        conn.execute(
            /* sqlite-large-value: allow-for-now lv-hadd33c4e0cb2 owner=smallclaims::store::document_pages::tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE documents SET binding_claim_id='absent' WHERE name='doc/a'",
            [],
        )
        .unwrap();
        // A deliberately damaged private fixture, beyond normal foreign-key admission.
        // Restore enforcement immediately; the old query is the eligibility oracle.
        conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        conn.execute(
            /* sqlite-large-value: allow-for-now lv-h24e997aab3fc owner=smallclaims::store::document_pages::tests TODO=bound-or-reference-values-and-reclamation */ "DELETE FROM blobs WHERE hash=(SELECT hash FROM documents WHERE name='doc/b')",
            [],
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    }
    for history in [false, true] {
        for limit in [0, 1, 10] {
            for cursor in [
                None,
                Some(("doc/a", missing_claim.store_index)),
                Some(("doc/b", missing_blob.store_index)),
            ] {
                parity(&missing, None, Some("doc/"), history, cursor, limit);
            }
        }
        let page = read(&missing, None, Some("doc/"), history, None, 1).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].binding_claim_id, good.id);
    }
    // Three rows with only the middle one ineligible: each metadata failure independently
    // leaves two page slots for the surrounding rows, including a cursor before the page.
    for missing_blob in [false, true] {
        let gap = node();
        let first = bind(&gap, "doc/a", "first", 3000, "gap", None);
        bind(&gap, "doc/b", "middle", 3001, "gap", None);
        let last = bind(&gap, "doc/c", "last", 3002, "gap", None);
        {
            let conn = gap.connection.write();
            if missing_blob {
                conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
                conn.execute(
                    /* sqlite-large-value: allow-for-now lv-hbccafe49f0c6 owner=smallclaims::store::document_pages::tests TODO=bound-or-reference-values-and-reclamation */ "DELETE FROM blobs WHERE hash=(SELECT hash FROM documents WHERE name='doc/b')",
                    [],
                )
                .unwrap();
                conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
            } else {
                conn.execute(
                    /* sqlite-large-value: allow-for-now lv-h72b16570b8c6 owner=smallclaims::store::document_pages::tests TODO=bound-or-reference-values-and-reclamation */ "UPDATE documents SET binding_claim_id='absent' WHERE name='doc/b'",
                    [],
                )
                .unwrap();
            }
        }
        for history in [false, true] {
            for cursor in [None, Some(("doc/", 0))] {
                parity(&gap, None, Some("doc/"), history, cursor, 2);
                let page = read(&gap, None, Some("doc/"), history, cursor, 2).unwrap();
                assert_eq!(page.len(), 2);
                assert_eq!(page[0].binding_claim_id, first.id);
                assert_eq!(page[1].binding_claim_id, last.id);
            }
        }
    }
}

fn plans(
    store: &Store,
    name: Option<&str>,
    prefix: Option<&str>,
    history: bool,
    after: Option<(&str, u64)>,
) -> (Vec<String>, Vec<String>) {
    let lower = match (prefix.filter(|s| !s.is_empty()), after.map(|v| v.0)) {
        (Some(p), Some(c)) => Some(p.max(c)),
        (p, c) => p.or(c),
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
    let conn = store.readers.get();
    let old = conn
        .prepare(/* sqlite-large-value: allow-for-now lv-h464a8f9dc23e owner=smallclaims::store::document_pages::tests TODO=trace-statement-and-value-representation */ &format!("EXPLAIN QUERY PLAN {OLD}"))
        .unwrap()
        .query_map(
            params![
                name,
                history,
                prefix,
                after.map(|v| v.0),
                after.map(|v| v.1),
                10
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let new = conn
        .prepare(/* sqlite-large-value: allow-for-now lv-hf84348139b26 owner=smallclaims::store::document_pages::tests TODO=trace-statement-and-value-representation */ &format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(
            params![
                name,
                prefix,
                after.map(|v| v.0),
                after.map(|v| v.1),
                lower,
                upper,
                10
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    println!(
        "document-page OLD={old:?} NEW={new:?} name={name:?} prefix={prefix:?} history={history} cursor={after:?}"
    );
    (
        old.into_iter().map(|(_, _, detail)| detail).collect(),
        new.into_iter().map(|(_, _, detail)| detail).collect(),
    )
}

fn assert_plans(store: &Store) {
    let sql: String = store
        .readers
        .get()
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name='documents'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !sql.to_ascii_uppercase().contains("COLLATE"),
        "core name uses default BINARY: {sql}"
    );
    for name in [None, Some("doc/target/a")] {
        for prefix in [
            None,
            Some(""),
            Some("doc/target/"),
            Some("doc/z/"),
            Some("\u{10ffff}"),
        ] {
            for history in [false, true] {
                for after in [
                    None,
                    Some(("doc/a", 0)),
                    Some(("doc/target/a", 0)),
                    Some(("doc/z", 0)),
                ] {
                    let (old, new) = plans(store, name, prefix, history, after);
                    let context = format!("OLD={old:?} NEW={new:?}");
                    let driven = new
                        .iter()
                        .find(|s| s.starts_with("SEARCH d ") || s.starts_with("SCAN d "))
                        .expect(&context);
                    assert!(driven.contains("document_canonical_latest"), "{context}");
                    if name.is_some() || prefix.is_some_and(|s| !s.is_empty()) || after.is_some() {
                        assert!(driven.starts_with("SEARCH d "), "{context}");
                    }
                    for table in ["b", "c"] {
                        assert!(
                            new.iter()
                                .any(|s| s.starts_with(&format!("SEARCH {table} "))),
                            "{context}"
                        );
                    }
                    assert!(
                        !new.iter().any(|s| s.starts_with("SCAN n ")
                            || s.starts_with("SCAN cursor_document ")
                            || s.contains("TEMP B-TREE")),
                        "{context}"
                    );
                }
            }
        }
    }
}

#[test]
fn actual_store_document_plans_seek_before_and_after_population_and_analyze() {
    println!("document-page bundled SQLite: {}", rusqlite::version());
    let store = node();
    bind(&store, "doc/target/a", "target", 1000, "target", None);
    println!("document-page plan stage: initial");
    assert_plans(&store);
    for n in 0..256 {
        bind(
            &store,
            &format!("doc/before/{n:04}"),
            &format!("distinct-content-{n}"),
            1001 + n,
            "population",
            None,
        );
    }
    println!("document-page plan stage: populated distinct blobs");
    assert_plans(&store);
    store.connection.write().execute_batch(/* sqlite-large-value: allow-for-now lv-hec196dcb5418 owner=smallclaims::store::document_pages::tests TODO=trace-statement-and-value-representation */ "ANALYZE").unwrap();
    println!("document-page plan stage: analyzed distinct blobs");
    assert_plans(&store);
}

#[test]
fn normal_and_upgraded_document_read_indexes_match_the_required_layout() {
    fn layout(store: &Store) {
        let conn = store.readers.get();
        for (table, index, expected) in [
            (
                "documents",
                "document_canonical_latest",
                vec![
                    ("name".to_owned(), 0_i64, "BINARY".to_owned()),
                    ("binding_key".to_owned(), 1, "BINARY".to_owned()),
                ],
            ),
            (
                "blobs",
                "sqlite_autoindex_blobs_1",
                vec![("hash".to_owned(), 0_i64, "BINARY".to_owned())],
            ),
            (
                "claims",
                "sqlite_autoindex_claims_1",
                vec![("id".to_owned(), 0_i64, "BINARY".to_owned())],
            ),
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_index_list(?1) WHERE name=?2)",
                    params![table, index],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "{table} lacks required {index}");
            if table != "documents" {
                let unique: bool = conn
                    .query_row(
                        "SELECT \"unique\" FROM pragma_index_list(?1) WHERE name=?2",
                        params![table, index],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert!(unique, "scalar metadata requires unique {table}/{index}");
            }
            let columns = conn
                .prepare(
                    "SELECT name,desc,coll FROM pragma_index_xinfo(?1) WHERE key=1 ORDER BY seqno",
                )
                .unwrap()
                .query_map([index], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(columns, expected, "{table}/{index}");
        }
    }
    layout(&node());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("documents.sqlite3");
    let store = Store::open(&path, "documents-upgrade", Arc::new(runtime::Plain)).unwrap();
    bind(&store, "doc/upgrade", "first", 1000, "first", None);
    bind(
        &store,
        "doc/upgrade",
        "second",
        2000,
        "second",
        Some("agent/author"),
    );
    layout(&store);
    let expected = serde_json::to_value(read(&store, None, None, true, None, 10).unwrap()).unwrap();
    // The supported schema-fourteen document layout, on the actual configured writer.
    // blobs retains its historical hash PK; document_index rebuilds the added binding key.
    store
        .connection
        .write()
        .execute_batch(
            /* sqlite-large-value: allow-for-now lv-h28d782e2dd8a owner=smallclaims::store::document_pages::tests TODO=bound-or-reference-values-and-reclamation */ "DROP TRIGGER projection_digest_documents_insert;
         DROP TRIGGER projection_digest_documents_update;
         DROP TRIGGER projection_digest_documents_delete;
         DROP INDEX document_canonical_latest;
         ALTER TABLE documents DROP COLUMN binding_key;
         DELETE FROM meta WHERE key='canonical_document_binding_keys';
         PRAGMA user_version=14;",
        )
        .unwrap();
    drop(store);
    for _ in 0..2 {
        let reopened = Store::open(&path, "documents-upgrade", Arc::new(runtime::Plain)).unwrap();
        layout(&reopened);
        assert_eq!(
            serde_json::to_value(read(&reopened, None, None, true, None, 10).unwrap()).unwrap(),
            expected
        );
        parity(&reopened, None, Some("doc/"), true, None, 10);
    }
}

fn measured(
    store: &Store,
    use_old: bool,
    name: Option<&str>,
    prefix: Option<&str>,
    after: Option<(&str, u64)>,
) -> SqliteWork {
    let scope = SqliteWorkScope::start();
    let result = if use_old {
        old(store, name, prefix, true, after, 5)
    } else {
        read(store, name, prefix, true, after, 5)
    };
    let work = scope.finish();
    assert!(!result.unwrap().is_empty());
    work
}

fn bytecode(store: &Store, name: Option<&str>, prefix: Option<&str>, after: Option<(&str, u64)>) {
    // Diagnostics outside every work scope. Preserve full opcode operands; print both
    // Results before asserting so an EXPLAIN error is retained with the work counts.
    type Opcode = (i64, String, i64, i64, i64, Option<String>, i64);
    fn explain(
        conn: &Connection,
        sql: &str,
        values: &[&dyn rusqlite::ToSql],
    ) -> rusqlite::Result<Vec<Opcode>> {
        conn.prepare(/* sqlite-large-value: allow-for-now lv-h128be0424e3e owner=smallclaims::store::document_pages::tests TODO=trace-statement-and-value-representation */ &format!("EXPLAIN {sql}"))?
            .query_map(values, |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })?
            .collect()
    }
    let cursor = after.map(|v| v.0);
    let index = after.map(|v| v.1);
    let lower = match (prefix.filter(|s| !s.is_empty()), cursor) {
        (Some(p), Some(c)) => Some(p.max(c)),
        (p, c) => p.or(c),
    };
    let upper = prefix.and_then(prefix_end);
    let sql = query(
        name.is_some(),
        prefix.is_some(),
        lower.is_some(),
        upper.is_some(),
        true,
        after.is_some(),
    );
    let conn = store.readers.get();
    let old = explain(
        &conn,
        OLD,
        &[&name, &true, &prefix, &cursor, &index, &5_i64],
    );
    let new = explain(
        &conn,
        &sql,
        &[&name, &prefix, &cursor, &index, &lower, &upper, &5_i64],
    );
    println!(
        "document-page bytecode name={name:?} prefix={prefix:?} cursor={after:?} OLD={old:?} NEW={new:?}"
    );
    plans(store, name, prefix, true, after);
}

#[test]
fn filtered_document_pages_keep_warmed_work_flat_with_unrelated_earlier_names() {
    let store = node();
    // Fix the blob lookup's terminal-key boundary before both measurements. This name
    // is below every request and its actual content hash f11a... is above all the target
    // hashes and c270... (the growth content). The selected document rows stay unchanged.
    bind(&store, "doc/anchor", "anchor-12", 999, "anchor", None);
    for n in 0..4 {
        bind(
            &store,
            &format!("doc/target/{n}"),
            &format!("target-{n}"),
            1000 + n,
            "targets",
            None,
        );
    }
    let shapes = [
        (Some("doc/target/1"), None, None),
        (None, Some("doc/target/"), None),
        (None, None, Some(("doc/target/0", 0))),
        (None, Some("doc/target/"), Some(("doc/target/0", 0))),
    ];
    let before = shapes.map(|(name, prefix, after)| {
        bytecode(&store, name, prefix, after);
        for use_old in [true, false] {
            measured(&store, use_old, name, prefix, after);
        }
        (
            measured(&store, true, name, prefix, after),
            measured(&store, false, name, prefix, after),
        )
    });
    for n in 0..512 {
        bind(
            &store,
            &format!("doc/before/{n:04}"),
            "unrelated",
            2000 + n,
            "population",
            None,
        );
    }
    let after_work = shapes.map(|(name, prefix, after)| {
        bytecode(&store, name, prefix, after);
        for use_old in [true, false] {
            measured(&store, use_old, name, prefix, after);
        }
        (
            measured(&store, true, name, prefix, after),
            measured(&store, false, name, prefix, after),
        )
    });
    // A second growth point is diagnostic, not a relaxed bound. The original exact-flat
    // assertions below still govern the first 512 additions, including a one-step delta.
    for n in 512..1024 {
        bind(
            &store,
            &format!("doc/before/{n:04}"),
            "unrelated",
            2000 + n,
            "population",
            None,
        );
    }
    let twice_work = shapes.map(|(name, prefix, after)| {
        bytecode(&store, name, prefix, after);
        for use_old in [true, false] {
            measured(&store, use_old, name, prefix, after);
        }
        (
            measured(&store, true, name, prefix, after),
            measured(&store, false, name, prefix, after),
        )
    });
    println!("document-page diagnostic after1024={twice_work:?}");
    println!("document-page before={before:?} after={after_work:?}");
    for ((old_before, new_before), (old_after, new_after)) in before.into_iter().zip(after_work) {
        assert!(new_before.statements > 0 && new_before.vm_steps > 0);
        assert_eq!(new_before.statements, new_after.statements);
        assert_eq!(new_before.vm_steps, new_after.vm_steps);
        assert_eq!(new_before.fullscan_steps, 0);
        assert_eq!(new_after.fullscan_steps, 0);
        assert_eq!(new_before.sorts, 0);
        assert_eq!(new_after.sorts, 0);
        assert!(
            old_after.vm_steps > old_before.vm_steps,
            "old={old_before:?}->{old_after:?}"
        );
    }
}

#[test]
fn isolated_blob_boundary_explains_the_retained_one_step_difference() {
    let store = node();
    for n in 0..4 {
        bind(
            &store,
            &format!("doc/target/{n}"),
            &format!("target-{n}"),
            1000 + n,
            "targets",
            None,
        );
    }
    // Verbatim generated f04 history/prefix statement, without later planner hints. This explains
    // the retained f04/de4 negative; it must not follow changes to the production query.
    // Its +1, exact flatness, blob-only mutation and full-metadata assertions stay unchanged.
    // Probe changes only blobs, never documents, so it isolates the lookup's boundary.
    let sql = "SELECT d.name,d.hash,b.size,d.created_index,d.hash=(SELECT n.hash FROM documents n WHERE n.name=d.name ORDER BY n.binding_key DESC LIMIT 1),
        d.binding_claim_id,c.accepted_at_unix_ms,c.actor,c.origin
        FROM documents d CROSS JOIN blobs b CROSS JOIN claims c
        WHERE b.hash=d.hash AND c.id=d.binding_claim_id AND d.name COLLATE BINARY>=?5 AND d.name COLLATE BINARY<?6 AND substr(d.name,1,length(?2))=?2 ORDER BY d.name,d.binding_key DESC LIMIT ?7";
    let execute = || {
        let conn = store.readers.get();
        let scope = SqliteWorkScope::start();
        let result = conn
            .prepare_cached(/* sqlite-large-value: allow-for-now lv-h4f7a011b7194 owner=smallclaims::store::document_pages::tests TODO=trace-statement-and-value-representation */ sql)
            .unwrap()
            .query_map(
                params![
                    None::<String>,
                    "doc/target/",
                    None::<String>,
                    None::<u64>,
                    "doc/target/",
                    "doc/target0",
                    5
                ],
                decode,
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        (serde_json::to_value(result).unwrap(), scope.finish())
    };
    execute();
    let (before_rows, before) = execute();
    let insert_blob = |content: &str| {
        let hash = hex::encode(Sha256::digest(content.as_bytes()));
        store
            .connection
            .write()
            .execute(
                /* sqlite-large-value: allow-for-now lv-hefb8b69fd6f2 owner=smallclaims::store::document_pages::tests TODO=bound-or-reference-values-and-reclamation */ "INSERT INTO blobs(hash,bytes,size) VALUES(?1,?2,?3)",
                params![hash, content.as_bytes(), content.len()],
            )
            .unwrap();
    };
    // 0896... is below all target hashes; c270... is above them. These are real hashes
    // of the bound bytes, not synthetic keys that bypass the Store's normal representation.
    let lowest_target = hex::encode(Sha256::digest(b"target-1"));
    let highest_target = hex::encode(Sha256::digest(b"target-2"));
    assert!(hex::encode(Sha256::digest(b"below-8")) < lowest_target);
    assert!(hex::encode(Sha256::digest(b"unrelated")) > highest_target);
    insert_blob("below-8");
    execute();
    let (below_rows, below) = execute();
    insert_blob("unrelated");
    execute();
    let (one_rows, one) = execute();
    for n in 0..512 {
        insert_blob(&format!("only-blob-{n}"));
    }
    execute();
    let (many_rows, many) = execute();
    println!(
        "document-page isolated blob boundary BEFORE={before:?} BELOW={below:?} ABOVE={one:?} MANY={many:?}"
    );
    assert_eq!(before_rows, below_rows);
    assert_eq!(before_rows, one_rows);
    assert_eq!(one_rows, many_rows);
    assert_eq!(before.statements, 1);
    assert_eq!(below.statements, 1);
    assert_eq!(one.statements, 1);
    assert_eq!(many.statements, 1);
    assert_eq!(one.vm_steps, before.vm_steps + 1);
    assert_eq!(below.vm_steps, before.vm_steps);
    assert_eq!(many.vm_steps, one.vm_steps);
    assert_eq!(
        (
            before.fullscan_steps,
            below.fullscan_steps,
            one.fullscan_steps,
            many.fullscan_steps
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(
        (before.sorts, below.sorts, one.sorts, many.sorts),
        (0, 0, 0, 0)
    );
}
