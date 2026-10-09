//! The attention list's refresher. One task folds every person's attention rows off the request
//! path and publishes them; collection windows serve the newest publication under its own cut
//! and never fold attention while it runs. See `store::attention_list`.

use super::*;
use crate::store::attention_list::{AttentionDelta, AttentionPublication};
use crate::store::owner_lists::OwnerView;

/// The shortest pause between two refreshes. A refresh also pauses as long as it took, so the
/// refresher never takes more than about half a core. Windows never wait for it.
const ATTENTION_LIST_REFRESH_PAUSE: Duration = Duration::from_secs(1);

/// How often the rows are evaluated again with no claim: eligibility and grace periods read the
/// clock. The same period collection windows used to reread attention on.
const ATTENTION_LIST_CLOCK: Duration = Duration::from_secs(30);

/// How long after a window last read attention or a summary the refresher keeps folding
/// attention for new commits. Their windows reread at least every clock period while they are
/// open, so this passes only when none is. A window that opens later gets the newest list at
/// once and wakes the refresher.
const ATTENTION_LIST_IDLE: Duration = Duration::from_secs(600);

/// Keep attention, every person's glasses and arrangements, and the summary rows windows read
/// published off the request path. As the daemon starts it folds them once; after that it
/// refreshes, at most once a second, when a commit lands, a roster is published, the clock
/// passes attention's period, or a window asks. A refresh folds attention again only when
/// something attention reads changed (see `Store::attention_list_delta`) and reads again only
/// the people whose glasses or arrangements changed; otherwise each view keeps its rows.
pub fn start_attention_list(state: &AppState) {
    // An escape hatch while the published list is new: windows then fold attention as before.
    if std::env::var_os("ST3_ATTENTION_LIST").is_some_and(|value| value == "off") {
        return;
    }
    let Some(wake) = state.store.start_attention_list_refresher() else {
        return;
    };
    let store = state.store.clone();
    let state = state.clone();
    let mut changed = state.event_notify.subscribe();
    let mut roster = state.store.subscribe_agent_roster();
    tokio::spawn(async move {
        // However this task ends, its views stop being served: windows read for themselves.
        struct Stopped(Arc<Store>);
        impl Drop for Stopped {
            fn drop(&mut self) {
                self.0.stop_attention_list_refresher();
            }
        }
        let _stopped = Stopped(store.clone());
        let idle = ATTENTION_LIST_IDLE.as_millis() as u64;
        loop {
            let started = tokio::time::Instant::now();
            let reader = store.clone();
            let summary_state = state.clone();
            // Attention folds only while windows read it; the first fold always runs.
            let attention = store.attention_list_read_within(idle)
                || store.newest_attention_list().1.is_none();
            // One view at a time: attention, every person's glasses and arrangements, then the
            // summary rows windows read. One view's failure leaves the others refreshing.
            let refreshed = tokio::task::spawn_blocking(move || {
                // A pairing completed or revoked since the last pass changes which windows a
                // session may hold; their windows reread to recheck it, as before.
                if let Err(error) = reader.recheck_pairings() {
                    eprintln!("st3: pairing recheck for published views failed: {error:#}");
                }
                [
                    ("attention", crate::performance::task("attention/refresh", || {
                        if attention { refresh_attention_list(&reader) } else { Ok(()) }
                    })),
                    ("glasses", crate::performance::task("person-views/refresh", || {
                        refresh_owner_list(&reader, OwnerView::Glasses)
                    })),
                    ("arrangements", crate::performance::task("person-views/refresh", || {
                        refresh_owner_list(&reader, OwnerView::Arrangements)
                    })),
                    ("summary", crate::performance::task("summary/refresh", || {
                        super::client_v0::summary::refresh_published(
                            &summary_state,
                            ATTENTION_LIST_IDLE.as_millis() as u64,
                        )
                        .map(|_| ())
                    })),
                ]
            })
            .await;
            match refreshed {
                Ok(results) => {
                    for (view, result) in results {
                        if let Err(error) = &result {
                            eprintln!("st3: {view} view refresh failed: {error:#}");
                        }
                        store.note_view_refreshed(view, result.is_ok());
                    }
                }
                Err(error) => {
                    eprintln!("st3: published view refresh stopped: {error}");
                    for view in crate::store::attention_list::PUBLISHED_VIEWS {
                        store.note_view_refreshed(view, false);
                    }
                }
            }
            // A summary selection a window is waiting for is computed at once, without the
            // pause; it is computed only once its selection is registered.
            if store.summary_selection_waiting() {
                let summary_state = state.clone();
                let refreshed = tokio::task::spawn_blocking(move || {
                    crate::performance::task("summary/refresh", || {
                        super::client_v0::summary::refresh_published(
                            &summary_state,
                            ATTENTION_LIST_IDLE.as_millis() as u64,
                        )
                    })
                })
                .await;
                if let Ok(result) = &refreshed
                    && let Err(error) = result
                {
                    eprintln!("st3: summary view refresh failed: {error:#}");
                }
                store.note_view_refreshed("summary", matches!(refreshed, Ok(Ok(_))));
            }
            tokio::time::sleep(started.elapsed().max(ATTENTION_LIST_REFRESH_PAUSE)).await;
            // Attention is evaluated again when its clock period ends or a claim it read
            // becomes eligible, counted from when it was evaluated, not from this wait.
            let due = store
                .attention_list_due_at(ATTENTION_LIST_CLOCK.as_millis())
                .map_or(ATTENTION_LIST_CLOCK, |due| {
                    Duration::from_millis(due.saturating_sub(client_now_ms()) as u64)
                });
            // A glasses or arrangements window rereads only when its view publishes, so every
            // commit is weighed; with nothing changed that is a few indexed reads.
            tokio::select! {
                () = wake.notified() => {}
                result = changed.changed() => {
                    if result.is_err() {
                        return;
                    }
                }
                // The summary counts working agents from the published roster.
                result = roster.changed() => {
                    if result.is_err() {
                        return;
                    }
                }
                () = tokio::time::sleep(due) => {}
            }
        }
    });
}

/// Publish the list at the current cut in one short snapshot: the same rows when nothing
/// attention reads changed since the newest publication, else a fold of every person's rows.
/// A fold that comes out equal keeps the published rows, so windows have nothing to reread.
/// The clock is read inside the snapshot: every local claim in it was accepted before then.
pub(crate) fn refresh_attention_list(store: &Store) -> anyhow::Result<()> {
    let (generation, previous) = store.newest_attention_list();
    let clock = ATTENTION_LIST_CLOCK.as_millis();
    store.read_snapshot(|index| {
        let now = client_now_ms();
        let (cause, due) = match &previous {
            None => (Some("first".to_owned()), None),
            Some(previous)
                if now < previous.evaluated_at_unix_ms
                    || now - previous.evaluated_at_unix_ms >= clock =>
            {
                (Some("clock".to_owned()), None)
            }
            Some(previous) if previous.due_at_unix_ms.is_some_and(|due| now >= due) => {
                (Some("due".to_owned()), None)
            }
            Some(previous) => match store.attention_list_delta(previous, index, now)? {
                AttentionDelta::Unchanged(None) if previous.cut == index => return Ok(()),
                AttentionDelta::Unchanged(due) => (None, due),
                AttentionDelta::Refold(cause) => (Some(cause), None),
            },
        };
        let projection = store.attention_projection_frontier()?;
        let (rows, evaluated_at_unix_ms, due_at_unix_ms, future_after) = match (cause, &previous) {
            (None, Some(previous)) => {
                // Still pending future acceptance times stand beside the new ones.
                let due = [previous.due_at_unix_ms, due].into_iter().flatten().min();
                let future_after = due.and(previous.future_after.or(Some(previous.cut)));
                (Arc::clone(&previous.rows), previous.evaluated_at_unix_ms, due, future_after)
            }
            (cause, _) => {
                store.note_attention_list_fold(cause.as_deref().unwrap_or("first"));
                let rows = client_attention_resources_at(store, None, false, now)?;
                let rows = match &previous {
                    Some(previous) if *previous.rows == rows => Arc::clone(&previous.rows),
                    _ => Arc::new(rows),
                };
                // Claims dated after `now` were folded as not yet eligible: those since the
                // previous cut, and earlier ones still pending. Evaluate again when the first
                // of them is.
                let start = previous
                    .as_ref()
                    .map(|previous| previous.future_after.unwrap_or(previous.cut).min(previous.cut));
                let due = match start {
                    Some(start) if start < index => store.attention_future_since(start, index, now)?,
                    _ => None,
                };
                (rows, now, due, due.and(start))
            }
        };
        let changed = previous
            .as_ref()
            .is_none_or(|previous| !Arc::ptr_eq(&previous.rows, &rows));
        store.publish_attention_list(
            generation,
            AttentionPublication {
                cut: index,
                projection,
                evaluated_at_unix_ms,
                due_at_unix_ms,
                future_after,
                published_at_unix_ms: client_now_ms(),
                rows,
            },
            changed,
        );
        Ok(())
    })
}

/// Publish every person's rows of `view` (glasses or arrangements) at the current cut in one
/// short snapshot, reading again only the people whose rows may have changed.
pub(crate) fn refresh_owner_list(store: &Store, view: OwnerView) -> anyhow::Result<()> {
    if store.refresh_owner_list(view, client_now_ms())? {
        store.publish_collection_view(view.collection());
    }
    Ok(())
}

/// A window's rows of a person-owned view (glasses, arrangements): `person`'s rows in the newest
/// publication, with its cut and publication time. `None` while a refresher runs but has
/// published nothing yet; the refresher is woken either way when the publication is older than
/// the window's snapshot at `index`.
pub(crate) fn published_owner_rows(
    store: &Store,
    view: OwnerView,
    index: u64,
    person: &str,
) -> Option<(u64, u128, Vec<Value>)> {
    let Some(publication) = store.published_owner_list(view) else {
        store.request_attention_list_refresh();
        return None;
    };
    if publication.cut < index {
        store.request_attention_list_refresh();
    }
    let rows = publication
        .owners
        .get(person)
        .map(|rows| (**rows).clone())
        .unwrap_or_default();
    Some((publication.cut, publication.published_at_unix_ms, rows))
}

/// A window's attention rows: the newest publication's rows for `person` (every person's for
/// none), at most `keep` of them, with that publication's cut and publication time. `None`
/// while a refresher runs but has published nothing yet; the refresher is woken either way
/// when the publication is older than `index`.
pub(crate) fn published_attention_rows(
    store: &Store,
    index: u64,
    person: Option<&str>,
    keep: usize,
) -> Option<(u64, u128, Vec<Value>)> {
    let Some(publication) = store.published_attention_list() else {
        store.request_attention_list_refresh();
        return None;
    };
    if publication.cut < index {
        store.request_attention_list_refresh();
    }
    let rows = publication
        .rows
        .iter()
        .filter(|row| person.is_none_or(|person| row["person_id"] == person))
        .take(keep)
        .cloned()
        .collect();
    Some((publication.cut, publication.published_at_unix_ms, rows))
}

/// A window of a published view before its first publication: retry shortly.
pub(super) fn published_view_not_ready(collection: &str) -> ApiError {
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: format!("{collection}-not-ready"),
        message: format!("the {collection} view is still being prepared; retry shortly"),
        details: Box::default(),
    }
}
