//! Bind re-attestation to an independently fetched old job and a genuinely fresh run/attempt.
use super::{observers::Produced, *};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Opaque links: only construction against authenticated old/new job observations is allowed.
#[derive(Default)]
pub struct Links(BTreeMap<String, String>);

pub fn identity(repository: &str, runner: &str) -> Result<(u64, u64, u64)> {
    let prefix = format!("github:{repository}/actions/runs/");
    let pieces = runner
        .strip_prefix(&prefix)
        .context("predecessor repository/runner")?
        .split('/')
        .collect::<Vec<_>>();
    ensure!(
        pieces.len() == 5 && pieces[1] == "attempts" && pieces[3] == "jobs",
        "invalid CI runner identity"
    );
    let number = |value: &str| -> Result<u64> {
        let number = value.parse::<u64>()?;
        ensure!(
            number > 0 && number.to_string() == value,
            "invalid CI execution identity"
        );
        Ok(number)
    };
    Ok((number(pieces[0])?, number(pieces[2])?, number(pieces[4])?))
}

pub fn prior(repo: &Path, repository: &str, id: &str) -> Result<Statement> {
    let statement = show(repo, id)?;
    ensure!(
        statement.predicate.tier == TrustTier::Degraded,
        "re-attestation predecessor must be degraded"
    );
    // Degraded verification requires explicit absence of a signature and no trust file.
    verify_record(repo, id, Path::new("unused-for-unsigned-evidence"))?;
    let Fact::Tests { runner, checks, .. } = &statement.predicate.fact else {
        bail!("predecessor is not CI tests")
    };
    ensure!(
        checks.len() == 1,
        "CI re-attestation needs one predecessor per check"
    );
    identity(repository, runner)?;
    Ok(statement)
}

/// The transport supplies old job objects fetched through the same attempt-specific
/// endpoint as the original observer. No old output or timestamp is accepted on faith.
pub fn prepare(
    repo: &Path,
    repository: &str,
    current: &[Statement],
    jobs: &[Value],
    predecessors: &[(String, Value, Value)],
) -> Result<Links> {
    let mut links = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for (id, old_run_info, old_job) in predecessors {
        ensure!(seen.insert(id), "duplicate re-attestation selector");
        let old = prior(repo, repository, id)?;
        let Fact::Tests {
            runner: old_runner,
            checks: old_checks,
            inputs: old_inputs,
        } = &old.predicate.fact
        else {
            unreachable!()
        };
        let (old_run, old_attempt, old_id) = identity(repository, old_runner)?;
        ensure!(
            old_run_info["id"].as_u64() == Some(old_run)
                && old_run_info["run_attempt"].as_u64() == Some(old_attempt)
                && old_run_info["repository"]["full_name"].as_str() == Some(repository)
                && old_run_info["name"] == "Workspace CI"
                && old_run_info["path"] == ".github/workflows/fleet.yml"
                && old_run_info["status"] == "completed"
                && matches!(
                    old_run_info["event"].as_str(),
                    Some("pull_request" | "merge_group" | "workflow_dispatch")
                ),
            "predecessor run belongs to a different workflow/repository or is incomplete"
        );
        let (name, old_check) = old_checks.first_key_value().unwrap();
        ensure!(
            old_job["id"].as_u64() == Some(old_id)
                && old_job["run_id"].as_u64() == Some(old_run)
                && old_job["run_attempt"].as_u64() == Some(old_attempt)
                && old_job["name"].as_str() == Some(name)
                && old_job["status"] == "completed",
            "predecessor job identity/status mismatch"
        );
        ensure!(
            old_check.output_sha256 == hex::encode(Sha256::digest(serde_json::to_vec(old_job)?))
                && old_check.exit_code
                    == if old_job["conclusion"] == "success" {
                        0
                    } else {
                        1
                    },
            "predecessor differs from authenticated job observation"
        );
        let old_completed = chrono::DateTime::parse_from_rfc3339(
            old_job["completed_at"]
                .as_str()
                .context("old job completion time")?,
        )?;
        ensure!(
            old_completed == chrono::DateTime::parse_from_rfc3339(&old.predicate.observed_at)?,
            "predecessor timestamp differs from authenticated completion"
        );
        let applicable = current.iter().filter(|statement| {
            statement.predicate.change_id == old.predicate.change_id && statement.subject[0].digest == old.subject[0].digest
                && matches!(&statement.predicate.fact, Fact::Tests { checks, inputs, .. } if inputs == old_inputs
                    && checks.len() == 1 && checks.get(name).is_some_and(|check| check.command == old_check.command))
        }).collect::<Vec<_>>();
        ensure!(
            applicable.len() == 1,
            "predecessor lacks a unique identical keyed check/tree/input map"
        );
        let new = applicable[0];
        let Fact::Tests { runner, .. } = &new.predicate.fact else {
            unreachable!()
        };
        let (new_run, new_attempt, new_id) = identity(repository, runner)?;
        ensure!(
            (new_run, new_attempt) != (old_run, old_attempt) && new_id != old_id,
            "re-attestation requires a fresh run/attempt, never the original job"
        );
        let new_jobs = jobs
            .iter()
            .filter(|job| {
                job["id"].as_u64() == Some(new_id)
                    && job["run_id"].as_u64() == Some(new_run)
                    && job["run_attempt"].as_u64() == Some(new_attempt)
            })
            .collect::<Vec<_>>();
        ensure!(
            new_jobs.len() == 1,
            "missing or ambiguous fresh job observation"
        );
        let started = chrono::DateTime::parse_from_rfc3339(
            new_jobs[0]["started_at"]
                .as_str()
                .context("fresh job start time")?,
        )?;
        let completed = chrono::DateTime::parse_from_rfc3339(&new.predicate.observed_at)?;
        ensure!(
            started >= old_completed && completed >= started,
            "keyed rerun must start after degraded check completed"
        );
        ensure!(
            links.insert(runner.clone(), id.clone()).is_none(),
            "multiple predecessors for one current check"
        );
    }
    Ok(Links(links))
}

impl Links {
    pub fn for_runner(&self, runner: &str) -> Option<&String> {
        self.0.get(runner)
    }
}

/// Count only new full records. The signed note's issuance time includes observer
/// scheduling delay, unlike the rerun's earlier CI completion timestamp.
pub fn measurements(repo: &Path, produced: &[Produced]) -> Result<Vec<Value>> {
    let mut samples = Vec::new();
    for result in produced
        .iter()
        .filter(|result| result.newly_recorded && result.tier == TrustTier::Full)
    {
        let current = show(repo, &result.record)?;
        if let Some(id) = &current.predicate.re_attests {
            let old = show(repo, id)?;
            let start = chrono::DateTime::parse_from_rfc3339(&old.predicate.observed_at)?
                .timestamp_millis();
            let issued = git_text(repo, &["show", "-s", "--format=%ct", &result.record], None)?
                .parse::<i64>()?
                .checked_mul(1000)
                .context("issuance time overflow")?;
            let elapsed = elapsed_ms(start, issued);
            samples.push(serde_json::json!({
                "record":result.record,
                "prior":id,
                "elapsed_ms":elapsed,
                "skewed":elapsed.is_none()
            }));
        }
    }
    Ok(samples)
}

/// Milliseconds from the degraded completion to the signed note, or `None` when
/// the observer clock reads earlier than GitHub's completion time. The record is
/// already written by then, so a skewed clock marks the sample instead of failing
/// a valid observation.
fn elapsed_ms(start: i64, issued: i64) -> Option<i64> {
    issued.checked_sub(start).filter(|value| *value >= 0)
}

#[cfg(test)]
mod tests {
    use super::elapsed_ms;

    #[test]
    fn clock_skew_marks_the_sample_instead_of_failing() {
        assert_eq!(elapsed_ms(1_000, 4_500), Some(3_500));
        assert_eq!(elapsed_ms(1_000, 1_000), Some(0));
        assert_eq!(elapsed_ms(5_000, 1_000), None);
        assert_eq!(elapsed_ms(i64::MIN, 1), None);
    }
}
