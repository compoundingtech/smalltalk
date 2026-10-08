//! Read-after-write receipts and reactive application waits. No routes or catch-up worker.
//!
//! Local tokens are database/epoch bound. Replicated tokens must explicitly map the claim
//! identity into the receiver's log; indices from different machines are never comparable.
//! A receipt is neither authorization nor evidence that a maximum applied key is a prefix.
//! Only the runtime-owned certified source cut plus view readiness admits a ready read.

use super::{
    Readiness, Views,
    events::{self, Boundary, ProviderIdentity, Publisher},
};
use crate::{ClaimInput, ClaimRecord, Store};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{broadcast, watch},
    time::Instant,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteReceipt {
    pub database_id: String,
    pub epoch: u64,
    pub claim_id: String,
    pub store_index: u64,
}

/// The write is committed even when its receipt cannot be captured. Do not present a
/// post-commit metadata error as a rolled-back write or retry a non-idempotent command.
#[derive(Debug)]
pub enum WriteOutcome {
    Committed {
        claim: ClaimRecord,
        receipt: WriteReceipt,
    },
    CommittedWithoutReceipt {
        claim: ClaimRecord,
        reason: String,
    },
}

/// Opt-in wrapper around the existing admitted Store append. No source replay or new write
/// semantics. Production runtimes retain admission/idempotency and their own view adapters.
pub fn write(store: &Store, input: &ClaimInput) -> std::result::Result<WriteOutcome, crate::Error> {
    let claim = store.append_claim(input)?;
    Ok(match committed_receipt(store, &claim) {
        Ok(receipt) => WriteOutcome::Committed { claim, receipt },
        Err(error) => WriteOutcome::CommittedWithoutReceipt {
            claim,
            reason: format!("{error:#}"),
        },
    })
}

/// Capture only an already committed record. Never issue a receipt from an unfinished
/// transaction; this short read snapshot verifies the ID and local position together.
pub fn committed_receipt(store: &Store, claim: &ClaimRecord) -> Result<WriteReceipt> {
    store.read_snapshot(|_| {
        let connection = store.readers.get();
        let frontiers = events::frontiers(&connection)?;
        let index: Option<u64> = connection
            .query_row(
                "SELECT store_index FROM claims WHERE id=?1",
                [&claim.id],
                |row| row.get(0),
            )
            .optional()?;
        ensure!(
            index == Some(claim.store_index),
            "write is missing or its local position changed"
        );
        let receipt = WriteReceipt {
            database_id: frontiers.database_id,
            epoch: frontiers.source_cut.epoch,
            claim_id: claim.id.clone(),
            store_index: claim.store_index,
        };
        validate(&receipt)?;
        Ok(receipt)
    })
}

/// Routing is explicit: a rotated/restored local database cannot silently reinterpret its
/// old receipt as a remote one. Replicated mode is chosen by the receiving application.
#[derive(Clone, Copy, Debug)]
pub enum Target<'a> {
    Local(&'a WriteReceipt),
    Replicated(&'a WriteReceipt),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gap {
    DatabaseReplaced,
    EpochChanged,
    UnknownLocalWrite,
    ClaimPositionChanged,
    ProviderReplaced,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum After {
    Ready {
        boundary: Boundary,
        mapped_index: u64,
    },
    Pending {
        boundary: Boundary,
        mapped_index: Option<u64>,
    },
    Resync {
        boundary: Boundary,
        reason: Gap,
    },
}

fn validate(receipt: &WriteReceipt) -> Result<()> {
    ensure!(
        !receipt.database_id.is_empty() && receipt.database_id.len() <= 128,
        "invalid receipt database identity"
    );
    ensure!(
        !receipt.claim_id.is_empty() && receipt.claim_id.len() <= 1024,
        "invalid receipt claim identity"
    );
    ensure!(
        receipt.epoch > 0
            && receipt.epoch <= i64::MAX as u64
            && receipt.store_index > 0
            && receipt.store_index <= i64::MAX as u64,
        "invalid receipt log position"
    );
    Ok(())
}

/// Call in the same read snapshot as the predicate/output. This conservatively requires
/// the entire admitted source to be processed and the requested view ready. MAX of per-view
/// applied indices, key generation and semantic generation never substitute for that cut.
pub fn inspect(
    connection: &Connection,
    views: &Views,
    view: &str,
    target: Target<'_>,
) -> Result<After> {
    let receipt = match target {
        Target::Local(r) | Target::Replicated(r) => r,
    };
    validate(receipt)?;
    let boundary = events::capture(connection, views, view)?;
    let gap = if matches!(target, Target::Local(_)) {
        if boundary.identity.database_id != receipt.database_id {
            Some(Gap::DatabaseReplaced)
        } else if boundary.source_cut.epoch != receipt.epoch {
            Some(Gap::EpochChanged)
        } else {
            None
        }
    } else {
        None
    };
    if let Some(reason) = gap {
        return Ok(After::Resync { boundary, reason });
    }
    let mapped_index = connection
        .query_row(
            "SELECT store_index FROM claims WHERE id=?1",
            [&receipt.claim_id],
            |r| r.get::<_, u64>(0),
        )
        .optional()?;
    if matches!(target, Target::Local(_)) {
        let gap = match mapped_index {
            None => Some(Gap::UnknownLocalWrite),
            Some(index) if index != receipt.store_index => Some(Gap::ClaimPositionChanged),
            _ => None,
        };
        if let Some(reason) = gap {
            return Ok(After::Resync { boundary, reason });
        }
    }
    if let Some(index) = mapped_index
        && boundary.source_cut.projected >= index
        && matches!(boundary.availability.readiness, Readiness::Ready(_))
    {
        return Ok(After::Ready {
            boundary,
            mapped_index: index,
        });
    }
    Ok(After::Pending {
        boundary,
        mapped_index,
    })
}

#[derive(Debug)]
pub enum WaitOutcome<T> {
    Ready {
        value: T,
        boundary: Boundary,
        mapped_index: u64,
    },
    Resync {
        boundary: Boundary,
        reason: Gap,
    },
    TimedOut,
    Cancelled,
    PublisherClosed,
}

pub struct WaitOptions {
    pub deadline: Instant,
    pub cancellation: watch::Receiver<bool>,
}

/// Subscribe before the authoritative snapshot, release it before awaiting, and recheck
/// after every notice or lag. `read` is side-effect-free and must evaluate current authority,
/// owner/incarnation/dependencies in that SAME snapshot. Its value is not a permission to
/// perform later external effects without a fresh effect fence.
///
/// Missing/fenced views wait for availability changes until the caller's deadline/cancel.
/// Missing feed/schema or database errors propagate; they never become successful reads.
/// Publisher drop returns Closed. Cancellation sender closure also cancels. No polling,
/// timeout retry, source replay or automatic initialization runs here.
pub async fn wait<T>(
    store: &Store,
    views: &Views,
    view: &str,
    target: Target<'_>,
    publisher: &Publisher,
    options: WaitOptions,
    read: impl FnMut(&Connection, &Boundary) -> Result<T>,
) -> Result<WaitOutcome<T>> {
    wait_subscribed(
        store,
        views,
        view,
        target,
        publisher.subscribe(),
        options,
        read,
    )
    .await
}

/// Already-subscribed form for bridges that capture their subscription before other work.
/// Owning the receiver lets the publisher shut down independently while this wait is pending.
pub async fn wait_subscribed<T>(
    store: &Store,
    views: &Views,
    view: &str,
    target: Target<'_>,
    mut notices: broadcast::Receiver<events::Notice>,
    options: WaitOptions,
    mut read: impl FnMut(&Connection, &Boundary) -> Result<T>,
) -> Result<WaitOutcome<T>> {
    let WaitOptions {
        deadline,
        mut cancellation,
    } = options;
    let mut identity: Option<ProviderIdentity> = None;
    loop {
        if *cancellation.borrow() || cancellation.has_changed().is_err() {
            return Ok(WaitOutcome::Cancelled);
        }
        if Instant::now() >= deadline {
            return Ok(WaitOutcome::TimedOut);
        }
        let result = store.read_snapshot(|_| {
            let connection = store.readers.get();
            let state = inspect(&connection, views, view, target)?;
            let boundary = match &state {
                After::Ready { boundary, .. }
                | After::Pending { boundary, .. }
                | After::Resync { boundary, .. } => boundary,
            };
            if identity.as_ref().is_some_and(|id| id != &boundary.identity) {
                return Ok(Some(WaitOutcome::Resync {
                    boundary: boundary.clone(),
                    reason: Gap::ProviderReplaced,
                }));
            }
            identity = Some(boundary.identity.clone());
            Ok(match state {
                After::Ready {
                    boundary,
                    mapped_index,
                } => {
                    let value = read(&connection, &boundary)?;
                    Some(WaitOutcome::Ready {
                        value,
                        boundary,
                        mapped_index,
                    })
                }
                After::Resync { boundary, reason } => {
                    Some(WaitOutcome::Resync { boundary, reason })
                }
                After::Pending { .. } => None,
            })
        })?;
        if let Some(result) = result {
            return Ok(result);
        }
        tokio::select! {
            biased;
            changed = cancellation.changed() => {
                if changed.is_err() || *cancellation.borrow_and_update() {
                    return Ok(WaitOutcome::Cancelled);
                }
            }
            _ = tokio::time::sleep_until(deadline) => return Ok(WaitOutcome::TimedOut),
            notice = notices.recv() => match notice {
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {},
                Err(broadcast::error::RecvError::Closed) => return Ok(WaitOutcome::PublisherClosed),
            },
        }
    }
}
