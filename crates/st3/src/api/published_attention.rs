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

/// How long after a window last read the list the refresher keeps following commits. After
/// that it waits for a window to ask; that window gets the newest publication meanwhile.
const ATTENTION_LIST_IDLE: Duration = Duration::from_secs(600);

/// Keep the attention list published off the request path. As the daemon starts it folds the
/// list once; after that it refreshes when a commit lands, when the clock passes the list's
/// period, or when a window asks. A refresh folds again only when something attention reads
/// changed (see `Store::attention_list_delta`); otherwise it keeps the same rows.
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
        loop {
            let started = tokio::time::Instant::now();
            let reader = store.clone();
            let summary_state = state.clone();
            // One view at a time: attention, every person's glasses and arrangements, then the
            // summary rows windows read.
            // One view's failure leaves the others refreshing.
            let refreshed = tokio::task::spawn_blocking(move || {
                [
                    ("attention", crate::performance::task("attention/refresh", || {
                        refresh_attention_list(&reader)
                    })),
                    ("glasses and arrangements", crate::performance::task("person-views/refresh", || {
                        refresh_owner_lists(&reader)
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
                        if let Err(error) = result {
                            eprintln!("st3: {view} view refresh failed: {error:#}");
                        }
                    }
                }
                Err(error) => eprintln!("st3: published view refresh stopped: {error}"),
            }
            tokio::time::sleep(started.elapsed().max(ATTENTION_LIST_REFRESH_PAUSE)).await;
            let idle = ATTENTION_LIST_IDLE.as_millis() as u64;
            // Commits and clock ticks matter only while someone reads; a window's ask always does.
            loop {
                tokio::select! {
                    () = wake.notified() => break,
                    result = changed.changed() => {
                        if result.is_err() {
                            return;
                        }
                        if store.attention_list_read_within(idle) {
                            break;
                        }
                    }
                    // The summary counts working agents from the published roster.
                    result = roster.changed() => {
                        if result.is_err() {
                            return;
                        }
                        if store.attention_list_read_within(idle) {
                            break;
                        }
                    }
                    () = tokio::time::sleep(ATTENTION_LIST_CLOCK) => {
                        if store.attention_list_read_within(idle) {
                            break;
                        }
                    }
                }
            }
        }
    });
}

/// Publish the list at the current cut in one short snapshot: the same rows when nothing
/// attention reads changed since the newest publication, else a fold of every person's rows.
/// A fold that comes out equal keeps the published rows, so windows have nothing to reread.
pub(crate) fn refresh_attention_list(store: &Store) -> anyhow::Result<()> {
    let (generation, previous) = store.newest_attention_list();
    let now = client_now_ms();
    let clock = ATTENTION_LIST_CLOCK.as_millis();
    store.read_snapshot(|index| {
        let cause = match &previous {
            None => Some("first".to_owned()),
            Some(previous)
                if now < previous.evaluated_at_unix_ms
                    || now - previous.evaluated_at_unix_ms >= clock =>
            {
                Some("clock".to_owned())
            }
            Some(previous) => match store.attention_list_delta(previous, index)? {
                AttentionDelta::Unchanged if previous.cut == index => return Ok(()),
                AttentionDelta::Unchanged => None,
                AttentionDelta::Refold(cause) => Some(cause),
            },
        };
        let projection = store.attention_projection_frontier()?;
        let (rows, evaluated_at_unix_ms) = match (cause, &previous) {
            (None, Some(previous)) => (Arc::clone(&previous.rows), previous.evaluated_at_unix_ms),
            (cause, _) => {
                store.note_attention_list_fold(cause.as_deref().unwrap_or("first"));
                let rows = client_attention_resources_at(store, None, false, now)?;
                let rows = match &previous {
                    Some(previous) if *previous.rows == rows => Arc::clone(&previous.rows),
                    _ => Arc::new(rows),
                };
                (rows, now)
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
                published_at_unix_ms: client_now_ms(),
                rows,
            },
            changed,
        );
        Ok(())
    })
}

/// Publish every person's glasses and arrangements at the current cut, each in its own short
/// snapshot, reading again only the people whose rows may have changed.
pub(crate) fn refresh_owner_lists(store: &Store) -> anyhow::Result<()> {
    for view in OwnerView::ALL {
        store.refresh_owner_list(view, client_now_ms())?;
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
