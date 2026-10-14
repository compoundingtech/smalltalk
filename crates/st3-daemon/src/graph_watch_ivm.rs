//! Bridge from committed IVM invalidations to graph-watch consumers.
//!
//! The IVM journal is a coalesced current-row feed. It never selects a durable agent wake:
//! those require an occurrence-preserving canonical transition source. This bridge holds
//! the publisher lifetime, subscribes before capturing an authorized snapshot, and reads
//! bounded indexed pages only after a commit notice or an explicit client continuation.
//! The caller owns row authorization, window ranking, delta encoding and resync delivery.

use std::{borrow::Borrow, sync::Arc};

use anyhow::Result;
use rusqlite::Connection;
use smallclaims::{
    Store,
    ivm::{Readiness, Views, events},
};
use tokio::sync::broadcast;

pub(crate) struct IvmViewBridge<S = Store> {
    store: Arc<S>,
    views: Arc<Views>,
    publisher: Arc<events::Publisher>,
}

pub(crate) struct OpenView<T> {
    pub notices: broadcast::Receiver<events::Notice>,
    pub boundary: events::Boundary,
    /// None means output is not ready. Boundary still carries current availability.
    pub snapshot: Option<T>,
}

pub(crate) enum ViewWake {
    Committed(events::CommitFrontiers),
    /// A dropped notice is recovered by checking the indexed journal and its floor.
    Lagged,
    /// A failed provider capture never becomes a successful update or synthetic row.
    Unavailable(String),
    Closed,
}

/// Both streams are read at the same database cut. The caller applies a Changes page
/// before retaining either returned cursor; a failed delivery leaves both old cursors
/// available for the next attempt. This page carries invalidations, not row contents.
pub(crate) enum ViewPage<T> {
    Changes {
        /// The callback receives raw view-global keys, but only its authorized result
        /// can leave this bridge. It must refresh the bounded window when readiness
        /// becomes Ready, even when no semantic key was invalidated.
        output: Option<T>,
        has_availability_change: bool,
        next_keys: events::ChangeCursor,
        next_status: events::ChangeCursor,
        more: bool,
        boundary: events::Boundary,
    },
    Resync {
        reason: events::Gap,
        boundary: events::Boundary,
    },
    SnapshotChanged {
        boundary: events::Boundary,
    },
}

impl<S: Borrow<Store>> IvmViewBridge<S> {
    /// Use the runtime owner's one publisher for this exact graph Store. The installer
    /// owns the journal and both receipt waits and socket consumers share this publisher.
    /// Construction does no SQL and attaches no additional commit observer.
    pub(crate) fn from_shared(
        store: Arc<S>,
        views: Arc<Views>,
        publisher: Arc<events::Publisher>,
    ) -> Self {
        Self {
            store,
            views,
            publisher,
        }
    }

    /// Register one socket before any of its authorized view captures.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<events::Notice> {
        self.publisher.subscribe()
    }

    /// Register for commits before reading authorization, output and both cursors in one
    /// short snapshot. Authorization runs before exposing availability or a boundary.
    /// The caller must release `OpenView`'s snapshot work before awaiting.
    pub(crate) fn subscribe_snapshot<T>(
        &self,
        view: &str,
        authorize: impl FnOnce(&Connection) -> Result<()>,
        read: impl FnOnce(&Connection, &events::Boundary) -> Result<T>,
    ) -> Result<OpenView<T>> {
        let notices = self.publisher.subscribe();
        let store = self.store.as_ref().borrow();
        let (boundary, snapshot) = store.read_snapshot(|_| {
            let connection = store.readers.get();
            authorize(&connection)?;
            let boundary = events::capture(&connection, &self.views, view)?;
            let snapshot = if matches!(&boundary.availability.readiness, Readiness::Ready(_)) {
                Some(read(&connection, &boundary)?)
            } else {
                None
            };
            Ok((boundary, snapshot))
        })?;
        Ok(OpenView {
            notices,
            boundary,
            snapshot,
        })
    }

    /// Drain at most `limit` invalidations from each stream and read affected output
    /// in the same database snapshot. Authorization runs before exposing status or rows;
    /// the callback must select only authorized keys and return no raw view-global key.
    /// It is skipped while the view is not Ready. Availability
    /// diagnostics remain in the returned boundary. A continuation passes the previous
    /// page's snapshot version; the first page after a commit passes None. Expiry or
    /// provider replacement requires a fresh authorized output snapshot.
    pub(crate) fn page_with_rows<T>(
        &self,
        keys: &events::ChangeCursor,
        status: &events::ChangeCursor,
        limit: usize,
        expected: Option<&events::SnapshotVersion>,
        authorize: impl FnOnce(&Connection) -> Result<()>,
        read: impl FnOnce(
            &Connection,
            &events::Boundary,
            &[events::ViewChanged],
            &[events::ViewAvailabilityChanged],
        ) -> Result<T>,
    ) -> Result<ViewPage<T>> {
        anyhow::ensure!(
            keys.identity == status.identity,
            "key and availability cursors belong to different providers"
        );
        let store = self.store.as_ref().borrow();
        store.read_snapshot(|_| {
            let connection = store.readers.get();
            authorize(&connection)?;
            let key_page = events::keys(&connection, &self.views, keys, limit, expected)?;
            let status_page =
                events::availability(&connection, &self.views, status, limit, expected)?;
            match (key_page, status_page) {
                (events::Page::Resync { reason, boundary }, _)
                | (_, events::Page::Resync { reason, boundary }) => {
                    Ok(ViewPage::Resync { reason, boundary })
                }
                (events::Page::SnapshotChanged { boundary }, _)
                | (_, events::Page::SnapshotChanged { boundary }) => {
                    Ok(ViewPage::SnapshotChanged { boundary })
                }
                (
                    events::Page::Changes {
                        items: keys,
                        next: next_keys,
                        more: more_keys,
                        boundary,
                    },
                    events::Page::Changes {
                        items: availability,
                        next: next_status,
                        more: more_status,
                        ..
                    },
                ) => {
                    let output = if matches!(&boundary.availability.readiness, Readiness::Ready(_))
                    {
                        Some(read(&connection, &boundary, &keys, &availability)?)
                    } else {
                        None
                    };
                    Ok(ViewPage::Changes {
                        output,
                        has_availability_change: !availability.is_empty(),
                        next_keys,
                        next_status,
                        more: more_keys || more_status,
                        boundary,
                    })
                }
            }
        })
    }

    pub(crate) async fn next_notice(
        receiver: &mut broadcast::Receiver<events::Notice>,
    ) -> ViewWake {
        match receiver.recv().await {
            Ok(events::Notice::Committed(frontiers)) => ViewWake::Committed(frontiers),
            Ok(events::Notice::Unavailable(error)) => ViewWake::Unavailable(error),
            Err(broadcast::error::RecvError::Lagged(_)) => ViewWake::Lagged,
            Err(broadcast::error::RecvError::Closed) => ViewWake::Closed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use smallclaims::{
        ClaimRecord,
        ivm::{Contribution, Definition, SourceCut, View},
        store::{append_claim_record_tx, canonical},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Rows;
    impl View for Rows {
        fn definition(&self) -> Definition {
            Definition {
                name: "fixture.rows",
                fingerprint: "fixture.rows.v1",
                kinds: &["fixture.row"],
                local_kinds: &[],
                max_contributions: 1,
            }
        }
        fn contributions(
            &self,
            claim: &ClaimRecord,
            rank: &canonical::ClaimKey,
        ) -> Result<Vec<Contribution>> {
            Ok(vec![Contribution {
                key: claim.subject.clone(),
                register: "row".into(),
                value: claim.body["fields"]["row"].clone(),
                rank: canonical::sortable_key(rank),
            }])
        }
    }

    // The real st3 wrapper exercises Borrow<GraphStore>. Source maintenance is explicit
    // in this transport fixture; it does not certify the production card dependencies.
    struct Fixture {
        store: Arc<crate::store::Store>,
        views: Arc<Views>,
        publisher: Arc<events::Publisher>,
        bridge: IvmViewBridge<crate::store::Store>,
    }
    impl Fixture {
        fn new(capacity: usize) -> Self {
            let store = Arc::new(crate::store::Store::open_memory("alder").unwrap());
            let views = Arc::new(Views::new(vec![Box::new(Rows)]).unwrap());
            {
                let mut writer = store.connection.write();
                views.create_schema(&writer).unwrap();
                let tx = writer.transaction().unwrap();
                views.initialize_empty(&tx, cut(0)).unwrap();
                events::install(&tx, capacity).unwrap();
                tx.commit().unwrap();
            }
            let publisher =
                Arc::new(events::Publisher::attach(store.as_ref().borrow(), 2).unwrap());
            let bridge =
                IvmViewBridge::from_shared(store.clone(), views.clone(), publisher.clone());
            Self {
                store,
                views,
                publisher,
                bridge,
            }
        }
        fn append(&self, key: &str, row: Value) {
            self.store
                .connection
                .batched(|tx| {
                    let claim = append_claim_record_tx(
                        tx,
                        self.store.origin(),
                        key,
                        "fixture.row",
                        None,
                        &json!({"fields":{"row":row}}),
                        &[],
                        None,
                    )?;
                    let rank = canonical::claim_key(tx, &claim.id)?;
                    self.views.change(tx, None, Some((&claim, &rank)), 1)?;
                    self.views.publish_cut(tx, cut(claim.store_index))?;
                    Ok::<_, anyhow::Error>(())
                })
                .unwrap()
                .unwrap();
        }
    }
    fn cut(index: u64) -> SourceCut {
        SourceCut {
            epoch: 1,
            admitted: index,
            projected: index,
            local_generation: 0,
        }
    }

    #[tokio::test]
    async fn borrowed_st3_store_shares_publisher_and_keeps_post_capture_commit() {
        let fixture = Fixture::new(32);
        let authorized = AtomicUsize::new(0);
        let open = fixture
            .bridge
            .subscribe_snapshot(
                "fixture.rows",
                |_| {
                    authorized.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                |_, boundary| Ok(boundary.source_cut),
            )
            .unwrap();
        assert_eq!(authorized.load(Ordering::SeqCst), 1);
        assert_eq!(open.snapshot, Some(cut(0)));
        let mut notices = open.notices;
        let mut waiter = fixture.publisher.subscribe();
        fixture.append("row/visible", json!({"id":"row/visible","value":1}));
        let ViewWake::Committed(socket_cut) =
            IvmViewBridge::<crate::store::Store>::next_notice(&mut notices).await
        else {
            panic!("missing socket wake");
        };
        let events::Notice::Committed(wait_cut) = waiter.recv().await.unwrap() else {
            panic!("missing shared receipt wake");
        };
        assert_eq!(socket_cut, wait_cut);
        assert_eq!(socket_cut.source_cut.projected, 1);
        let ViewPage::Changes {
            output,
            next_keys,
            next_status,
            boundary,
            ..
        } = fixture
            .bridge
            .page_with_rows(
                &open.boundary.keys,
                &open.boundary.status,
                1,
                None,
                |_| Ok(()),
                |_, boundary, keys, _| {
                    assert_eq!(boundary.source_cut, socket_cut.source_cut);
                    assert_eq!(keys.len(), 1);
                    Ok(keys.iter().filter(|key| key.key == "row/visible").count())
                },
            )
            .unwrap()
        else {
            panic!("missing key page");
        };
        assert_eq!(output, Some(1));
        assert_eq!(next_keys.identity, next_status.identity);
        assert_eq!(boundary.source_cut, socket_cut.source_cut);
        // Reading a candidate page never advances the caller's delivered cursor. A failed
        // send can retry the same cursors and receives the same authorized result.
        let ViewPage::Changes { output, .. } = fixture
            .bridge
            .page_with_rows(
                &open.boundary.keys,
                &open.boundary.status,
                1,
                None,
                |_| Ok(()),
                |_, _, keys, _| Ok(keys.len()),
            )
            .unwrap()
        else {
            panic!("retry lost the key");
        };
        assert_eq!(output, Some(1));
    }

    #[test]
    fn unauthorized_capture_and_page_never_run_row_callback() {
        let fixture = Fixture::new(32);
        let reads = AtomicUsize::new(0);
        assert!(
            fixture
                .bridge
                .subscribe_snapshot(
                    "fixture.rows",
                    |_| anyhow::bail!("forbidden"),
                    |_, _| {
                        reads.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                )
                .is_err()
        );
        let open = fixture
            .bridge
            .subscribe_snapshot("fixture.rows", |_| Ok(()), |_, _| Ok(()))
            .unwrap();
        fixture.append("row/private", json!({"id":"row/private"}));
        assert!(
            fixture
                .bridge
                .page_with_rows(
                    &open.boundary.keys,
                    &open.boundary.status,
                    1,
                    None,
                    |_| anyhow::bail!("forbidden"),
                    |_, _, _, _| {
                        reads.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                )
                .is_err()
        );
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ready_transition_refreshes_even_without_a_key_and_expiry_requires_resync() {
        let fixture = Fixture::new(2);
        fixture
            .store
            .connection
            .batched(|tx| {
                tx.execute("UPDATE ivm_views SET ready=0 WHERE name='fixture.rows'", [])?;
                Ok::<_, anyhow::Error>(())
            })
            .unwrap()
            .unwrap();
        let open = fixture
            .bridge
            .subscribe_snapshot("fixture.rows", |_| Ok(()), |_, _| Ok(()))
            .unwrap();
        assert!(open.snapshot.is_none());
        fixture
            .store
            .connection
            .batched(|tx| {
                tx.execute("UPDATE ivm_views SET ready=1 WHERE name='fixture.rows'", [])?;
                Ok::<_, anyhow::Error>(())
            })
            .unwrap()
            .unwrap();
        let ViewPage::Changes {
            output,
            has_availability_change,
            ..
        } = fixture
            .bridge
            .page_with_rows(
                &open.boundary.keys,
                &open.boundary.status,
                2,
                None,
                |_| Ok(()),
                |_, _, keys, availability| {
                    assert!(keys.is_empty());
                    assert!(!availability.is_empty());
                    Ok("refresh")
                },
            )
            .unwrap()
        else {
            panic!("availability lost");
        };
        assert_eq!(output, Some("refresh"));
        assert!(has_availability_change);
        fixture.append("row/a", json!({"id":"row/a"}));
        fixture.append("row/b", json!({"id":"row/b"}));
        fixture.append("row/c", json!({"id":"row/c"}));
        assert!(matches!(
            fixture
                .bridge
                .page_with_rows(
                    &open.boundary.keys,
                    &open.boundary.status,
                    1,
                    None,
                    |_| Ok(()),
                    |_, _, _, _| anyhow::bail!("gap must not read rows"),
                )
                .unwrap(),
            ViewPage::<()>::Resync {
                reason: events::Gap::Expired { .. },
                ..
            }
        ));
    }
    #[tokio::test]
    async fn lag_rechecks_journal_and_changed_continuation_skips_rows() {
        let fixture = Fixture::new(32);
        let open = fixture
            .bridge
            .subscribe_snapshot("fixture.rows", |_| Ok(()), |_, _| Ok(()))
            .unwrap();
        fixture.append("row/a", json!({"id":"row/a"}));
        fixture.append("row/b", json!({"id":"row/b"}));
        fixture.append("row/c", json!({"id":"row/c"}));
        let mut notices = open.notices;
        assert!(matches!(
            IvmViewBridge::<crate::store::Store>::next_notice(&mut notices).await,
            ViewWake::Lagged
        ));
        let ViewPage::Changes {
            next_keys,
            next_status,
            more,
            boundary,
            ..
        } = fixture
            .bridge
            .page_with_rows(
                &open.boundary.keys,
                &open.boundary.status,
                1,
                None,
                |_| Ok(()),
                |_, _, keys, _| Ok(keys.len()),
            )
            .unwrap()
        else {
            panic!("lag lost retained journal");
        };
        assert!(more);
        fixture.append("row/d", json!({"id":"row/d"}));
        let reads = AtomicUsize::new(0);
        let result = fixture
            .bridge
            .page_with_rows(
                &next_keys,
                &next_status,
                1,
                Some(&boundary.snapshot),
                |_| Ok(()),
                |_, _, _, _| {
                    reads.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .unwrap();
        assert!(matches!(result, ViewPage::SnapshotChanged { .. }));
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }
}
