//! Durable input for the agent card's local delivery source. Installation is explicit.
//! Producer capture runs outside Store transactions; namespace reducers consume only SQL.
//! Private certificates must also pass the live producer guard around the authorized read.
use std::sync::{Arc, Weak};

use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

use crate::api::delivery_presence::source::{self, CapturedAssessment, Certificate, Change};
use crate::store::Store;

pub(crate) const DRIVERS: [&str; 5] = ["claude", "codex", "opencode", "pi", "omp"];
#[cfg(test)]
const PAGE_LIMIT: usize = 128;
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_agent_delivery_presence (
 recipient TEXT NOT NULL, driver TEXT NOT NULL,
 producer_epoch TEXT NOT NULL, producer_revision INTEGER NOT NULL CHECK(producer_revision>=0),
 state TEXT NOT NULL CHECK(state IN ('pending','ready','fenced')),
 assessment TEXT, certificate TEXT, evaluation_time_ms INTEGER,
 next_deadline_ms INTEGER, error TEXT,
 PRIMARY KEY(recipient,driver),
 CHECK(state!='ready' OR (assessment IS NOT NULL AND certificate IS NOT NULL
  AND evaluation_time_ms IS NOT NULL AND next_deadline_ms IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS local_agent_delivery_due
 ON local_agent_delivery_presence(next_deadline_ms,recipient,driver) WHERE state='ready';
CREATE INDEX IF NOT EXISTS local_agent_delivery_pending
 ON local_agent_delivery_presence(recipient,driver) WHERE state!='ready';
"#;

pub(crate) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

/// The runtime owner must retain this registration. It has no strong Store cycle and neither
/// installs another Publisher nor certifies graph/source coverage. Managed source hooks and
/// this table's exact capture descriptor must already be installed by the composition owner.
pub(crate) fn install_sink(store: &Arc<Store>) -> Result<source::Registration<'static>> {
    ensure!(
        store.ivm_views().is_some(),
        "delivery source needs the shared registry"
    );
    let store: Weak<Store> = Arc::downgrade(store);
    source::install_sink(move |change| {
        let store = store
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("delivery Store closed"))?;
        changed(&store, change)
    })
}

fn changed(store: &Store, change: &Change) -> Result<()> {
    store
        .connection
        .batched(|tx| pending(tx, change))
        .map_err(anyhow::Error::msg)??;
    for driver in DRIVERS {
        let outcome = (|| {
            let capture = source::capture(&change.recipient, driver)?;
            ensure!(
                capture.certificate.epoch == change.epoch
                    && capture.certificate.revision >= change.revision,
                "delivery notification belongs to another producer boundary"
            );
            commit(store, &capture)
        })();
        if let Err(error) = outcome {
            // This durable fence helps namespace consumers. Even if this write fails, the
            // producer helper revokes live coverage and prevents serving persisted old rows.
            let reason = error.to_string();
            store
                .connection
                .batched(|tx| fence(tx, change, &reason))
                .map_err(anyhow::Error::msg)??;
            return Err(error);
        }
    }
    // DB notifications may precede acknowledgement. Existing collection dirty retries must
    // recheck the live guard after this success; no second Publisher or artificial DB wake.
    Ok(())
}

fn pending(tx: &Transaction<'_>, change: &Change) -> Result<()> {
    ensure!(
        !change.recipient.is_empty() && change.recipient.len() <= 1024,
        "invalid delivery recipient"
    );
    for driver in DRIVERS {
        tx.execute("INSERT INTO local_agent_delivery_presence(recipient,driver,producer_epoch,producer_revision,state)
            VALUES(?1,?2,?3,?4,'pending') ON CONFLICT(recipient,driver) DO UPDATE SET
            producer_epoch=excluded.producer_epoch,producer_revision=excluded.producer_revision,state='pending',error=NULL
            WHERE local_agent_delivery_presence.producer_epoch!=excluded.producer_epoch
               OR local_agent_delivery_presence.producer_revision<=excluded.producer_revision",
            params![change.recipient, driver, change.epoch, change.revision])?;
    }
    Ok(())
}

fn fence(tx: &Transaction<'_>, change: &Change, reason: &str) -> Result<()> {
    tx.execute(
        "UPDATE local_agent_delivery_presence SET state='fenced',error=?1
        WHERE recipient=?2 AND producer_epoch=?3 AND producer_revision<=?4",
        params![
            reason.chars().take(2048).collect::<String>(),
            change.recipient,
            change.epoch,
            change.revision
        ],
    )?;
    Ok(())
}

fn persist(tx: &Transaction<'_>, capture: &CapturedAssessment) -> Result<()> {
    let cert = &capture.certificate;
    ensure!(
        DRIVERS.contains(&cert.driver.as_str()),
        "unsupported delivery driver"
    );
    ensure!(
        !cert.recipient.is_empty() && cert.recipient.len() <= 1024 && cert.epoch.len() <= 128,
        "unbounded delivery certificate identity"
    );
    let assessment = serde_json::to_string(&capture.assessment)?;
    let certificate = serde_json::to_string(cert)?;
    ensure!(
        assessment.len() <= 16 * 1024 && certificate.len() <= 16 * 1024,
        "unbounded delivery capture"
    );
    let changed = tx.execute("INSERT INTO local_agent_delivery_presence
        (recipient,driver,producer_epoch,producer_revision,state,assessment,certificate,evaluation_time_ms,next_deadline_ms)
        VALUES(?1,?2,?3,?4,'ready',?5,?6,?7,?8) ON CONFLICT(recipient,driver) DO UPDATE SET
        producer_epoch=excluded.producer_epoch,producer_revision=excluded.producer_revision,state='ready',
        assessment=excluded.assessment,certificate=excluded.certificate,evaluation_time_ms=excluded.evaluation_time_ms,
        next_deadline_ms=excluded.next_deadline_ms,error=NULL
        WHERE local_agent_delivery_presence.producer_epoch!=excluded.producer_epoch
           OR local_agent_delivery_presence.producer_revision<=excluded.producer_revision",
        params![cert.recipient,cert.driver,cert.epoch,cert.revision,assessment,certificate,
            cert.evaluation_time_ms,cert.next_deadline_ms])?;
    ensure!(changed == 1, "newer delivery revision already persisted");
    Ok(())
}

pub(crate) fn commit(store: &Store, capture: &CapturedAssessment) -> Result<Certificate> {
    source::commit_capture(capture, |capture| {
        store
            .connection
            .batched(|tx| persist(tx, capture))
            .map_err(anyhow::Error::msg)??;
        Ok(())
    })
}

/// Indexed finite clock input. Capture/commit each returned key outside this snapshot/writer.
/// Wall time schedules conservatively; the live helper's monotonic deadline grants coverage.
#[cfg(test)]
pub(crate) fn due_page(
    connection: &Connection,
    at_ms: u64,
    limit: usize,
) -> Result<Vec<(String, String)>> {
    ensure!(
        (1..=PAGE_LIMIT).contains(&limit),
        "invalid delivery clock page limit"
    );
    let mut statement = connection.prepare("SELECT recipient,driver FROM local_agent_delivery_presence
        WHERE state='ready' AND next_deadline_ms<=?1 ORDER BY next_deadline_ms,recipient,driver LIMIT ?2")?;
    statement
        .query_map(params![at_ms, limit], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()
        .map_err(Into::into)
}

pub(crate) fn row(
    connection: &Connection,
    recipient: &str,
    driver: &str,
    at_ms: u64,
) -> Result<Option<CapturedAssessment>> {
    ensure!(DRIVERS.contains(&driver), "unsupported delivery driver");
    let row: Option<(String,String,String,u64,u64,u64)> = connection.query_row(
        "SELECT assessment,certificate,producer_epoch,producer_revision,evaluation_time_ms,next_deadline_ms
         FROM local_agent_delivery_presence WHERE recipient=?1 AND driver=?2 AND state='ready'",
        params![recipient,driver], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
    let Some((assessment, certificate, epoch, revision, evaluated, deadline)) = row else {
        return Ok(None);
    };
    ensure!(
        evaluated <= at_ms && at_ms < deadline,
        "delivery SQL deadline pending"
    );
    let certificate: Certificate = serde_json::from_str(&certificate)?;
    ensure!(
        certificate.recipient == recipient
            && certificate.driver == driver
            && certificate.epoch == epoch
            && certificate.revision == revision
            && certificate.evaluation_time_ms == evaluated
            && certificate.next_deadline_ms == deadline,
        "delivery row certificate does not match its source"
    );
    Ok(Some(CapturedAssessment {
        assessment: serde_json::from_str::<Value>(&assessment)?,
        certificate,
    }))
}

/// Decode the complete captured namespace physical row. This never reads the live producer
/// or current SQL, and cannot replace the whole-namespace boundary certificate/guard.
#[cfg(test)]
pub(crate) fn decode_sql_row(physical: &Value, at_ms: u64) -> Result<Option<CapturedAssessment>> {
    let state = physical["state"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("invalid delivery source state"))?;
    ensure!(
        matches!(state, "pending" | "ready" | "fenced"),
        "unsupported delivery source state"
    );
    if state != "ready" {
        return Ok(None);
    }
    let text = |key: &str| -> Result<&str> {
        physical[key]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("invalid delivery {key} SQL cell"))
    };
    let recipient = text("recipient")?;
    let driver = text("driver")?;
    ensure!(
        !recipient.is_empty() && recipient.len() <= 1024 && DRIVERS.contains(&driver),
        "invalid delivery source identity"
    );
    let assessment = text("assessment")?;
    let encoded = text("certificate")?;
    ensure!(
        assessment.len() <= 16 * 1024 && encoded.len() <= 16 * 1024,
        "delivery SQL payload budget exceeded"
    );
    let integer = |key: &str| -> Result<u64> {
        physical[key]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("invalid delivery {key} SQL integer"))
    };
    let evaluated = integer("evaluation_time_ms")?;
    let deadline = integer("next_deadline_ms")?;
    ensure!(
        evaluated <= at_ms && at_ms < deadline,
        "delivery namespace clock pending"
    );
    let certificate: Certificate = serde_json::from_str(encoded)?;
    ensure!(
        certificate.recipient == recipient
            && certificate.driver == driver
            && certificate.epoch == text("producer_epoch")?
            && certificate.revision == integer("producer_revision")?
            && certificate.evaluation_time_ms == evaluated
            && certificate.next_deadline_ms == deadline,
        "delivery namespace certificate binding mismatch"
    );
    Ok(Some(CapturedAssessment {
        assessment: serde_json::from_str(assessment)?,
        certificate,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn capture(recipient: &str, driver: &str, revision: u64, deadline: u64) -> CapturedAssessment {
        CapturedAssessment {
            assessment: json!({"state":"current","polled_seconds_ago":0}),
            certificate: serde_json::from_value(json!({
                "epoch":"fixture-source-epoch", "recipient":recipient,"driver":driver,"revision":revision,
                "evaluation_time_ms":100,"watermark_ns":10,"deadline_ns":1000,
                "next_deadline_ms":deadline,"follows":null
            })).unwrap(),
        }
    }

    #[test]
    fn ivm_delivery_sql_rows_keep_exact_evidence_and_indexed_deadlines() {
        let mut connection = Connection::open_in_memory().unwrap();
        create_schema(&connection).unwrap();
        let first = capture("agent/delivery-a", "codex", 2, 200);
        let second = capture("agent/delivery-b", "omp", 1, 300);
        let tx = connection.transaction().unwrap();
        persist(&tx, &first).unwrap();
        persist(&tx, &second).unwrap();
        tx.commit().unwrap();
        let selected = row(&connection, "agent/delivery-a", "codex", 150)
            .unwrap()
            .unwrap();
        assert_eq!(selected.assessment, first.assessment);
        assert_eq!(selected.certificate, first.certificate);
        assert_eq!(
            due_page(&connection, 200, 1).unwrap(),
            vec![("agent/delivery-a".into(), "codex".into())]
        );
        assert!(row(&connection, "agent/delivery-a", "codex", 200).is_err());
        assert!(row(&connection, "agent/delivery-a", "codex", 99).is_err());
        assert!(due_page(&connection, 300, 129).is_err());
        let plan: Vec<String> = connection.prepare("EXPLAIN QUERY PLAN SELECT recipient,driver FROM local_agent_delivery_presence
            WHERE state='ready' AND next_deadline_ms<=?1 ORDER BY next_deadline_ms,recipient,driver LIMIT 128").unwrap()
            .query_map([200],|row| row.get(3)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert!(
            plan.iter()
                .any(|line| line.contains("local_agent_delivery_due"))
        );
    }

    #[test]
    fn ivm_delivery_sql_old_callbacks_never_rewind_ready_rows() {
        let mut connection = Connection::open_in_memory().unwrap();
        create_schema(&connection).unwrap();
        let tx = connection.transaction().unwrap();
        persist(&tx, &capture("agent/delivery", "codex", 3, 200)).unwrap();
        pending(
            &tx,
            &Change {
                epoch: "fixture-source-epoch".into(),
                recipient: "agent/delivery".into(),
                revision: 2,
            },
        )
        .unwrap();
        assert!(persist(&tx, &capture("agent/delivery", "codex", 2, 200)).is_err());
        fence(
            &tx,
            &Change {
                epoch: "fixture-source-epoch".into(),
                recipient: "agent/delivery".into(),
                revision: 2,
            },
            "late failure",
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(
            row(&connection, "agent/delivery", "codex", 150)
                .unwrap()
                .unwrap()
                .certificate
                .revision,
            3
        );
        let tx = connection.transaction().unwrap();
        pending(
            &tx,
            &Change {
                epoch: "fixture-source-epoch".into(),
                recipient: "agent/delivery".into(),
                revision: 4,
            },
        )
        .unwrap();
        tx.commit().unwrap();
        assert!(
            row(&connection, "agent/delivery", "codex", 150)
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(test)]
mod namespace_decode_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn delivery_namespace_decoder_refuses_pending_expired_and_mismatched_evidence() {
        let mut row = json!({"recipient":"agent/namespace-delivery","driver":"codex","producer_epoch":"fixture-epoch","producer_revision":7,
            "state":"ready","assessment":"{\"state\":\"current\"}","evaluation_time_ms":100,"next_deadline_ms":200,
            "certificate":serde_json::to_string(&json!({"epoch":"fixture-epoch","recipient":"agent/namespace-delivery","driver":"codex","revision":7,"evaluation_time_ms":100,"watermark_ns":10,"deadline_ns":1000,"next_deadline_ms":200,"follows":null})).unwrap(),"error":null});
        let decoded = decode_sql_row(&row, 150).unwrap().unwrap();
        assert_eq!(decoded.assessment, json!({"state":"current"}));
        assert_eq!(decoded.certificate.revision, 7);
        assert!(decode_sql_row(&row, 200).is_err());
        row["producer_revision"] = json!(8);
        assert!(decode_sql_row(&row, 150).is_err());
        row["state"] = json!("pending");
        assert!(decode_sql_row(&row, 150).unwrap().is_none());
    }
}
