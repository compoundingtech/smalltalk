//! Local creation receipts establish resource identity and opener attribution without a network read.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use notify::Watcher as _;
use tokio::sync::{Notify, watch};

use crate::{model::ClaimInput, recorder::Receipt, store::Store};

/// Watches the spool, with bounded exponential backoff scans as a fallback.
pub async fn run(
    store: Arc<Store>,
    directory: PathBuf,
    notify: Arc<Notify>,
    event_notify: watch::Sender<u64>,
) {
    let spool_wake = Arc::new(Notify::new());
    let _watcher = spool_watcher(&directory, spool_wake.clone())
        .map_err(|error| eprintln!("st3: recorder spool watch unavailable; using backoff: {error:#}"))
        .ok();
    let mut delay = Duration::from_secs(1);
    loop {
        let receipt_store = store.clone();
        let receipts = directory.clone();
        match tokio::task::spawn_blocking(move || ingest_once(&receipt_store, &receipts)).await {
            Ok(Ok(0)) => delay = (delay * 2).min(Duration::from_secs(60)),
            Ok(Ok(_)) => {
                delay = Duration::from_secs(1);
                crate::performance::record_wake("recorder receipts", Some("resource.observed"));
                notify.notify_one();
                event_notify.send_modify(|generation| *generation = generation.saturating_add(1));
            }
            Ok(Err(error)) => eprintln!("st3: recorder receipt ingestion failed: {error:#}"),
            Err(error) => eprintln!("st3: recorder receipt ingestion stopped: {error}"),
        }
        tokio::select! {
            () = spool_wake.notified() => {}
            () = tokio::time::sleep(delay) => {}
        }
    }
}

fn spool_watcher(directory: &Path, wake: Arc<Notify>) -> Result<notify::RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        // Scanning the spool opens its directory and reads receipts. Those access
        // events must not schedule another scan of the same directory.
        if event.is_ok_and(|event| !matches!(event.kind, notify::EventKind::Access(_))) {
            wake.notify_one();
        }
    })?;
    watcher.watch(directory, notify::RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

/// Imports completed JSON files, returning how many new claims were appended.
/// Invalid receipts are discarded; failed appends retry after 1, 2, 4, and 8 seconds,
/// then dead-letter on the fifth failure. Retry file modification times persist not-before deadlines.
/// A committed receipt is safe to replay if deletion fails: URL and actor identify its claim.
pub fn ingest_once(store: &Store, directory: &Path) -> Result<usize> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).context("read recorder receipts"),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.context("read recorder receipt entry")?;
        if entry.file_type()?.is_file()
            && entry.path().extension().is_some_and(|extension| extension == "tmp")
            && entry.metadata()?.modified()?.elapsed().is_ok_and(|age| age >= Duration::from_secs(3600))
        {
            fs::remove_file(entry.path()).context("remove stale recorder temporary file")?;
            continue;
        }
        if entry.file_type()?.is_file() && entry.path().extension().is_some_and(|extension| extension == "json") {
            files.push(entry.path());
        }
    }
    // Recorder names are creation nanoseconds followed by pid; the earliest opener wins.
    files.sort();
    let mut appended = 0;
    for path in files {
        let stem = path.file_stem().unwrap().to_string_lossy();
        let (base, attempts) = stem.rsplit_once(".attempt-")
            .and_then(|(base, attempt)| attempt.parse::<u32>().ok().map(|attempt| (base, attempt)))
            .unwrap_or((&stem, 0));
        if attempts > 0 && fs::metadata(&path)?.modified()? > SystemTime::now() {
            continue;
        }
        let result = (|| -> Result<bool> {
            let bytes = fs::read(&path)?;
            let input = serde_json::from_slice::<Receipt>(&bytes)
                .context("decode recorder receipt")
                .and_then(claim_input);
            let input = match input {
                Ok(input) => input,
                Err(error) => {
                    eprintln!("st3: discarding invalid recorder receipt {}: {error:#}", path.display());
                    return Ok(false);
                }
            };
            // A second capture of the same creation may come from another mission run.
            // URL and actor, not that later context, identify the consumed receipt.
            if store.operation_claim(input.idempotency_key.as_deref().unwrap())?.is_some() {
                return Ok(false);
            }
            let (_, inserted) = store.append_claim_outcome(&input)?;
            Ok(inserted)
        })();
        match result {
            Ok(inserted) => {
                appended += usize::from(inserted);
                if let Err(error) = fs::remove_file(&path) {
                    eprintln!("st3: could not remove consumed recorder receipt {}: {error}", path.display());
                }
            }
            Err(error) => {
                let attempts = attempts.saturating_add(1);
                let destination = if attempts >= 5 {
                    path.with_file_name(format!("{base}.dead-letter"))
                } else {
                    // Persist the deadline before renaming, so watcher events cannot retry early.
                    let not_before = SystemTime::now() + Duration::from_secs(1 << (attempts - 1));
                    fs::File::options().write(true).open(&path)?
                        .set_times(fs::FileTimes::new().set_modified(not_before))
                        .context("schedule failed recorder receipt retry")?;
                    path.with_file_name(format!("{base}.attempt-{attempts}.json"))
                };
                fs::rename(&path, &destination).context("retain failed recorder receipt")?;
                if attempts >= 5 {
                    eprintln!("st3: dead-lettered recorder receipt {} after {attempts} attempts: {error:#}", destination.display());
                }
            }
        }
    }
    Ok(appended)
}

fn claim_input(receipt: Receipt) -> Result<ClaimInput> {
    ensure!(receipt.schema == "st3.recorder.receipt.v1", "unsupported recorder receipt schema");
    chrono::DateTime::parse_from_rfc3339(&receipt.at).context("invalid recorder receipt timestamp")?;
    let url = reqwest::Url::parse(&receipt.url).context("invalid recorder receipt URL")?;
    ensure!(url.scheme() == "https" && url.host_str() == Some("github.com"), "receipt URL is not a GitHub resource");
    let mut parts = url.path_segments().context("receipt URL has no path")?;
    let (Some(owner), Some(repo), Some(collection), Some(number), None) =
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
    else {
        anyhow::bail!("receipt URL is not an issue or pull request");
    };
    ensure!(!owner.is_empty() && !repo.is_empty(), "receipt URL has no repository");
    let (segment, kind) = match collection {
        "issues" => ("issue", "vcs.issue"),
        "pull" => ("pull-request", "vcs.pull-request"),
        _ => anyhow::bail!("receipt URL is not an issue or pull request"),
    };
    let number = number.parse::<u64>().context("invalid receipt resource number")?;
    ensure!(number > 0, "invalid receipt resource number");
    let repository = format!("resource/github/{owner}/{repo}");
    let idempotency_key = format!("receipt:{}:{}", receipt.url, receipt.actor);
    let mut facts = BTreeMap::from([
        ("repository".into(), Value::String(repository.clone())),
        ("number".into(), Value::from(number)),
        ("url".into(), Value::String(receipt.url)),
        ("opened_by".into(), Value::String(receipt.actor)),
    ]);
    if let Some(run) = receipt.mission_run {
        facts.insert("opened_by_run".into(), Value::String(format!("mission-run/{run}")));
    }
    st3_schema::registry()
        .validate_resource_facts(kind, &facts)
        .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?;
    Ok(ClaimInput {
        subject: format!("{repository}/{segment}/{number}"),
        kind: "resource.observed".into(),
        actor: None,
        fields: BTreeMap::from([
            ("kind".into(), Value::String(kind.into())),
            ("facts".into(), serde_json::to_value(facts)?),
            ("attribution_only".into(), Value::Bool(true)),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(idempotency_key),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn receipt(url: &str, actor: &str, run: Option<&str>) -> Receipt {
        Receipt {
            schema: "st3.recorder.receipt.v1".into(),
            url: url.into(),
            actor: actor.into(),
            mission_run: run.map(str::to_owned),
            exit_code: Some(0),
            at: "2026-10-03T12:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn scanning_an_empty_spool_does_not_wake_an_idle_consumer() {
        let spool = tempfile::tempdir().unwrap();
        let wake = Arc::new(Notify::new());
        let _watcher = spool_watcher(spool.path(), wake.clone()).unwrap();
        assert_eq!(fs::read_dir(spool.path()).unwrap().count(), 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(250), wake.notified())
                .await
                .is_err(),
            "the consumer's own scan queued another scan"
        );
        // Ignoring reads must not suppress the next atomic publication.
        fs::write(spool.path().join("receipt.tmp"), b"partial").unwrap();
        fs::rename(spool.path().join("receipt.tmp"), spool.path().join("receipt.json")).unwrap();
        tokio::time::timeout(Duration::from_secs(2), wake.notified()).await.unwrap();
    }

    #[tokio::test]
    async fn spool_publication_wakes_an_idle_consumer() {
        let spool = tempfile::tempdir().unwrap();
        let wake = Arc::new(Notify::new());
        let _watcher = spool_watcher(spool.path(), wake.clone()).unwrap();
        fs::write(spool.path().join("receipt.tmp"), b"partial").unwrap();
        fs::rename(spool.path().join("receipt.tmp"), spool.path().join("receipt.json")).unwrap();
        tokio::time::timeout(Duration::from_secs(2), wake.notified()).await.unwrap();
    }

    #[test]
    fn failed_append_is_dead_lettered_after_five_attempts() {
        let store = Store::open_memory("receipt-node").unwrap();
        store.append_claim(&ClaimInput {
            subject: "resource/github/acme/demo/issue/9".into(),
            kind: "resource.observed".into(), actor: None,
            fields: BTreeMap::from([
                ("kind".into(), json!("vcs.pull-request")),
                ("facts".into(), json!({"number": 99})),
            ]), evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
        let spool = tempfile::tempdir().unwrap();
        fs::write(spool.path().join("1-1.json"), serde_json::to_vec(
            &receipt("https://github.com/acme/demo/issues/9", "agent/builder", None)
        ).unwrap()).unwrap();
        for attempt in 1..=5 {
            if attempt > 1 {
                fs::File::options().write(true)
                    .open(spool.path().join(format!("1-1.attempt-{}.json", attempt - 1))).unwrap()
                    .set_times(fs::FileTimes::new().set_modified(
                        std::time::SystemTime::now() - Duration::from_secs(1)
                    )).unwrap();
            }
            let failed_at = std::time::SystemTime::now();
            assert_eq!(ingest_once(&store, spool.path()).unwrap(), 0);
            let name = if attempt == 5 { "1-1.dead-letter".into() }
                else { format!("1-1.attempt-{attempt}.json") };
            let path = spool.path().join(name);
            assert!(path.exists());
            if attempt < 5 {
                assert!(fs::metadata(&path).unwrap().modified().unwrap()
                    >= failed_at + Duration::from_secs(1 << (attempt - 1)));
            }
        }
        assert_eq!(ingest_once(&store, spool.path()).unwrap(), 0);
        assert!(spool.path().join("1-1.dead-letter").exists());
    }

    #[test]
    fn pending_retry_survives_repeated_scans_and_retries_when_due() {
        let store = Store::open_memory("receipt-node").unwrap();
        store.append_claim(&ClaimInput {
            subject: "resource/github/acme/demo/issue/9".into(),
            kind: "resource.observed".into(), actor: None,
            fields: BTreeMap::from([
                ("kind".into(), json!("vcs.pull-request")),
                ("facts".into(), json!({"number": 99})),
            ]), evidence: Vec::new(), expected_subject: None, idempotency_key: None,
        }).unwrap();
        let spool = tempfile::tempdir().unwrap();
        fs::write(spool.path().join("1-1.json"), serde_json::to_vec(
            &receipt("https://github.com/acme/demo/issues/9", "agent/builder", None)
        ).unwrap()).unwrap();
        assert_eq!(ingest_once(&store, spool.path()).unwrap(), 0);
        let retry = spool.path().join("1-1.attempt-1.json");
        fs::File::options().write(true).open(&retry).unwrap().set_times(
            fs::FileTimes::new().set_modified(std::time::SystemTime::now() + Duration::from_secs(3600))
        ).unwrap();
        for _ in 0..10 {
            assert_eq!(ingest_once(&store, spool.path()).unwrap(), 0);
            assert!(retry.exists(), "an early scan consumed a retry attempt");
        }
        drop(store);
        let restarted_store = Store::open_memory("receipt-node").unwrap();
        assert_eq!(ingest_once(&restarted_store, spool.path()).unwrap(), 0);
        assert!(restarted_store.claims_page(
            Some("resource/github/acme/demo/issue/9"), None, 0, None, false, 100
        ).unwrap().claims.is_empty());
        fs::File::options().write(true).open(&retry).unwrap().set_times(
            fs::FileTimes::new().set_modified(std::time::SystemTime::now() - Duration::from_secs(1))
        ).unwrap();
        assert_eq!(ingest_once(&restarted_store, spool.path()).unwrap(), 1);
        assert!(!retry.exists());
        assert_eq!(restarted_store.claims_page(
            Some("resource/github/acme/demo/issue/9"), None, 0, None, false, 100
        ).unwrap().claims.len(), 1);
    }

    #[test]
    fn stale_temporary_files_are_removed_but_active_publications_remain() {
        let store = Store::open_memory("receipt-node").unwrap();
        let spool = tempfile::tempdir().unwrap();
        let stale = spool.path().join("old.tmp");
        let fresh = spool.path().join("new.tmp");
        fs::write(&stale, b"partial").unwrap();
        fs::write(&fresh, b"partial").unwrap();
        fs::File::options().write(true).open(&stale).unwrap().set_times(
            fs::FileTimes::new().set_modified(std::time::SystemTime::now() - Duration::from_secs(3601))
        ).unwrap();
        assert_eq!(ingest_once(&store, spool.path()).unwrap(), 0);
        assert!(!stale.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn receipt_duplicates_collapse_and_consumed_files_disappear() {
        let store = Store::open_memory("receipt-node").unwrap();
        let spool = tempfile::tempdir().unwrap();
        let value = receipt("https://github.com/acme/demo/issues/8", "agent/builder", Some("raw/run"));
        for name in ["1-1.json", "2-1.json"] {
            fs::write(spool.path().join(name), serde_json::to_vec(&value).unwrap()).unwrap();
        }
        assert_eq!(ingest_once(&store, spool.path()).unwrap(), 1);
        assert!(!spool.path().join("1-1.json").exists());
        assert!(!spool.path().join("2-1.json").exists());
        let subject = "resource/github/acme/demo/issue/8";
        let actual = store.latest_actual_value(subject).unwrap().unwrap();
        assert_eq!(actual["kind"], "vcs.issue");
        assert_eq!(actual["facts"], json!({
            "repository": "resource/github/acme/demo", "number": 8,
            "url": value.url, "opened_by": "agent/builder", "opened_by_run": "mission-run/raw/run"
        }));
        let claims = store.claims_page(Some(subject), None, 0, None, false, 100).unwrap().claims;
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].actor, None, "daemon ingestion must not impersonate the opener");
        let replay = receipt(&value.url, &value.actor, Some("later-run"));
        fs::write(spool.path().join("3-1.json"), serde_json::to_vec(&replay).unwrap()).unwrap();
        assert_eq!(ingest_once(&store, spool.path()).unwrap(), 0);
        assert!(!spool.path().join("3-1.json").exists());
        assert_eq!(
            store.latest_actual_value(subject).unwrap().unwrap()["facts"]["opened_by_run"],
            "mission-run/raw/run"
        );
    }

    #[test]
    fn invalid_receipts_are_discarded_without_blocking_valid_receipts() {
        let store = Store::open_memory("receipt-node").unwrap();
        let spool = tempfile::tempdir().unwrap();
        fs::write(spool.path().join("1-1.json"), b"{broken").unwrap();
        let invalid_actor = receipt("https://github.com/acme/demo/issues/1", "not-a-subject", None);
        fs::write(
            spool.path().join("2-1.json"),
            serde_json::to_vec(&invalid_actor).unwrap(),
        ).unwrap();
        let valid = receipt("https://github.com/acme/demo/issues/2", "agent/builder", None);
        fs::write(
            spool.path().join("3-1.json"),
            serde_json::to_vec(&valid).unwrap(),
        ).unwrap();
        assert_eq!(ingest_once(&store, spool.path()).unwrap(), 1);
        for name in ["1-1.json", "2-1.json", "3-1.json"] {
            assert!(!spool.path().join(name).exists());
        }
        assert!(store.latest_actual_value("resource/github/acme/demo/issue/1").unwrap().is_none());
        assert_eq!(
            store.latest_actual_value("resource/github/acme/demo/issue/2").unwrap().unwrap()["facts"]["opened_by"],
            "agent/builder"
        );
    }

    #[test]
    fn second_receipt_from_another_agent_appends_nothing() {
        let store = Store::open_memory("receipt-node").unwrap();
        let first = claim_input(receipt("https://github.com/acme/demo/pull/9", "agent/first", Some("first"))).unwrap();
        store.append_claim(&first).unwrap();
        let before = store.latest_actual_value(&first.subject).unwrap();
        let second = claim_input(receipt("https://github.com/acme/demo/pull/9", "agent/second", Some("second"))).unwrap();
        let (_, inserted) = store.append_claim_outcome(&second).unwrap();
        assert!(!inserted, "an opener already recorded must make a second receipt a no-op");
        assert_eq!(store.claims_page(Some(&first.subject), None, 0, None, false, 100).unwrap().claims.len(), 1);
        assert_eq!(store.latest_actual_value(&first.subject).unwrap(), before);
    }


    #[test]
    fn receipt_preserves_a_mission_run_only_opener_without_taking_ownership() {
        for (path, kind, segment) in [
            ("issues", "vcs.issue", "issue"),
            ("pull", "vcs.pull-request", "pull-request"),
        ] {
            let store = Store::open_memory("receipt-node").unwrap();
            let url = format!("https://github.com/acme/demo/{path}/9");
            let subject = format!("resource/github/acme/demo/{segment}/9");
            let prior = json!({
                "state": "open", "title": "Observed", "opened_by_run": "mission-run/original",
            });
            store.append_claim(&ClaimInput {
                subject: subject.clone(),
                kind: "resource.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("kind".into(), json!(kind)),
                    ("facts".into(), prior.clone()),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            }).unwrap();
            let (_, inserted) = store.append_claim_outcome(
                &claim_input(receipt(&url, "agent/later", Some("later"))).unwrap(),
            ).unwrap();
            assert!(!inserted);
            let facts = store.latest_actual_value(&subject).unwrap().unwrap()["facts"].clone();
            assert_eq!(facts, prior);
            assert_eq!(store.claims_page(Some(&subject), None, 0, None, false, 100).unwrap().claims.len(), 1);
        }
    }
}
