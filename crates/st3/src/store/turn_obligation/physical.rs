//! Exact physical OLD/NEW reselection for the enclosing Source's shared row budget.
//! No producer reconstruction, namespace, registry or formatter reads live here.
use super::*;

/// Schema-order descriptors for the enclosing Source's typed Table conversion.
/// This is an owned physical contract, not a Source registry or activation certificate.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TableDescriptor {
    pub name: &'static str,
    pub columns: &'static [&'static str],
    pub key: &'static [&'static str],
}

pub(crate) const TABLES: [TableDescriptor; 5] = [
    TableDescriptor {
        name: "agent_turn_obligation_evidence",
        columns: &[
            "subject",
            "receipt",
            "claim_id",
            "native_key",
            "terminal",
            "canonical_key",
            "visible_index",
            "body",
        ],
        key: &["subject", "receipt", "claim_id", "terminal"],
    },
    TableDescriptor {
        name: "agent_turn_obligations",
        columns: &[
            "subject",
            "receipt",
            "native_key",
            "source_claim",
            "source_key",
            "body",
            "first_index",
            "terminal_index",
            "acknowledged_index",
        ],
        key: &["subject", "receipt"],
    },
    TableDescriptor {
        name: "local_turn_obligation_versions",
        columns: &[
            "subject",
            "receipt",
            "visible_index",
            "source_claim",
            "body",
            "terminal",
        ],
        key: &["subject", "receipt", "visible_index"],
    },
    TableDescriptor {
        name: "local_turn_obligation_pending",
        columns: &["claim_id", "subject"],
        key: &["claim_id"],
    },
    TableDescriptor {
        name: "local_turn_obligation_dirty",
        columns: &["subject"],
        key: &["subject"],
    },
];

pub(crate) fn schema_fingerprint() -> Result<String> {
    canonical_hash(&json!({"contract":"turn-obligation-physical.v1",
        "tables": TABLES.iter().map(|table| json!({"name":table.name,"columns":table.columns,"key":table.key})).collect::<Vec<_>>() }))
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Key {
    Evidence {
        subject: String,
        receipt: String,
        claim: String,
        terminal: bool,
    },
    Head {
        subject: String,
        receipt: String,
    },
    Version {
        subject: String,
        receipt: String,
        index: u64,
    },
    Pending {
        claim: String,
    },
    Dirty {
        subject: String,
    },
}

/// Complete physical cells in schema order, retained before mutation by the Source.
/// The physical PK survives deletion, body changes and canonical renumbering.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Row {
    pub key: Key,
    pub cells: Vec<Value>,
}

#[derive(Clone, Debug)]
pub(crate) struct Change {
    pub old: Option<Row>,
    pub new_key: Option<Key>,
}

#[derive(Clone, Debug)]
pub(crate) struct Reselected {
    pub old: Option<Row>,
    pub new: Option<Row>,
    pub subjects: BTreeSet<String>,
}

impl Key {
    fn lookup(&self) -> (&'static str, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value as Cell;
        match self {
            Self::Evidence {
                subject,
                receipt,
                claim,
                terminal,
            } => (
                "SELECT * FROM agent_turn_obligation_evidence WHERE subject=?1 AND receipt=?2 AND claim_id=?3 AND terminal=?4",
                vec![
                    Cell::Text(subject.clone()),
                    Cell::Text(receipt.clone()),
                    Cell::Text(claim.clone()),
                    Cell::Integer(i64::from(*terminal)),
                ],
            ),
            Self::Head { subject, receipt } => (
                "SELECT * FROM agent_turn_obligations WHERE subject=?1 AND receipt=?2",
                vec![Cell::Text(subject.clone()), Cell::Text(receipt.clone())],
            ),
            Self::Version {
                subject,
                receipt,
                index,
            } => (
                "SELECT * FROM local_turn_obligation_versions WHERE subject=?1 AND receipt=?2 AND visible_index=?3",
                vec![
                    Cell::Text(subject.clone()),
                    Cell::Text(receipt.clone()),
                    Cell::Integer((*index).min(i64::MAX as u64) as i64),
                ],
            ),
            Self::Pending { claim } => (
                "SELECT * FROM local_turn_obligation_pending WHERE claim_id=?1",
                vec![Cell::Text(claim.clone())],
            ),
            Self::Dirty { subject } => (
                "SELECT * FROM local_turn_obligation_dirty WHERE subject=?1",
                vec![Cell::Text(subject.clone())],
            ),
        }
    }
}

impl Row {
    fn subject(&self) -> Result<String> {
        let c = &self.cells;
        let subject = match &self.key {
            Key::Evidence {
                subject,
                receipt,
                claim,
                terminal,
            } => {
                anyhow::ensure!(
                    c.len() == 8
                        && c[0] == *subject
                        && c[1] == *receipt
                        && c[2] == *claim
                        && c[4] == i64::from(*terminal),
                    "invalid retained evidence PK"
                );
                subject.as_str()
            }
            Key::Head { subject, receipt } => {
                anyhow::ensure!(
                    c.len() == 9 && c[0] == *subject && c[1] == *receipt,
                    "invalid retained head PK"
                );
                subject.as_str()
            }
            Key::Version {
                subject,
                receipt,
                index,
            } => {
                anyhow::ensure!(
                    *index <= i64::MAX as u64
                        && c.len() == 6
                        && c[0] == *subject
                        && c[1] == *receipt
                        && c[2] == *index,
                    "invalid retained version PK"
                );
                subject.as_str()
            }
            Key::Pending { claim } => {
                anyhow::ensure!(
                    c.len() == 2 && c[0] == *claim,
                    "invalid retained pending PK"
                );
                c[1].as_str().context("pending row has no subject")?
            }
            Key::Dirty { subject } => {
                anyhow::ensure!(
                    c.len() == 1 && c[0] == *subject,
                    "invalid retained dirty PK"
                );
                subject.as_str()
            }
        };
        anyhow::ensure!(!subject.is_empty(), "physical receipt row has no subject");
        anyhow::ensure!(
            serde_json::to_vec(c)?.len() <= 64 * 1024,
            "physical receipt row exceeds Source cap"
        );
        Ok(subject.to_owned())
    }
}

/// Charges every lookup, even an absent row, against the caller's aggregate budget.
/// Capture OLD before the mutation on the same commit-inclusive Source connection.
pub(crate) fn capture_row(
    connection: &Connection,
    key: &Key,
    remaining: &mut usize,
) -> Result<Option<Row>> {
    anyhow::ensure!(
        *remaining <= 128 && *remaining > 0,
        "physical receipt capture budget exhausted"
    );
    if let Key::Version { index, .. } = key {
        anyhow::ensure!(
            *index <= i64::MAX as u64,
            "physical cut key exceeds SQLite range"
        );
    }
    *remaining -= 1;
    let (sql, parameters) = key.lookup();
    let mut statement = connection.prepare_cached(sql)?;
    let count = statement.column_count();
    let cells = statement
        .query_row(rusqlite::params_from_iter(parameters), |row| {
            (0..count)
                .map(|column| match row.get_ref(column)? {
                    rusqlite::types::ValueRef::Null => Ok(Value::Null),
                    rusqlite::types::ValueRef::Integer(value) => Ok(json!(value)),
                    rusqlite::types::ValueRef::Text(value) => std::str::from_utf8(value)
                        .map(|value| json!(value))
                        .map_err(|_| rusqlite::Error::InvalidQuery),
                    _ => Err(rusqlite::Error::InvalidQuery),
                })
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .optional()?;
    let row = cells.map(|cells| Row {
        key: key.clone(),
        cells,
    });
    if let Some(row) = &row {
        row.subject()?;
    }
    Ok(row)
}

/// Reselect exact NEW physical keys; retain OLD subjects even after removal/retargeting.
/// Metadata/order correction changes the complete row without changing its immutable PK.
/// Caller combines this cost with the other families and visibility/card repair work.
/// On any refusal the enclosing Source must defer the entire family, not publish null.
pub(crate) fn reselect(
    connection: &Connection,
    changes: &[Change],
    remaining: &mut usize,
) -> Result<Vec<Reselected>> {
    let required = changes
        .iter()
        .map(|change| usize::from(change.old.is_some()) + usize::from(change.new_key.is_some()))
        .sum::<usize>();
    anyhow::ensure!(
        *remaining <= 128
            && changes.len() <= 128
            && required <= *remaining
            && changes
                .iter()
                .all(|change| change.old.is_some() || change.new_key.is_some()),
        "physical receipt change batch exceeds budget"
    );
    let mut result = Vec::with_capacity(changes.len());
    for change in changes {
        let mut subjects = BTreeSet::new();
        if let Some(old) = &change.old {
            anyhow::ensure!(*remaining > 0, "retained receipt consume budget exhausted");
            *remaining -= 1;
            subjects.insert(old.subject()?);
        }
        let new = change
            .new_key
            .as_ref()
            .map(|key| capture_row(connection, key, remaining))
            .transpose()?
            .flatten();
        if let Some(new) = &new {
            subjects.insert(new.subject()?);
        }
        result.push(Reselected {
            old: change.old.clone(),
            new,
            subjects,
        });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_descriptors_match_actual_schema_order_and_primary_keys() {
        let store = Store::open_memory("descriptor-fixture").unwrap();
        let connection = store.readers.get();
        for table in TABLES {
            let rows = connection
                .prepare(&format!("PRAGMA table_info({})", table.name))
                .unwrap()
                .query_map([], |row| {
                    Ok((row.get::<_, String>(1)?, row.get::<_, usize>(5)?))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(
                rows.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(),
                table.columns
            );
            let mut keys = rows.iter().filter(|row| row.1 > 0).collect::<Vec<_>>();
            keys.sort_by_key(|row| row.1);
            assert_eq!(
                keys.iter().map(|row| row.0.as_str()).collect::<Vec<_>>(),
                table.key
            );
        }
        println!(
            "owned physical schema fingerprint={}; reducer fingerprint={}",
            schema_fingerprint().unwrap(),
            super::super::reducer_fingerprint().unwrap()
        );
    }

    #[test]
    fn physical_turn_old_new_removal_retarget_and_canonical_renumbering() {
        let store = Store::open_memory("physical-turn-fixture").unwrap();
        let connection = store.connection.write();
        connection.execute("INSERT INTO agent_turn_obligation_evidence VALUES('agent/old','r','c',NULL,0,'01',1,'null')", []).unwrap();
        connection.execute("INSERT INTO agent_turn_obligations VALUES('agent/old','r',NULL,'c','01','null',1,NULL,NULL)", []).unwrap();
        connection
            .execute(
                "INSERT INTO local_turn_obligation_versions VALUES('agent/old','r',1,'c','null',0)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO local_turn_obligation_pending VALUES('c','agent/old')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO local_turn_obligation_dirty VALUES('agent/old')",
                [],
            )
            .unwrap();
        let keys = vec![
            Key::Evidence {
                subject: "agent/old".into(),
                receipt: "r".into(),
                claim: "c".into(),
                terminal: false,
            },
            Key::Head {
                subject: "agent/old".into(),
                receipt: "r".into(),
            },
            Key::Version {
                subject: "agent/old".into(),
                receipt: "r".into(),
                index: 1,
            },
            Key::Pending { claim: "c".into() },
            Key::Dirty {
                subject: "agent/old".into(),
            },
        ];
        let mut budget = 128;
        let old = keys
            .iter()
            .map(|key| capture_row(&connection, key, &mut budget).unwrap().unwrap())
            .collect::<Vec<_>>();
        connection
            .execute(
                "UPDATE agent_turn_obligation_evidence SET canonical_key='02'",
                [],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE local_turn_obligation_pending SET subject='agent/new'",
                [],
            )
            .unwrap();
        connection
            .execute("DELETE FROM agent_turn_obligations", [])
            .unwrap();
        connection
            .execute("DELETE FROM local_turn_obligation_versions", [])
            .unwrap();
        connection
            .execute("DELETE FROM local_turn_obligation_dirty", [])
            .unwrap();
        let changes = old
            .into_iter()
            .zip(keys)
            .map(|(old, key)| Change {
                old: Some(old),
                new_key: Some(key),
            })
            .collect::<Vec<_>>();
        let selected = reselect(&connection, &changes, &mut budget).unwrap();
        assert_eq!(
            budget, 113,
            "five initial reads + five retained OLD + five NEW lookups"
        );
        assert_eq!(selected[0].old.as_ref().unwrap().cells[5], "01");
        assert_eq!(selected[0].new.as_ref().unwrap().cells[5], "02");
        assert!(
            selected[1].new.is_none() && selected[2].new.is_none() && selected[4].new.is_none()
        );
        assert_eq!(
            selected[3].subjects,
            BTreeSet::from(["agent/old".into(), "agent/new".into()])
        );
        assert_eq!(selected[1].subjects, BTreeSet::from(["agent/old".into()]));
    }

    #[test]
    fn physical_turn_budget_counts_absence_and_rejects_bad_retained_keys() {
        let store = Store::open_memory("physical-turn-budget-fixture").unwrap();
        let connection = store.connection.write();
        let key = Key::Dirty {
            subject: "agent/missing".into(),
        };
        let mut budget = 128;
        for _ in 0..128 {
            assert!(
                capture_row(&connection, &key, &mut budget)
                    .unwrap()
                    .is_none()
            );
        }
        assert!(capture_row(&connection, &key, &mut budget).is_err());
        assert_eq!(budget, 0);
        let forged = Row {
            key: key.clone(),
            cells: vec![json!("agent/other")],
        };
        assert!(
            reselect(
                &connection,
                &[Change {
                    old: Some(forged),
                    new_key: None
                }],
                &mut 128
            )
            .is_err()
        );
        let out_of_range = Key::Version {
            subject: "agent/a".into(),
            receipt: "r".into(),
            index: u64::MAX,
        };
        assert!(capture_row(&connection, &out_of_range, &mut 128).is_err());
    }
}
