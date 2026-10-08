//! Bounded agents adapter for an explicitly installed, completely certified namespace.
//! This factory attaches nothing and changes no provider map. The bridge authorizes before
//! either callback. Both callbacks authenticate the whole live producer boundary, including
//! silent advances; selected rows or a graph frontier can never establish source completeness.
use super::*;
use crate::api::delivery_presence::source::boundary as producer;
use crate::store::{
    agent_card_ivm as cards, agent_card_source,
    collection_ivm::{agent_source, scope},
};
use anyhow::ensure;
use rusqlite::{Connection, OptionalExtension as _};
use smallclaims::ivm::{
    SourceCut, Views,
    install::{Installer, Root},
};

/// The source owner must capture the namespace certificate with this exact complete manifest.
/// This binds input eligibility/schema and the compiled full-card dependency/output definition.
pub(in crate::api::client_v0) fn manifest() -> String {
    agent_source::boundary::manifest()
}

/// Uses the existing Store-held registry. The source owner supplies its actual Installer and
/// enables this adapter only after complete namespace publication and producer acknowledgement.
pub(in crate::api::client_v0) fn factory(
    store: Arc<Store>,
    views: Arc<Views>,
    installer: Arc<Installer>,
) -> anyhow::Result<Adapter> {
    ensure!(
        store
            .ivm_views()
            .as_ref()
            .is_some_and(|registered| Arc::ptr_eq(registered, &views)),
        "agent adapter registry belongs to another Store"
    );
    let expected = Arc::new(manifest());
    let expected_source = Arc::new(agent_source::capture_fingerprint_for(store.origin())?);
    let store = Arc::downgrade(&store);
    let coverage_views = views.clone();
    let coverage_installer = installer.clone();
    let coverage_manifest = expected.clone();
    let coverage_source = expected_source.clone();
    Ok(Adapter {
        view: cards::VIEW,
        coverage: Arc::new(move |connection| {
            let Some((root, cut, certificate)) = certified(
                connection,
                &coverage_views,
                &coverage_installer,
                &coverage_manifest,
                &coverage_source,
                client_now_ms(),
            )?
            else {
                return Ok(false);
            };
            // Mandatory on Silent: never infer producer closure from SQL counts, selected row
            // certificates, an unchanged semantic key page or an acknowledged old epoch.
            producer::read_boundary(&certificate, || {
                unchanged(
                    connection,
                    &coverage_views,
                    &coverage_installer,
                    &root,
                    &cut,
                )?;
                cards::coverage(connection, &root, &cut, client_now_ms())
            })
        }),
        rows: Arc::new(
            move |state, _, request, connection, boundary, _, retained| {
                (|| -> anyhow::Result<_> {
                    let store = store.upgrade().context("agent adapter Store has closed")?;
                    ensure!(
                        Arc::ptr_eq(&state.store, &store),
                        "agent adapter rows belong to another Store"
                    );
                    ensure!(
                        request.collection == "agents",
                        "agent adapter used for another collection"
                    );
                    let limit = request.limit.unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS);
                    ensure!(
                        (1..=cards::WINDOW_LIMIT).contains(&limit),
                        "invalid agent window limit"
                    );
                    let (root, cut, certificate) = certified(
                        connection,
                        &views,
                        &installer,
                        &expected,
                        &expected_source,
                        client_now_ms(),
                    )?
                    .context("complete agent source certificate is pending")?;
                    ensure!(
                        cut == boundary.source_cut,
                        "agent row and collection source cuts differ"
                    );
                    let current = events::capture(connection, &views, cards::VIEW)?;
                    ensure!(
                        current.identity == boundary.identity
                            && current.snapshot == boundary.snapshot,
                        "agent row and collection boundaries differ"
                    );
                    let frame_time = frame_time(connection, cut.projected)?;
                    // Select only indexed metadata before fetching public bodies. Native row
                    // evidence must belong to the complete namespace footprint; an empty native
                    // selection still authenticates that entire boundary, including silent changes.
                    let window = cards::ranked_window(
                        connection,
                        &root.namespace,
                        limit,
                        false,
                        request.status.as_deref(),
                    )?;
                    let ids: Vec<_> = window.keys.iter().map(|key| key.agent.clone()).collect();
                    let row_certificates = agent_card_source::selected_certificates(
                        connection,
                        &root.namespace,
                        &ids,
                    )?;
                    producer::read_boundary_rows(&certificate, &row_certificates, || {
                        unchanged(connection, &views, &installer, &root, &cut)?;
                        let rows = cards::current_rows(
                            connection,
                            &root,
                            limit,
                            request.status.as_deref(),
                            retained,
                            &frame_time,
                        )?;
                        ensure!(
                            rows.1 == window.has_more
                                && rows
                                    .0
                                    .iter()
                                    .map(|row| row["id"].as_str())
                                    .eq(ids.iter().map(|id| Some(id.as_str()))),
                            "agent selection changed during row read"
                        );
                        ensure!(
                            cards::coverage(connection, &root, &cut, client_now_ms())?,
                            "agent deadline expired during row read"
                        );
                        unchanged(connection, &views, &installer, &root, &cut)?;
                        Ok(rows)
                    })
                })()
                .map_err(ApiError::internal)
            },
        ),
    })
}

// Presentation time belongs to the same authorized SQL snapshot as the selected cards.
fn frame_time(connection: &Connection, projected: u64) -> anyhow::Result<String> {
    let at = if projected == 0 {
        0
    } else {
        connection
            .query_row(
                "SELECT accepted_at_unix_ms FROM claims WHERE store_index <= ?1 ORDER BY store_index DESC LIMIT 1",
                [projected],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or_default()
    };
    Ok(client_timestamp(at))
}

fn certified(
    connection: &Connection,
    views: &Views,
    installer: &Installer,
    expected_manifest: &str,
    expected_source: &str,
    now: u128,
) -> anyhow::Result<Option<(Root, SourceCut, producer::Certificate)>> {
    if !scope::readable(connection)? {
        return Ok(None);
    }
    if !source_bound(connection, installer, expected_source)? {
        return Ok(None);
    }
    let Some(cut) = smallclaims::ivm::source_cut(connection)? else {
        return Ok(None);
    };
    if cut.admitted != cut.projected
        || cut.admitted != smallclaims::store::current_index(connection)?
    {
        return Ok(None);
    }
    if !matches!(
        views.readiness(connection, cards::VIEW, cut.epoch)?,
        Readiness::Ready(_)
    ) {
        return Ok(None);
    }
    let root = views.installed_root(connection, installer, cards::VIEW)?;
    if !cards::coverage(connection, &root, &cut, now)? {
        return Ok(None);
    }
    let Some(certificate) =
        agent_source::boundary::read(connection, &root, &cut, expected_manifest)?
    else {
        return Ok(None);
    };
    Ok(Some((root, cut, certificate)))
}

fn source_bound(
    connection: &Connection,
    installer: &Installer,
    expected: &str,
) -> anyhow::Result<bool> {
    let captured = crate::store::collection_ivm::status(connection)?;
    if captured.fingerprint != expected {
        return Ok(false);
    }
    let installed = installer.position(connection, cards::SOURCE)?;
    Ok(installed.fingerprint == expected && installed.epoch == captured.epoch)
}

fn unchanged(
    connection: &Connection,
    views: &Views,
    installer: &Installer,
    root: &Root,
    cut: &SourceCut,
) -> anyhow::Result<()> {
    ensure!(scope::readable(connection)?, "agent capture scope changed");
    ensure!(
        smallclaims::ivm::source_cut(connection)? == Some(*cut)
            && cut.admitted == smallclaims::store::current_index(connection)?,
        "agent source cut changed"
    );
    let current = views.installed_root(connection, installer, cards::VIEW)?;
    ensure!(
        current.namespace == root.namespace
            && current.epoch == root.epoch
            && current.revision == root.revision
            && current.generation == root.generation
            && current.status_revision == root.status_revision,
        "agent installed root changed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::collection_ivm;

    #[test]
    fn frame_time_keeps_held_wal_snapshot_through_append_and_same_index_repair() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("time.sqlite"), "time-fixture").unwrap();
        let append = || {
            store
                .append_claim(&crate::model::ClaimInput {
                    subject: "agent/time-fixture".into(),
                    kind: "runtime.observed".into(),
                    actor: Some("agent/time-fixture".into()),
                    fields: serde_json::from_value(json!({"status":"running"})).unwrap(),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap()
        };
        let first = append();
        store
            .connection
            .batched(|tx| {
                tx.execute(
                    "UPDATE claims SET accepted_at_unix_ms='1000' WHERE store_index=?1",
                    [first.store_index],
                )
            })
            .unwrap()
            .unwrap();
        let held = store.readers.get();
        held.execute_batch("BEGIN DEFERRED").unwrap();
        assert_eq!(
            frame_time(&held, first.store_index).unwrap(),
            client_timestamp(1000)
        );
        assert_eq!(frame_time(&held, 0).unwrap(), client_timestamp(0));
        let later = append();
        assert!(later.store_index > first.store_index);
        assert_eq!(
            frame_time(&held, first.store_index).unwrap(),
            client_timestamp(1000)
        );
        // This manually held transaction is not the bridge's pinned read_snapshot callback.
        // Its separate pool read demonstrates the isolation provided by the explicit Connection.
        store
            .connection
            .batched(|tx| {
                tx.execute(
                    "UPDATE claims SET accepted_at_unix_ms='9000' WHERE store_index=?1",
                    [first.store_index],
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(store.projection_time_at(first.store_index).unwrap(), 9000);
        assert_eq!(
            frame_time(&held, first.store_index).unwrap(),
            client_timestamp(1000)
        );
        held.execute_batch("COMMIT").unwrap();
        assert_eq!(
            frame_time(&held, first.store_index).unwrap(),
            client_timestamp(9000)
        );
        store
            .read_snapshot(|index| {
                let connection = store.readers.get();
                assert_eq!(store.projection_time_at(first.store_index)?, 9000);
                assert_eq!(
                    frame_time(&connection, first.store_index)?,
                    client_timestamp(9000)
                );
                std::thread::scope(|writer| {
                    writer
                        .spawn(|| {
                            store
                                .connection
                                .batched(|tx| {
                                    tx.execute(
                            "UPDATE claims SET accepted_at_unix_ms='12000' WHERE store_index=?1",
                            [first.store_index],
                        )
                                })
                                .unwrap()
                                .unwrap();
                        })
                        .join()
                        .unwrap();
                });
                // The old implementation is already correct on the real pinned bridge path.
                assert_eq!(store.projection_time_at(first.store_index)?, 9000);
                assert_eq!(
                    frame_time(&connection, first.store_index)?,
                    client_timestamp(9000)
                );
                assert_eq!(store.index()?, index);
                Ok(())
            })
            .unwrap();
        assert_eq!(store.projection_time_at(first.store_index).unwrap(), 12000);
    }

    struct Registered {
        root: tempfile::TempDir,
        store: Arc<Store>,
        views: Arc<Views>,
        installer: Arc<Installer>,
    }
    fn registered() -> Registered {
        registered_for(Some("card-fixture"))
    }
    fn registered_for(receiver: Option<&str>) -> Registered {
        let root = tempfile::tempdir().unwrap();
        // Exactly one actual card registry per disposable Store; no fabricated View or
        // Operator, namespace token, source cut, row or complete certificate is installed.
        let views = Arc::new(Views::new(cards::definitions()).unwrap());
        let store = Arc::new(
            Store::open_with_ivm_views(
                &root.path().join("card.sqlite"),
                "card-fixture",
                views.clone(),
            )
            .unwrap(),
        );
        let installer = Arc::new(Installer::new(vec![]).unwrap());
        store
            .connection
            .batched(|tx| {
                installer.create_schema(tx)?;
                match receiver {
                    Some(receiver) => agent_source::install_capture_for(tx, receiver, 1)?,
                    None => agent_source::install_capture(tx, 1)?,
                };
                Ok::<_, anyhow::Error>(())
            })
            .unwrap()
            .unwrap();
        let finishing = (views.clone(), installer.clone());
        store
            .install_transaction_hooks(scope::prepare, move |tx| {
                scope::finalize(tx, &finishing.0, &finishing.1, cards::SOURCE, |_, _| {
                    Ok(scope::Coverage::Complete)
                })?;
                Ok(())
            })
            .unwrap();
        store
            .connection
            .batched(|tx| {
                let fingerprint = match receiver {
                    Some(receiver) => agent_source::capture_fingerprint_for(receiver)?,
                    None => agent_source::capture_fingerprint(),
                };
                scope::begin_install(tx, &views, &installer, cards::SOURCE, &fingerprint, 1)
            })
            .unwrap()
            .unwrap();
        store
            .connection
            .batched(|tx| agent_source::clock::tick(tx, 0, agent_source::clock::Reason::Kernel))
            .unwrap()
            .unwrap();
        Registered {
            root,
            store,
            views,
            installer,
        }
    }
    fn state(f: &Registered) -> AppState {
        AppState {
            store: f.store.clone(),
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: tokio::sync::watch::channel(0).0,
            node: "card-fixture".into(),
            state_dir: f.root.path().into(),
            pty_root: f.root.path().join("pty"),
            pty_binary: "pty".into(),
            fleet_id: None,
            configured_peers: vec![],
            client_relay: None,
            native_session_home: None,
            planner_default: Default::default(),
        }
    }

    #[test]
    fn silent_coverage_refuses_unbound_other_origin_and_mismatched_installer_source() {
        let expected = agent_source::capture_fingerprint_for("card-fixture").unwrap();
        let bound = registered();
        assert!(scope::readable(&bound.store.readers.get()).unwrap());
        assert!(source_bound(&bound.store.readers.get(), &bound.installer, &expected).unwrap());
        assert!(
            !source_bound(
                &bound.store.readers.get(),
                &bound.installer,
                &agent_source::capture_fingerprint_for("other-origin").unwrap()
            )
            .unwrap()
        );
        let unbound = registered_for(None);
        let foreign = registered_for(Some("other-origin"));
        for fixture in [&unbound, &foreign] {
            let connection = fixture.store.readers.get();
            assert!(scope::readable(&connection).unwrap());
            assert!(!source_bound(&connection, &fixture.installer, &expected).unwrap());
            let adapter = factory(
                fixture.store.clone(),
                fixture.views.clone(),
                fixture.installer.clone(),
            )
            .unwrap();
            // Coverage is called independently of selected rows, including Silent advances.
            assert!(!(adapter.coverage)(&connection).unwrap());
        }
        bound
            .store
            .connection
            .batched(|tx| {
                tx.execute(
                    "UPDATE ivm_install_sources SET fingerprint=?1 WHERE name=?2",
                    rusqlite::params![
                        agent_source::capture_fingerprint_for("other-origin").unwrap(),
                        cards::SOURCE
                    ],
                )
            })
            .unwrap()
            .unwrap();
        let connection = bound.store.readers.get();
        assert!(scope::readable(&connection).unwrap());
        assert_eq!(
            collection_ivm::status(&connection).unwrap().fingerprint,
            expected
        );
        assert!(!source_bound(&connection, &bound.installer, &expected).unwrap());
        let adapter = factory(
            bound.store.clone(),
            bound.views.clone(),
            bound.installer.clone(),
        )
        .unwrap();
        assert!(!(adapter.coverage)(&connection).unwrap());
    }
    fn request() -> CollectionSubscribe {
        serde_json::from_value(
            json!({"kind":"subscribe","id":"cards","collection":"agents","limit":2}),
        )
        .unwrap()
    }
    async fn pending_read(
        f: &Registered,
        sources: Arc<Sources>,
        adapter: Arc<Adapter>,
        held: Held,
    ) -> Candidate {
        let permit = Arc::new(tokio::sync::Semaphore::new(1))
            .acquire_owned()
            .await
            .unwrap();
        read(
            state(f),
            ClientSession::local(Some("person/avery")).unwrap(),
            request(),
            permit,
            sources,
            adapter,
            held,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn registered_capture_without_operator_certificate_never_returns_rows_on_initial_or_reconnect()
     {
        let f = registered();
        assert!(scope::readable(&f.store.readers.get()).unwrap());
        let adapter =
            Arc::new(factory(f.store.clone(), f.views.clone(), f.installer.clone()).unwrap());
        assert!(!(adapter.coverage)(&f.store.readers.get()).unwrap());
        let sources = Sources::from_store(
            f.store.clone(),
            BTreeMap::from([("agents".into(), adapter.clone())]),
        )
        .unwrap()
        .unwrap();
        assert!(Arc::ptr_eq(
            &f.store.ivm_publisher().unwrap().unwrap(),
            &f.store.ivm_publisher().unwrap().unwrap()
        ));
        let first = pending_read(&f, sources.clone(), adapter.clone(), Held::default()).await;
        assert!(matches!(first.output, Output::Unavailable));
        let next = pending_read(
            &f,
            sources.clone(),
            adapter.clone(),
            Held {
                cursor: first.delivered,
                rows: Arc::new(BTreeMap::new()),
            },
        )
        .await;
        assert!(matches!(next.output, Output::Silent));
        let reconnect = pending_read(&f, sources, adapter, Held::default()).await;
        assert!(matches!(reconnect.output, Output::Unavailable));
        assert!(
            f.installer
                .root(&f.store.readers.get(), cards::VIEW)
                .is_err()
        );
    }

    #[test]
    fn raw_same_index_source_mutation_and_later_managed_commit_do_not_restore_coverage() {
        let f = registered();
        let adapter = factory(f.store.clone(), f.views.clone(), f.installer.clone()).unwrap();
        let index = f.store.index().unwrap();
        f.store
            .connection
            .write()
            .execute(
                "UPDATE local_agent_card_clock SET at_ms='1',revision=revision+1 WHERE singleton=1",
                [],
            )
            .unwrap();
        assert_eq!(f.store.index().unwrap(), index);
        assert!(
            collection_ivm::status(&f.store.readers.get())
                .unwrap()
                .gap
                .is_some()
        );
        assert!(!(adapter.coverage)(&f.store.readers.get()).unwrap());
        f.store
            .connection
            .batched(|tx| {
                tx.execute(
                    "INSERT INTO meta(key,value) VALUES('fixture-after-gap','1')",
                    [],
                )
            })
            .unwrap()
            .unwrap();
        assert!(!scope::readable(&f.store.readers.get()).unwrap());
        assert!(!(adapter.coverage)(&f.store.readers.get()).unwrap());
    }

    #[test]
    fn installed_prepare_rollback_preserves_cut_capture_and_pending_rows() {
        let f = registered();
        let before = smallclaims::ivm::source_cut(&f.store.readers.get()).unwrap();
        let result=f.store.connection.batched(|tx|->anyhow::Result<()> {
            tx.execute("UPDATE local_agent_card_clock SET at_ms='10',revision=revision+1 WHERE singleton=1",[])?;
            assert!(!collection_ivm::clean(tx)?);
            anyhow::bail!("rollback captured source before finalization")
        }).unwrap();
        assert!(result.is_err());
        assert_eq!(
            smallclaims::ivm::source_cut(&f.store.readers.get()).unwrap(),
            before
        );
        assert!(scope::readable(&f.store.readers.get()).unwrap());
        assert!(collection_ivm::clean(&f.store.readers.get()).unwrap());
        let adapter = factory(f.store.clone(), f.views.clone(), f.installer.clone()).unwrap();
        assert!(!(adapter.coverage)(&f.store.readers.get()).unwrap());
    }

    #[test]
    fn factory_rejects_other_store_registry_and_does_not_keep_store_alive() {
        let first = registered();
        let other = registered();
        assert!(
            factory(
                first.store.clone(),
                other.views.clone(),
                other.installer.clone()
            )
            .is_err()
        );
        let weak = Arc::downgrade(&first.store);
        let adapter = factory(
            first.store.clone(),
            first.views.clone(),
            first.installer.clone(),
        )
        .unwrap();
        drop(first);
        assert!(
            weak.upgrade().is_none(),
            "factory must not create a retained Store cycle"
        );
        drop(adapter);
    }
}

#[cfg(test)]
#[path = "agents/kernel_parity.rs"]
mod kernel_parity;

#[cfg(test)]
#[path = "agents/provider_parity.rs"]
mod provider_parity;
