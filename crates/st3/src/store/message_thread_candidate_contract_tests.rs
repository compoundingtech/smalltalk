//! Source-only contract for a future thread candidate index. This module does not install a
//! table or attach a reader. The SQL must select one effective parent per raw source, while the
//! canonical message fold remains the final authority at a pinned source cut.

use super::{canonical_child_string, fold_latest_values};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

const MAX_SOURCE_BYTES: i64 = 16 * 1024;

// `fields` suppresses the body root whenever the key exists, including null and nonobjects.
// Returning no parent for a wide or unsupported body is only safe when the future installer
// marks that source cut incomplete in the same transaction. These statements alone do not do so.
const CLAIM_PARENT: &str = r#"
SELECT length(CAST(?1 AS BLOB))<=?2 AND json_valid(?1),
       CASE WHEN length(CAST(?1 AS BLOB))<=?2 AND json_valid(?1) THEN
         CASE WHEN json_type(?1,'$.fields') IS NOT NULL THEN
           CASE WHEN json_type(?1,'$.fields.in_reply_to')='text'
                THEN json_extract(?1,'$.fields.in_reply_to') END
         ELSE
           CASE WHEN json_type(?1,'$.in_reply_to')='text'
                THEN json_extract(?1,'$.in_reply_to') END
         END
       END
"#;

// Match canonical_child_string: the FIRST named child wins even when its first argument is
// absent or not a string. A later valid child must not replace that first child's null result.
const DESIRED_PARENT: &str = r#"
SELECT length(CAST(?1 AS BLOB))<=?2 AND json_valid(?1),
       CASE WHEN length(CAST(?1 AS BLOB))<=?2 AND json_valid(?1)
                  AND json_type(?1,'$.children')='array' THEN
         (SELECT CASE WHEN json_type(child.value,'$.arguments[0]')='text'
                      THEN json_extract(child.value,'$.arguments[0]') END
          FROM json_each(?1,'$.children') AS child
          WHERE CASE WHEN child.type='object'
                     THEN json_extract(child.value,'$.name')='in-reply-to'
                     ELSE 0 END
          ORDER BY CAST(child.key AS INTEGER) LIMIT 1)
       END
"#;

fn source_parent(connection: &Connection, sql: &str, value: &Value) -> (bool, Option<String>) {
    let body = serde_json::to_string(value).unwrap();
    connection
        .query_row(sql, params![body, MAX_SOURCE_BYTES], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap()
}

fn legacy_parent(value: &str) -> String {
    if value.starts_with("message/") {
        value.to_owned()
    } else {
        format!("message/{value}")
    }
}

#[test]
fn claim_source_parent_matches_fold_with_fields_precedence() {
    let connection = Connection::open_in_memory().unwrap();
    for value in [
        json!({"in_reply_to":"message/root"}),
        json!({"fields":{"in_reply_to":"message/fields"},"in_reply_to":"message/root"}),
        json!({"fields":{"in_reply_to":null},"in_reply_to":"message/root"}),
        json!({"fields":null,"in_reply_to":"message/root"}),
        json!({"fields":"not-an-object","in_reply_to":"message/root"}),
        json!({"fields":{"in_reply_to":9},"in_reply_to":"message/root"}),
        json!({"fields":{"in_reply_to":""}}),
        json!({"fields":{"in_reply_to":"Message/Case"}}),
        json!({"fields":{"in_reply_to":" message/space "}}),
        json!({"fields":{}}),
        json!(null),
    ] {
        let (covered, parent) = source_parent(&connection, CLAIM_PARENT, &value);
        assert!(covered, "small valid claim must be covered: {value}");
        let actual = fold_latest_values([("message.sent".into(), value.clone())]).unwrap();
        let expected = actual
            .as_ref()
            .and_then(|actual| actual.get("in_reply_to"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        assert_eq!(parent, expected, "{value}");
    }
    assert_eq!(legacy_parent(""), "message/");
    assert_eq!(legacy_parent("Message/Case"), "message/Message/Case");
    assert_eq!(legacy_parent(" message/space "), "message/ message/space ");
}

#[test]
fn desired_source_parent_keeps_first_matching_child_and_first_string_argument() {
    let connection = Connection::open_in_memory().unwrap();
    for value in [
        json!({"children":[{"name":"other","arguments":["ignored"]},{"name":"in-reply-to","arguments":["message/first"]}]}),
        json!({"children":[{"name":"in-reply-to","arguments":[null]},{"name":"in-reply-to","arguments":["message/later"]}]}),
        json!({"children":[{"name":"in-reply-to","arguments":[]},{"name":"in-reply-to","arguments":["message/later"]}]}),
        json!({"children":["not-an-object",{"name":"in-reply-to","arguments":["bare"]}]}),
        json!({"children":[{"name":"in-reply-to","arguments":["", "message/later"]}]}),
        json!({"children":null}),
        json!({"children":"not-an-array"}),
        json!({}),
    ] {
        let (covered, parent) = source_parent(&connection, DESIRED_PARENT, &value);
        assert!(
            covered,
            "small valid desired source must be covered: {value}"
        );
        assert_eq!(
            parent,
            canonical_child_string(&value, "in-reply-to"),
            "{value}"
        );
    }
}

#[test]
fn oversized_source_requires_an_incomplete_cut_before_any_parent_is_used() {
    let connection = Connection::open_in_memory().unwrap();
    let value = json!({"fields":{"in_reply_to":"message/root","content":"x".repeat(MAX_SOURCE_BYTES as usize)}});
    assert_eq!(
        source_parent(&connection, CLAIM_PARENT, &value),
        (false, None)
    );
    let desired = json!({"children":[{"name":"in-reply-to","arguments":["message/root"]}],"padding":"x".repeat(MAX_SOURCE_BYTES as usize)});
    assert_eq!(
        source_parent(&connection, DESIRED_PARENT, &desired),
        (false, None)
    );
}
