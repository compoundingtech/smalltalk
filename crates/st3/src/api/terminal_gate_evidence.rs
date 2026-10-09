//! Test-only reader. It does not change Doctor's public response or install anything.
use crate::store::terminal_gate_evidence::VIEW;
use anyhow::{Result, ensure};
use rusqlite::{Transaction, params};
use smallclaims::ivm::install::Installer;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Evidence {
    Unknown,
    Fenced {
        reason: String,
    },
    Warning {
        at_least: usize,
        truncated: bool,
        witnesses: Vec<String>,
    },
}

/// Caller supplies a pinned read. Four indexed statements on a ready nonempty fixture;
/// no full count, source decode, mutation, refresher or fallback. Root/cut is only the
/// fixture installer's readiness, not certification of native authority dependencies.
pub(crate) fn read(tx: &Transaction<'_>, installer: &Installer) -> Result<Evidence> {
    let root = match installer.root(tx, VIEW) {
        Ok(root) => root,
        Err(error) => {
            let reason = installer
                .status(tx, VIEW)
                .ok()
                .and_then(|status| status.error)
                .unwrap_or_else(|| error.to_string());
            return Ok(Evidence::Fenced {
                reason: reason.chars().take(256).collect(),
            });
        }
    };
    let (members, pending): (usize, bool) = tx.query_row(
        "SELECT members,pending FROM test_terminal_meta WHERE namespace=?1",
        [root.namespace.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(members <= 64, "terminal reader membership cap");
    if pending {
        return Ok(Evidence::Fenced {
            reason: "fixture publication pending".into(),
        });
    }
    if members == 0 {
        return Ok(Evidence::Unknown);
    }
    let mut statement = tx.prepare(
        "SELECT body FROM test_terminal_members WHERE namespace=?1 ORDER BY key LIMIT ?2",
    )?;
    let mut rows = statement.query(params![root.namespace.as_str(), 16])?;
    let (mut witnesses, mut bytes) = (Vec::new(), 0usize);
    while let Some(row) = rows.next()? {
        let body: String = row.get(0)?;
        ensure!(body.len() <= 1024, "terminal reader body cap");
        if bytes + body.len() > 4096 {
            break;
        }
        bytes += body.len();
        witnesses.push(body);
    }
    ensure!(
        !witnesses.is_empty(),
        "terminal membership/output inconsistent"
    );
    Ok(Evidence::Warning {
        at_least: members,
        truncated: members > witnesses.len(),
        witnesses,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::terminal_gate_evidence::tests::{Fixture, facts, mutation};

    #[test]
    fn dormant_reader_warns_with_a_bounded_lower_bound_then_refuses_fenced_evidence() {
        let mut fixture = Fixture::new();
        for n in 0..20 {
            fixture.record(mutation(&format!("gate/{n:03}"), Some(facts())));
        }
        let before = fixture.db.total_changes();
        let tx = fixture.db.transaction().unwrap();
        match read(&tx, &fixture.installer).unwrap() {
            Evidence::Warning {
                at_least,
                truncated,
                witnesses,
            } => {
                assert_eq!(at_least, 20);
                assert!(truncated);
                assert_eq!(witnesses.len(), 16);
            }
            result => panic!("{result:?}"),
        }
        tx.commit().unwrap();
        assert_eq!(fixture.db.total_changes(), before);
        let tx = fixture.db.transaction().unwrap();
        fixture
            .installer
            .source_gap(
                &tx,
                crate::store::terminal_gate_evidence::SOURCE,
                "uncaptured fixture repair",
            )
            .unwrap();
        tx.commit().unwrap();
        let tx = fixture.db.transaction().unwrap();
        assert!(
            matches!(read(&tx, &fixture.installer).unwrap(), Evidence::Fenced { reason } if reason.contains("uncaptured fixture repair"))
        );
    }

    #[test]
    fn dormant_reader_keeps_empty_cancelled_and_pending_evidence_unknown() {
        let mut fixture = Fixture::new();
        let tx = fixture.db.transaction().unwrap();
        assert_eq!(read(&tx, &fixture.installer).unwrap(), Evidence::Unknown);
        tx.commit().unwrap();
        fixture.record(mutation("gate/one", Some(facts())));
        let mut cancelled = facts();
        cancelled["run"]["status"] = serde_json::json!("cancelled");
        fixture.record(mutation("gate/one", Some(cancelled)));
        let tx = fixture.db.transaction().unwrap();
        assert_eq!(read(&tx, &fixture.installer).unwrap(), Evidence::Unknown);
        tx.commit().unwrap();
        fixture.record(mutation("gate/one", Some(facts())));
        let tx = fixture.db.transaction().unwrap();
        let root = fixture.installer.root(&tx, VIEW).unwrap();
        tx.execute(
            "UPDATE test_terminal_meta SET pending=1 WHERE namespace=?1",
            [root.namespace.as_str()],
        )
        .unwrap();
        assert!(
            matches!(read(&tx, &fixture.installer).unwrap(), Evidence::Fenced { reason } if reason.contains("pending"))
        );
    }
}
