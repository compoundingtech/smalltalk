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

async fn prior(client: &crate::client::Client, subject: &str) -> Result<bool> {
    let page: ClaimsPage = client
        .get(&format!(
            "/v1/claims?subject={}&limit=1",
            urlencoding::encode(&marker(subject))
        ))
        .await?;
    Ok(!page.claims.is_empty())
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
/// Any receipt without a native binding is explicitly incomplete, including a fresh launch.
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
    anyhow::ensure!(
        !prior(client, subject).await?,
        "first-native-launch-incomplete: a durable outcome exists without a native binding; refusing to reseed or silently start another first launch"
    );
    let outcome = match seed.filter(|_| !deliberately_fresh) {
        Some(seed) => Outcome::Seeded {
            session_id: stage(seed, sessions)?,
        },
        None => Outcome::Fresh,
    };
    let input = receipt(subject, incarnation, &outcome);
    match client.post::<_, ClaimRecord>("/v1/claims", &input).await {
        Ok(record) => anyhow::ensure!(
            record.body.pointer("/fields/invocation_id") == input.fields.get("invocation_id"),
            "first-native-launch-incomplete: another invocation already recorded the first launch outcome; refusing to spawn"
        ),
        Err(error) => {
            if prior(client, subject).await? {
                anyhow::bail!(
                    "first-native-launch-incomplete: another invocation already recorded the first launch outcome; refusing to spawn"
                );
            }
            return Err(error);
        }
    }
    Ok(match outcome {
        Outcome::Fresh => None,
        Outcome::Seeded { session_id } => Some(session_id),
    })
}

/// Stage only the selected owner-local transcript, never its parent directory.
pub fn stage(seed: &Path, sessions: &Path) -> Result<String> {
    use std::io::{BufRead as _, BufReader};
    anyhow::ensure!(seed.is_absolute(), "seed transcript path must be absolute");
    let file = std::fs::File::open(seed).context("open seed transcript")?;
    let metadata = file.metadata()?;
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
    let mut header = None;
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let value: Value = serde_json::from_str(&line?).context("corrupt seed transcript")?;
        if index < 2 && value["type"] == "session" {
            header = value["id"].as_str().map(str::to_owned);
        }
    }
    anyhow::ensure!(
        header.as_deref() == Some(id),
        "seed filename and session header disagree"
    );
    match std::fs::symlink_metadata(sessions) {
        Ok(metadata) => anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "seed inventory must be a managed directory, not a link"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(sessions)?
        }
        Err(error) => return Err(error.into()),
    }
    let target = sessions.join(name);
    // An exclusive file link exposes only this transcript and works across local filesystems.
    match std::os::unix::fs::symlink(std::fs::canonicalize(seed)?, &target) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                let staged = std::fs::metadata(&target)?;
                anyhow::ensure!(
                    staged.dev() == metadata.dev() && staged.ino() == metadata.ino(),
                    "seed inventory filename collision"
                );
            }
            #[cfg(not(unix))]
            return Err(error.into());
        }
        Err(error) => return Err(error).context("stage seed transcript"),
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
    fn native_seed_validates_before_staging_and_links_only_one_transcript() {
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
