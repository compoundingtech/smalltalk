use super::*;

fn fixture() -> (tempfile::TempDir, AppState) {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    state.store = Arc::new(roster_followup_store());
    (root, state)
}

fn unrelated(store: &Store, key: &str) {
    store
        .append_claim(&ClaimInput {
            subject: format!("custom/fresh-proof/{key}"),
            kind: "custom.test.marker".into(),
            actor: None,
            fields: BTreeMap::new(),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        })
        .unwrap();
}

fn update_card(store: &Store) -> ClaimRecord {
    store
        .append_claim(&roster_local_observation(
            "runtime.observed",
            json!({"status":"running", "runtime_id":"fresh-new", "incarnation_id":"amber-1"}),
        ))
        .unwrap()
}

#[tokio::test]
async fn unrelated_claims_verify_the_exact_older_response_and_later_wait_target() {
    for history in [false, true] {
        let (_root, state) = fixture();
        let store = &state.store;
        let _wake = store.start_agent_roster_refresher().unwrap();
        refresh_agent_roster(store, history).unwrap();
        let query = ClientListQuery {
            history,
            fresh: true,
            ..ClientListQuery::default()
        };
        let (cut, cards, published_at) = store
            .published_agent_roster(store.index().unwrap(), history)
            .unwrap();
        let (Extension(before_snapshot), Json(before)) =
            client_agents_published_page(&state, &query, cut, None)
                .unwrap()
                .unwrap();
        unrelated(store, "request");
        let requested = store.index().unwrap();
        unrelated(store, "wait-start");
        let wanted = store.index().unwrap();
        assert!(cut < requested && requested < wanted);
        let refolds = store.agent_resources_refolded_cards_for_test();
        let writes = smallclaims::touched::writes();
        let waited = wait_for_agent_roster(store, history).await;
        assert_eq!(waited.wanted, Some(wanted));
        assert_eq!(waited.outcome, RosterFreshWaitOutcome::Unchanged);
        assert_eq!(
            roster_fresh_served_proof(store, history, waited, Some((cut, published_at, &cards))),
            RosterUnchangedProof::Verified
        );
        let (Extension(snapshot), Json(page)) =
            client_agents_published_page(&state, &query, requested, Some(waited))
                .unwrap()
                .unwrap();
        assert_eq!(snapshot.store_index, before_snapshot.store_index);
        assert_eq!(snapshot.store_index, cut);
        assert_eq!(snapshot.published_at, before_snapshot.published_at);
        assert_eq!(page.items, before.items);
        assert_eq!(
            store.agent_resources_refolded_cards_for_test(),
            refolds,
            "fresh reads never fold"
        );
        assert_eq!(
            smallclaims::touched::writes(),
            writes,
            "proof/page never write"
        );
    }
}

#[tokio::test]
async fn a_card_write_requires_a_new_publication_with_the_new_card() {
    for history in [false, true] {
        let (_root, state) = fixture();
        let store = &state.store;
        let _wake = store.start_agent_roster_refresher().unwrap();
        refresh_agent_roster(store, history).unwrap();
        let old = store
            .published_agent_roster(store.index().unwrap(), history)
            .unwrap();
        let changed = update_card(store);
        let requested = store.index().unwrap();
        assert!(changed.store_index > old.0);
        assert!(
            !store
                .served_agent_roster_unchanged_through(requested, history, old.0, old.2, &old.1)
                .unwrap()
        );
        let reader = Arc::clone(store);
        let waiter = tokio::spawn(async move { wait_for_agent_roster(&reader, history).await });
        store.fresh_agent_roster_wanted().await;
        assert!(
            !waiter.is_finished(),
            "changed cards cannot finish as unchanged"
        );
        refresh_agent_roster(store, history).unwrap();
        let waited = waiter.await.unwrap();
        // A publication racing the unchanged probe can satisfy that probe at cut==wanted.
        assert!(matches!(
            waited.outcome,
            RosterFreshWaitOutcome::Published | RosterFreshWaitOutcome::Unchanged
        ));
        let query = ClientListQuery {
            history,
            fresh: true,
            ..ClientListQuery::default()
        };
        let (Extension(snapshot), Json(page)) =
            client_agents_published_page(&state, &query, requested, Some(waited))
                .unwrap()
                .unwrap();
        assert!(snapshot.store_index >= changed.store_index);
        let card = page
            .items
            .iter()
            .find(|card| card["id"] == "agent/node.amber")
            .unwrap();
        assert_eq!(card["runtime_ids"], json!(["runtime/fresh-new"]));
    }
}

#[tokio::test]
async fn failed_refresh_remains_distinct_from_unchanged_and_keeps_the_old_response() {
    for history in [false, true] {
        let (_root, state) = fixture();
        let store = &state.store;
        let _wake = store.start_agent_roster_refresher().unwrap();
        refresh_agent_roster(store, history).unwrap();
        let query = ClientListQuery {
            history,
            fresh: true,
            ..ClientListQuery::default()
        };
        let old = store
            .published_agent_roster(store.index().unwrap(), history)
            .unwrap();
        let (Extension(before_snapshot), Json(before)) =
            client_agents_published_page(&state, &query, old.0, None)
                .unwrap()
                .unwrap();
        update_card(store);
        assert!(
            store
                .answer_agent_roster_requests::<()>(|| anyhow::bail!("private refresh failure"))
                .is_err()
        );
        let waited = wait_for_agent_roster(store, history).await;
        assert_eq!(waited.outcome, RosterFreshWaitOutcome::RefreshFailed);
        assert_ne!(
            roster_fresh_served_proof(store, history, waited, Some((old.0, old.2, &old.1))),
            RosterUnchangedProof::Verified
        );
        let (Extension(snapshot), Json(page)) =
            client_agents_published_page(&state, &query, store.index().unwrap(), Some(waited))
                .unwrap()
                .unwrap();
        assert_eq!(snapshot.store_index, before_snapshot.store_index);
        assert_eq!(snapshot.published_at, before_snapshot.published_at);
        assert_eq!(
            page.items, before.items,
            "observing failure must not change fallback policy"
        );
    }
}

#[tokio::test]
async fn closed_fixture_publication_channel_is_a_disconnection_not_unchanged() {
    let (_root, state) = fixture();
    let store = &state.store;
    let _wake = store.start_agent_roster_refresher().unwrap();
    refresh_agent_roster(store, true).unwrap();
    update_card(store);
    let (sender, receiver) = tokio::sync::watch::channel(0);
    drop(sender);
    let result =
        wait_for_agent_roster_publications(store, true, store.index().unwrap(), receiver).await;
    assert_eq!(result, RosterFreshWaitOutcome::Disconnected);
    // The closed receiver is a private branch control, not a production Store lifetime claim.
}

#[tokio::test(start_paused = true)]
async fn unsatisfied_fresh_wait_times_out_at_the_unchanged_virtual_deadline() {
    let (_root, state) = fixture();
    let store = &state.store;
    let _wake = store.start_agent_roster_refresher().unwrap();
    refresh_agent_roster(store, true).unwrap();
    let old = store
        .published_agent_roster(store.index().unwrap(), true)
        .unwrap();
    update_card(store);
    let started = tokio::time::Instant::now();
    let waited = wait_for_agent_roster(store, true).await;
    assert_eq!(waited.outcome, RosterFreshWaitOutcome::Timeout);
    assert_eq!(started.elapsed(), AGENT_ROSTER_READ_WAIT);
    assert_ne!(
        roster_fresh_served_proof(store, true, waited, Some((old.0, old.2, &old.1))),
        RosterUnchangedProof::Verified
    );
}

// Capture every attribute write rather than an SDK's final deduplicated map: two calls
// that write the same value must fail the exactly-once assertion too. No global tracer.
struct AttributeWrites {
    context: opentelemetry::trace::SpanContext,
    values: Arc<std::sync::Mutex<Vec<opentelemetry::KeyValue>>>,
}

impl opentelemetry::trace::Span for AttributeWrites {
    fn add_event_with_timestamp<T: Into<std::borrow::Cow<'static, str>>>(
        &mut self,
        _: T,
        _: std::time::SystemTime,
        _: Vec<opentelemetry::KeyValue>,
    ) {
    }
    fn span_context(&self) -> &opentelemetry::trace::SpanContext {
        &self.context
    }
    fn is_recording(&self) -> bool {
        true
    }
    fn set_attribute(&mut self, attribute: opentelemetry::KeyValue) {
        self.values.lock().unwrap().push(attribute);
    }
    fn set_status(&mut self, _: opentelemetry::trace::Status) {}
    fn update_name<T: Into<std::borrow::Cow<'static, str>>>(&mut self, _: T) {}
    fn add_link(&mut self, _: opentelemetry::trace::SpanContext, _: Vec<opentelemetry::KeyValue>) {}
    fn end_with_timestamp(&mut self, _: std::time::SystemTime) {}
}

fn with_attribute_writes<T>(work: impl FnOnce() -> T) -> (T, Vec<opentelemetry::KeyValue>) {
    use opentelemetry::trace::TraceContextExt as _;
    let values = Arc::new(std::sync::Mutex::new(Vec::new()));
    let context = opentelemetry::Context::new().with_span(AttributeWrites {
        context: opentelemetry::trace::SpanContext::empty_context(),
        values: Arc::clone(&values),
    });
    let _context = context.attach();
    let _export = crate::otel::fixture_export_scope();
    let result = crate::otel::roster_scope(|| {
        assert!(crate::otel::roster_recording());
        work()
    });
    let attributes = values.lock().unwrap().clone();
    (result, attributes)
}

#[test]
fn fresh_wait_attributes_are_recorded_once_on_each_page_return_path() {
    // Exercise real page selection/rendering. Missing, filtered/history not-ready,
    // partial head, full and filtered complete, plus an error during page rendering.
    for path in [
        "missing_head",
        "history_not_ready",
        "filtered_not_ready",
        "head",
        "complete",
        "filtered_complete",
        "page_error",
    ] {
        let (_root, state) = fixture();
        let store = &state.store;
        let _wake = store.start_agent_roster_refresher().unwrap();
        let mut query = ClientListQuery {
            fresh: true,
            ..Default::default()
        };
        if path == "history_not_ready" {
            query.history = true;
        }
        if matches!(path, "filtered_not_ready" | "filtered_complete") {
            query.status = Some("running".into());
        }
        if path == "head" {
            store
                .read_snapshot(|index| client_agent_roster_head(store, index))
                .unwrap();
            assert!(
                store
                    .published_agent_roster(store.index().unwrap(), false)
                    .is_none()
            );
            assert!(
                store
                    .published_agent_roster_head(store.index().unwrap(), 1)
                    .is_some()
            );
        } else if matches!(path, "complete" | "filtered_complete" | "page_error") {
            refresh_agent_roster(store, false).unwrap();
        }
        if path == "page_error" {
            // Direct helper branch fixture: real continuations use their separate handler.
            query.cursor = Some("invalid-private-cursor".into());
        }
        let cut = store.index().unwrap();
        let waited = RosterFreshWait {
            wanted: Some(cut),
            outcome: RosterFreshWaitOutcome::Unchanged,
        };
        for fresh in [true, false] {
            query.fresh = fresh;
            let (result, attributes) = with_attribute_writes(|| {
                client_agents_published_page(&state, &query, cut, fresh.then_some(waited))
            });
            if path == "page_error" {
                assert!(result.is_err());
            } else {
                assert_eq!(
                    result.unwrap().is_some(),
                    matches!(path, "head" | "complete" | "filtered_complete"),
                    "{path}"
                );
            }
            if fresh {
                let expected = if matches!(path, "complete" | "filtered_complete") {
                    "verified"
                } else {
                    "refused"
                };
                let proof = attributes
                    .iter()
                    .find(|attribute| attribute.key.as_str() == "st.roster.fresh_proof")
                    .unwrap();
                assert_eq!(proof.value.as_str(), expected, "{path}");
            }
            for key in [
                "st.roster.fresh_wait",
                "st.roster.fresh_wait_cut",
                "st.roster.fresh_proof",
                "st.roster.fresh_unchanged_verified",
            ] {
                assert_eq!(
                    attributes
                        .iter()
                        .filter(|attribute| attribute.key.as_str() == key)
                        .count(),
                    usize::from(fresh),
                    "{path}: {key}: {attributes:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn traced_unchanged_proof_reports_work_for_each_churn_interval() {
    for history in [false, true] {
        for churn in [0, 64, 1024] {
            let (_root, state) = fixture();
            let store = &state.store;
            let _wake = store.start_agent_roster_refresher().unwrap();
            refresh_agent_roster(store, history).unwrap();
            let (cut, cards, published_at) = store
                .published_agent_roster(store.index().unwrap(), history)
                .unwrap();
            for index in 0..churn {
                unrelated(store, &format!("cost-{index}"));
            }
            let through = store.index().unwrap();
            assert_eq!(through - cut, churn);
            let waited = wait_for_agent_roster(store, history).await;
            // At zero churn the ordinary numeric publication already satisfies the wait.
            assert_eq!(
                waited.outcome,
                if churn == 0 {
                    RosterFreshWaitOutcome::Published
                } else {
                    RosterFreshWaitOutcome::Unchanged
                }
            );
            let writes = smallclaims::touched::writes();
            let refolds = store.agent_resources_refolded_cards_for_test();
            let proof_scope = smallclaims::sqlite::work::SqliteWorkScope::start();
            let started = Instant::now();
            let verified = store
                .served_agent_roster_unchanged_through(through, history, cut, published_at, &cards)
                .unwrap();
            let proof_elapsed = started.elapsed();
            let proof_work = proof_scope.finish();
            assert!(verified);
            let query = ClientListQuery {
                history,
                fresh: true,
                ..Default::default()
            };
            let page_scope = smallclaims::sqlite::work::SqliteWorkScope::start();
            let started = Instant::now();
            let (page, attributes) = with_attribute_writes(|| {
                client_agents_published_page(&state, &query, through, Some(waited))
            });
            let page_elapsed = started.elapsed();
            let page_work = page_scope.finish();
            assert_eq!(page.unwrap().unwrap().0.0.store_index, cut);
            let proof = attributes
                .iter()
                .find(|attribute| attribute.key.as_str() == "st.roster.fresh_proof")
                .unwrap();
            assert_eq!(
                proof.value.as_str(),
                if churn == 0 {
                    "not_checked"
                } else {
                    "verified"
                }
            );
            assert_eq!(smallclaims::touched::writes(), writes);
            assert_eq!(store.agent_resources_refolded_cards_for_test(), refolds);
            eprintln!(
                "fresh-proof-cost history={history} claims={churn} cut={cut} through={through} isolated_proof={proof_work:?} proof_wall_us={} traced_page={page_work:?} page_wall_us={}",
                proof_elapsed.as_micros(),
                page_elapsed.as_micros()
            );
        }
    }
}
