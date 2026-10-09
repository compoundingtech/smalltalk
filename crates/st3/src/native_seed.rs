//! One durable first-native-launch opportunity per seat, independent of declaration and harness.
use crate::model::{ClaimInput, ClaimRecord, ClaimsPage};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::path::Path;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Outcome {
    Fresh,
    Seeded { session_id: String },
}

fn marker(subject: &str) -> String {
    format!(
        "custom/agent/first-native-launch-{}",
        hex::encode(Sha256::digest(subject.as_bytes()))
    )
}

fn receipt(subject: &str, incarnation: &str, outcome: &Outcome) -> ClaimInput {
    let mut fields = serde_json::from_value::<std::collections::BTreeMap<String, Value>>(
        serde_json::to_value(outcome).expect("outcome serializes"),
    )
    .expect("outcome is an object");
    fields.insert("agent".into(), subject.into());
    fields.insert("incarnation".into(), incarnation.into());
    fields.insert(
        "invocation_id".into(),
        uuid::Uuid::now_v7().to_string().into(),
    );
    ClaimInput {
        subject: marker(subject),
        kind: "custom.agent.first-native-launch".into(),
        actor: Some(subject.into()),
        fields,
        evidence: vec![],
        expected_subject: None,
        // One atomic operation key fences the seat; invocation_id distinguishes the winning
        // receipt from a cached response returned to another provider attempt.
        idempotency_key: Some(format!("first-native-launch:{}", marker(subject))),
    }
}

async fn marker_claim(
    client: &crate::client::Client,
    subject: &str,
    kind: &str,
) -> Result<Option<ClaimRecord>> {
    let mut after = 0;
    loop {
        let page: ClaimsPage = client.get(&format!(
            "/v1/claims?subject={}&after_index={after}&order=asc&limit=500",
            urlencoding::encode(&marker(subject))
        )).await?;
        let Some(last) = page.claims.last() else { return Ok(None); };
        after = last.store_index;
        let exhausted = page.claims.len() < 500;
        if let Some(record) = page.claims.into_iter().find(|record| record.kind == kind) {
            return Ok(Some(record));
        }
        if exhausted { return Ok(None); }
    }
}

async fn prior(client: &crate::client::Client, subject: &str) -> Result<Option<ClaimRecord>> {
    marker_claim(client, subject, "custom.agent.first-native-launch").await
}

/// Acknowledge an interrupted seeded attempt without deleting or rearming its receipt.
pub async fn acknowledge(
    client: &crate::client::Client,
    subject: &str,
    actor: &str,
    reason: &str,
) -> Result<ClaimRecord> {
    anyhow::ensure!(!reason.trim().is_empty(), "seed acknowledgement needs a reason");
    let record = prior(client, subject).await?.context("no first-native-launch receipt exists")?;
    anyhow::ensure!(
        matches!(serde_json::from_value::<Outcome>(record.body["fields"].clone())?, Outcome::Seeded { .. }),
        "only a seeded first-native-launch receipt needs acknowledgement"
    );
    client.post("/v1/claims", &ClaimInput {
        subject: marker(subject),
        kind: "custom.agent.first-native-launch-acknowledged".into(),
        actor: Some(actor.into()),
        fields: std::collections::BTreeMap::from([
            ("agent".into(), subject.into()),
            ("receipt".into(), record.id.clone().into()),
            ("reason".into(), reason.into()),
        ]),
        evidence: vec![record.id.clone()],
        expected_subject: None,
        idempotency_key: Some(format!("first-native-launch-acknowledged:{}", record.id)),
    }).await
}

async fn existing_outcome(
    client: &crate::client::Client,
    subject: &str,
    incarnation: &str,
    seed: Option<&Path>,
    record: &ClaimRecord,
) -> Result<Option<String>> {
    if matches!(serde_json::from_value::<Outcome>(record.body["fields"].clone())?, Outcome::Fresh) {
        return Ok(None);
    }
    if marker_claim(client, subject, "custom.agent.first-native-launch-acknowledged")
        .await?
        .is_some_and(|claim| claim.body.pointer("/fields/receipt").and_then(Value::as_str)
            == Some(record.id.as_str()))
    {
        return Ok(None);
    }
    let reason = "first-native-launch-incomplete: the seeded attempt has no recorded native binding; \
        never reseeding. Remove seed to launch fresh, or acknowledge with \
        `st agents acknowledge-seed AGENT --reason REASON` to accept fresh launches without rearming seed.";
    let _: ClaimRecord = client.post("/v1/claims", &ClaimInput {
        subject: subject.into(),
        kind: "harness.diagnostic".into(),
        actor: Some(subject.into()),
        fields: std::collections::BTreeMap::from([
            ("code".into(), "first-native-launch-incomplete".into()),
            ("severity".into(), "warning".into()),
            ("status".into(), "incomplete".into()),
            ("reason".into(), reason.into()),
            ("incarnation_id".into(), incarnation.into()),
        ]),
        evidence: vec![record.id.clone()],
        expected_subject: None,
        idempotency_key: Some(format!("first-native-launch-incomplete:{}:{incarnation}", record.id)),
    }).await?;
    eprintln!("{reason}");
    anyhow::ensure!(seed.is_none(), "{reason}");
    Ok(None)
}

/// Generated exact resume/continuation takes precedence over a declaration's seed. Omit the
/// wrapper selector so the driver boundary never receives both opportunities together.
pub fn omit_seed_for_native_resume(member: &mut crate::model::MemberSpec) {
    if member.driver.as_deref() != Some("omp")
        || ![
            crate::suspension::RESUME_ENV,
            crate::suspension::CONTINUE_ENV,
            crate::suspension::CONTINUE_PATH_ENV,
        ]
        .iter()
        .any(|variable| member.environment.contains_key(*variable))
    {
        return;
    }
    let crate::model::LaunchSpec::Argv(argv) = &mut member.launch else {
        return;
    };
    // Skip scalar wrapper values: an initial message may literally be "--seed" or "--".
    let mut index = 3;
    while index < argv.len() {
        match argv[index].as_str() {
            "--" => break,
            "--seed" => {
                argv.drain(index..index + 2);
                return;
            }
            "--subject" | "--identity" | "--initial-message" | "--initial-message-id" => {
                index += 2;
            }
            _ => index += 1,
        }
    }
}

/// Validate and stage before compare-and-create; only the winning invocation may spawn seeded.
/// Fresh outcomes never block; unbound seeded outcomes never reseed and surface a durable notice.
pub async fn first_launch(
    client: &crate::client::Client,
    subject: &str,
    incarnation: &str,
    driver: &str,
    seed: Option<&Path>,
    sessions: &Path,
    strict_resume: bool,
) -> Result<Option<String>> {
    anyhow::ensure!(seed.is_none() || driver == "omp", "native seed is OMP-only");
    anyhow::ensure!(
        seed.is_none() || !strict_resume,
        "native seed cannot accompany resume environment"
    );
    let mut after = 0;
    let mut deliberately_fresh = strict_resume;
    loop {
        let page: ClaimsPage = client
            .get(&format!(
                "/v1/claims?subject={}&after_index={after}&order=asc&limit=500",
                urlencoding::encode(subject)
            ))
            .await?;
        if page
            .claims
            .iter()
            .any(|claim| claim.kind == "harness.session-file")
        {
            return Ok(None);
        }
        deliberately_fresh |= page.claims.iter().any(|claim| {
            claim.kind == "runtime.action.requested"
                && claim.body.pointer("/fields/action").and_then(Value::as_str)
                    == Some("fresh-context")
        });
        let Some(last) = page.claims.last() else {
            break;
        };
        after = last.store_index;
        if page.claims.len() < 500 {
            break;
        }
    }
    if let Some(record) = prior(client, subject).await? {
        return existing_outcome(client, subject, incarnation, seed, &record).await;
    }
    let outcome = match seed.filter(|_| !deliberately_fresh) {
        Some(seed) => Outcome::Seeded {
            session_id: stage(seed, sessions)?,
        },
        None => Outcome::Fresh,
    };
    let input = receipt(subject, incarnation, &outcome);
    match client.post::<_, ClaimRecord>("/v1/claims", &input).await {
        Ok(record) if record.body.pointer("/fields/invocation_id") != input.fields.get("invocation_id") => {
            return existing_outcome(client, subject, incarnation, seed, &record).await;
        }
        Ok(_) => {}
        Err(error) => {
            if let Some(record) = prior(client, subject).await? {
                return existing_outcome(client, subject, incarnation, seed, &record).await;
            }
            return Err(error);
        }
    }
    Ok(match outcome {
        Outcome::Fresh => None,
        Outcome::Seeded { session_id } => Some(session_id),
    })
}

/// Seed validation bounds include newline bytes and apply before inventory creation.
pub const MAX_SEED_LINE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_SEED_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum SeedValidationLimitError {
    LineBytes { line: usize, limit: usize },
    TotalBytes { limit: u64 },
}

impl std::fmt::Display for SeedValidationLimitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LineBytes { line, limit } => {
                write!(formatter, "seed transcript line {line} exceeds {limit} bytes")
            }
            Self::TotalBytes { limit } => {
                write!(formatter, "seed transcript exceeds {limit} total bytes")
            }
        }
    }
}

impl std::error::Error for SeedValidationLimitError {}

fn read_seed_line(
    reader: &mut impl std::io::BufRead,
    bytes: &mut Vec<u8>,
    total: &mut u64,
    line: usize,
) -> Result<usize> {
    use std::io::{BufRead as _, Read as _};
    bytes.clear();
    // Take bounds read_until's allocation even when a source has no newline.
    let read_limit = (MAX_SEED_LINE_BYTES as u64 + 1)
        .min(MAX_SEED_TOTAL_BYTES.saturating_sub(*total) + 1);
    let count = std::io::Read::by_ref(reader)
        .take(read_limit)
        .read_until(b'\n', bytes)?;
    *total += count as u64;
    if *total > MAX_SEED_TOTAL_BYTES {
        return Err(SeedValidationLimitError::TotalBytes {
            limit: MAX_SEED_TOTAL_BYTES,
        }
        .into());
    }
    if count > MAX_SEED_LINE_BYTES {
        return Err(SeedValidationLimitError::LineBytes {
            line,
            limit: MAX_SEED_LINE_BYTES,
        }
        .into());
    }
    Ok(count)
}

fn ensure_seed_owner(metadata: &std::fs::Metadata) -> Result<()> {
    anyhow::ensure!(metadata.is_file(), "seed transcript must be a file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        // SAFETY: geteuid takes no arguments and does not dereference memory.
        anyhow::ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "seed transcript must belong to the launch owner"
        );
    }
    Ok(())
}

fn ensure_staged_seed(target: &Path, source: &std::fs::Metadata, id: &str) -> Result<()> {
    use std::io::BufReader;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    anyhow::ensure!(
        !std::fs::symlink_metadata(target)?.file_type().is_symlink(),
        "seed inventory filename collision: staged transcript must not be a link"
    );
    let file = options.open(target)?;
    let staged = file.metadata()?;
    ensure_seed_owner(&staged)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        anyhow::ensure!(
            staged.dev() != source.dev() || staged.ino() != source.ino(),
            "seed inventory filename collision: staged transcript must be independent"
        );
    }
    #[cfg(not(unix))]
    let _ = source;
    // Only inspect the identifying header: the provider may be appending its next entry.
    let mut reader = BufReader::new(file);
    let mut bytes = Vec::new();
    let mut total = 0;
    let mut header = None;
    for index in 0..2 {
        if read_seed_line(&mut reader, &mut bytes, &mut total, index + 1)? == 0 {
            break;
        }
        let value: Value = serde_json::from_slice(&bytes).context("corrupt staged seed header")?;
        if value["type"] == "session" {
            header = value["id"].as_str().map(str::to_owned);
            if header.as_deref() == Some(id) {
                break;
            }
        }
    }
    anyhow::ensure!(
        header.as_deref() == Some(id),
        "seed inventory filename collision: filename and session header disagree"
    );
    Ok(())
}

/// Copy only the selected owner-local transcript into this seat's inventory.
pub fn stage(seed: &Path, sessions: &Path) -> Result<String> {
    use std::io::{BufReader, Read as _, Seek as _, Write as _};
    anyhow::ensure!(seed.is_absolute(), "seed transcript path must be absolute");
    let file = std::fs::File::open(seed).context("open seed transcript")?;
    let metadata = file.metadata()?;
    ensure_seed_owner(&metadata)?;
    if metadata.len() > MAX_SEED_TOTAL_BYTES {
        return Err(SeedValidationLimitError::TotalBytes {
            limit: MAX_SEED_TOTAL_BYTES,
        }
        .into());
    }
    let name = seed
        .file_name()
        .and_then(|name| name.to_str())
        .context("seed filename is not UTF-8")?;
    let id = name
        .strip_suffix(".jsonl")
        .and_then(|name| name.rsplit_once('_'))
        .map(|(_, id)| id)
        .context("seed filename must be <time>_<uuid>.jsonl")?;
    uuid::Uuid::parse_str(id).context("seed session ID is not a UUID")?;

    // Capture exactly the validated bytes outside the inventory. A second source read
    // could otherwise copy unvalidated edits made between validation and publication.
    let mut snapshot = tempfile::tempfile().context("capture seed transcript")?;
    let mut reader = BufReader::new(file.take(MAX_SEED_TOTAL_BYTES + 1));
    let mut bytes = Vec::new();
    let mut total = 0;
    let mut index = 0;
    let mut header = None;
    while read_seed_line(&mut reader, &mut bytes, &mut total, index + 1)? != 0 {
        let value: Value = serde_json::from_slice(&bytes).context("corrupt seed transcript")?;
        if index < 2 && value["type"] == "session" {
            header = value["id"].as_str().map(str::to_owned);
        }
        snapshot.write_all(&bytes)?;
        index += 1;
    }
    anyhow::ensure!(
        header.as_deref() == Some(id),
        "seed filename and session header disagree"
    );
    std::fs::create_dir_all(sessions)?;
    let inventory = std::fs::symlink_metadata(sessions)?;
    anyhow::ensure!(
        inventory.is_dir() && !inventory.file_type().is_symlink(),
        "seed inventory must be a managed directory, not a link"
    );
    let target = sessions.join(name);
    match std::fs::symlink_metadata(&target) {
        Ok(_) => {
            ensure_staged_seed(&target, &metadata, id)?;
            return Ok(id.to_owned());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    snapshot.rewind()?;
    let mut pending = tempfile::NamedTempFile::new_in(sessions)?;
    std::io::copy(&mut snapshot, &mut pending)?;
    pending.as_file().sync_all()?;
    // No-clobber publication is atomic for concurrent calls for the same seat.
    match pending.persist_noclobber(&target) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure_staged_seed(&target, &metadata, id)?;
        }
        Err(error) => return Err(error.error).context("stage seed transcript"),
    }
    std::fs::File::open(sessions)?.sync_all()?;
    Ok(id.to_owned())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_seed_generated_resume_omits_only_the_wrapper_seed() {
        for message in ["--seed", "--"] {
            let intent = crate::parse_intent(
                &format!("version 2\nagent \"example\" {{ workspace \"/work\"; harness \"omp\" {{ seed \"/seed.jsonl\"; message {message:?} id=\"initial\"; args \"--\" \"--seed\" \"literal\"; }} }}"),
                "node",
            ).unwrap();
            let original = intent
                .subjects
                .values()
                .find_map(|subject| subject.member.as_ref())
                .unwrap();
            let crate::model::LaunchSpec::Argv(original_argv) = &original.launch else {
                panic!("typed launch expected");
            };
            let seed_index = original_argv
                .windows(2)
                .position(|pair| pair == ["--seed", "/seed.jsonl"])
                .unwrap();
            let mut expected = original_argv.clone();
            expected.drain(seed_index..seed_index + 2);
            for variable in [
                crate::suspension::RESUME_ENV,
                crate::suspension::CONTINUE_ENV,
                crate::suspension::CONTINUE_PATH_ENV,
            ] {
                let mut member = original.clone();
                member.environment.insert(variable.into(), "native".into());
                omit_seed_for_native_resume(&mut member);
                let crate::model::LaunchSpec::Argv(argv) = &member.launch else {
                    unreachable!()
                };
                assert_eq!(*argv, expected);
                assert_eq!(member.environment[variable], "native");
            }
            let mut first_launch = original.clone();
            omit_seed_for_native_resume(&mut first_launch);
            assert_eq!(first_launch.launch, original.launch);
        }
    }

    #[test]
    fn native_seed_validates_before_staging_and_copies_only_one_transcript() {
        let source = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        let seed = source.path().join(format!("time_{id}.jsonl"));
        let sessions = managed.path().join("inventory");
        assert!(stage(&seed, &sessions).is_err());
        std::fs::write(&seed, "broken").unwrap();
        assert!(stage(&seed, &sessions).is_err());
        assert!(!sessions.exists());
        std::fs::write(
            &seed,
            format!("{{\"type\":\"session\",\"id\":\"{id}\"}}\nbroken\n"),
        )
        .unwrap();
        assert!(stage(&seed, &sessions).is_err());
        std::fs::write(&seed, "{\"type\":\"session\",\"id\":\"other\"}\n").unwrap();
        assert!(stage(&seed, &sessions).is_err());
        assert!(!sessions.exists());
        std::fs::write(&seed, format!("{{\"type\":\"session\",\"id\":\"{id}\"}}\n")).unwrap();
        std::fs::write(source.path().join("unrelated.jsonl"), "private").unwrap();
        assert_eq!(stage(&seed, &sessions).unwrap(), id);
        assert_eq!(stage(&seed, &sessions).unwrap(), id);
        assert_eq!(std::fs::read_dir(&sessions).unwrap().count(), 1);
        assert!(
            !std::fs::symlink_metadata(&sessions)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let target = sessions.join(seed.file_name().unwrap());
        assert!(!std::fs::symlink_metadata(&target).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), std::fs::read(&seed).unwrap());
    }

    #[test]
    fn native_seed_copies_are_seat_local_and_repeated_staging_preserves_provider_edits() {
        use std::io::Write as _;
        let source = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        let seed = source.path().join(format!("time_{id}.jsonl"));
        let original = format!("{{\"type\":\"session\",\"id\":\"{id}\"}}\n");
        std::fs::write(&seed, &original).unwrap();
        let first = managed.path().join("first");
        let second = managed.path().join("second");
        assert_eq!(stage(&seed, &first).unwrap(), id);
        assert_eq!(stage(&seed, &second).unwrap(), id);
        let first_file = first.join(seed.file_name().unwrap());
        let second_file = second.join(seed.file_name().unwrap());
        assert_ne!(first_file, second_file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let identity = |path: &Path| {
                let metadata = std::fs::metadata(path).unwrap();
                (metadata.dev(), metadata.ino())
            };
            assert_ne!(identity(&seed), identity(&first_file));
            assert_ne!(identity(&seed), identity(&second_file));
            assert_ne!(identity(&first_file), identity(&second_file));
        }
        // A provider can be midway through appending a JSON entry when staging repeats.
        let mut provider = std::fs::OpenOptions::new().append(true).open(&first_file).unwrap();
        provider.write_all(b"{\"type\":\"message\",\"text\":\"in progress").unwrap();
        let evolved = std::fs::read(&first_file).unwrap();
        assert_eq!(stage(&seed, &first).unwrap(), id);
        assert_eq!(std::fs::read(&first_file).unwrap(), evolved);
        assert_eq!(std::fs::read_to_string(&seed).unwrap(), original);
        assert_eq!(std::fs::read_to_string(&second_file).unwrap(), original);
        std::fs::write(&seed, format!("{original}{{\"type\":\"source-edit\"}}\n")).unwrap();
        assert_eq!(stage(&seed, &second).unwrap(), id);
        assert_eq!(std::fs::read_to_string(&second_file).unwrap(), original);
        assert_eq!(std::fs::read(&first_file).unwrap(), evolved);
    }

    #[test]
    fn native_seed_concurrent_staging_publishes_one_complete_copy() {
        let source = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        let seed = source.path().join(format!("time_{id}.jsonl"));
        let transcript = format!(
            "{{\"type\":\"session\",\"id\":\"{id}\"}}\n{{\"type\":\"message\",\"text\":\"{}\"}}\n",
            "seed content".repeat(4096)
        );
        std::fs::write(&seed, &transcript).unwrap();
        let sessions = managed.path().join("inventory");
        let target = sessions.join(seed.file_name().unwrap());
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        assert_eq!(stage(&seed, &sessions).unwrap(), id);
                        assert_eq!(std::fs::read_to_string(&target).unwrap(), transcript);
                    })
                })
                .collect();
            for worker in workers {
                worker.join().unwrap();
            }
        });
        assert_eq!(std::fs::read_dir(&sessions).unwrap().count(), 1);
        assert!(!std::fs::symlink_metadata(&target).unwrap().file_type().is_symlink());
    }

    #[test]
    fn native_seed_caps_return_typed_errors_before_inventory_creation() {
        use std::io::Write as _;
        let source = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        let seed = source.path().join(format!("time_{id}.jsonl"));
        let sessions = managed.path().join("inventory");
        let header = format!("{{\"type\":\"session\",\"id\":\"{id}\"}}\n");
        let mut file = std::fs::File::create(&seed).unwrap();
        file.write_all(header.as_bytes()).unwrap();
        // Sparse, unterminated second line: no allocation proportional to source size.
        file.set_len(header.len() as u64 + MAX_SEED_LINE_BYTES as u64 + 1).unwrap();
        let error = stage(&seed, &sessions).unwrap_err();
        assert_eq!(
            error.downcast_ref::<SeedValidationLimitError>(),
            Some(&SeedValidationLimitError::LineBytes { line: 2, limit: MAX_SEED_LINE_BYTES })
        );
        assert!(!sessions.exists());
        file.set_len(MAX_SEED_TOTAL_BYTES + 1).unwrap();
        let error = stage(&seed, &sessions).unwrap_err();
        assert_eq!(
            error.downcast_ref::<SeedValidationLimitError>(),
            Some(&SeedValidationLimitError::TotalBytes { limit: MAX_SEED_TOTAL_BYTES })
        );
        assert!(!sessions.exists());
    }

    #[test]
    fn native_seed_stream_caps_include_newlines_and_stop_at_one_excess_byte() {
        use std::io::{BufReader, Read as _};
        let mut reader = BufReader::new(std::io::repeat(b' ').take(MAX_SEED_LINE_BYTES as u64));
        let mut bytes = Vec::new();
        let mut total = 0;
        assert_eq!(
            read_seed_line(&mut reader, &mut bytes, &mut total, 1).unwrap(),
            MAX_SEED_LINE_BYTES
        );
        let mut reader = std::io::Cursor::new(b"{}\nremaining");
        total = MAX_SEED_TOTAL_BYTES - 2;
        let error = read_seed_line(&mut reader, &mut bytes, &mut total, 2).unwrap_err();
        assert_eq!(reader.position(), 3);
        assert_eq!(
            error.downcast_ref::<SeedValidationLimitError>(),
            Some(&SeedValidationLimitError::TotalBytes { limit: MAX_SEED_TOTAL_BYTES })
        );
        let mut reader = std::io::Cursor::new(vec![b' '; MAX_SEED_LINE_BYTES + 10]);
        total = 0;
        let error = read_seed_line(&mut reader, &mut bytes, &mut total, 1).unwrap_err();
        assert_eq!(reader.position(), MAX_SEED_LINE_BYTES as u64 + 1);
        assert_eq!(bytes.len(), MAX_SEED_LINE_BYTES + 1);
        assert_eq!(
            error.downcast_ref::<SeedValidationLimitError>(),
            Some(&SeedValidationLimitError::LineBytes { line: 1, limit: MAX_SEED_LINE_BYTES })
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_seed_rejects_shared_links_and_mismatched_inventory_collisions() {
        let source = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        let seed = source.path().join(format!("time_{id}.jsonl"));
        std::fs::write(&seed, format!("{{\"type\":\"session\",\"id\":\"{id}\"}}\n")).unwrap();
        let sessions = managed.path().join("inventory");
        std::fs::create_dir(&sessions).unwrap();
        let target = sessions.join(seed.file_name().unwrap());
        std::os::unix::fs::symlink(&seed, &target).unwrap();
        assert!(stage(&seed, &sessions).is_err());
        assert!(std::fs::symlink_metadata(&target).unwrap().file_type().is_symlink());
        std::fs::remove_file(&target).unwrap();
        std::fs::hard_link(&seed, &target).unwrap();
        assert!(stage(&seed, &sessions).is_err());
        std::fs::remove_file(&target).unwrap();
        let collision = "{\"type\":\"session\",\"id\":\"other\"}\n";
        std::fs::write(&target, collision).unwrap();
        assert!(stage(&seed, &sessions).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), collision);
    }

    #[test]
    fn native_seed_one_seat_operation_cannot_record_a_second_outcome() {
        let store = crate::store::Store::open_memory("node").unwrap();
        let winner = receipt("agent/example", "one", &Outcome::Fresh);
        let loser = receipt(
            "agent/example",
            "one",
            &Outcome::Seeded {
                session_id: "native".into(),
            },
        );
        let recorded = store.append_claim(&winner).unwrap();
        if let Ok(replayed) = store.append_claim(&loser) {
            assert_eq!(replayed.id, recorded.id);
            assert_eq!(
                replayed.body.pointer("/fields/invocation_id"),
                winner.fields.get("invocation_id")
            );
            assert_ne!(
                replayed.body.pointer("/fields/invocation_id"),
                loser.fields.get("invocation_id")
            );
        }
        assert_eq!(
            store
                .claims_for(
                    &marker("agent/example"),
                    Some("custom.agent.first-native-launch")
                )
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn native_seed_receipt_identity_is_seat_scoped_and_outcome_is_typed() {
        let fresh = receipt("agent/example", "one", &Outcome::Fresh);
        let seeded = receipt(
            "agent/example",
            "two",
            &Outcome::Seeded {
                session_id: "native".into(),
            },
        );
        assert_eq!(fresh.subject, seeded.subject);
        assert_eq!(fresh.expected_subject, None);
        assert_eq!(fresh.idempotency_key, seeded.idempotency_key);
        assert_ne!(
            fresh.fields["invocation_id"],
            seeded.fields["invocation_id"]
        );
        assert_eq!(fresh.fields["outcome"], "fresh");
        assert_eq!(seeded.fields["outcome"], "seeded");
    }
}
