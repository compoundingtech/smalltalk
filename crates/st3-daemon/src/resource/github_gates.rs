//! What the built-in `merged` and `ci-passed` gates read from GitHub: one pull request's merge
//! state, or one named check or commit status on a commit or branch head.
//!
//! Each question has three answers, as an exec gate does. A refusal to look at all, such as a
//! missing token, a repository st cannot see, or a pull request that closed unmerged, breaks the
//! gate; a rate limit or an unreachable GitHub only means not yet.

use std::time::Duration;

use serde_json::Value;

pub use crate::gate_kinds::GateAnswer;

use crate::github_http::GithubAuth;
use super::{
    GITHUB_AUTH_REMEDY, ProviderForbidden, ProviderRateLimit, ProviderUnauthenticated,
    github_api_base, github_client, github_json, github_auth,
};

/// Whether pull request `locator` (`OWNER/REPO#NUMBER`) has merged.
pub async fn pull_request_merged(locator: &str) -> GateAnswer {
    let token = match github_auth().await {
        Ok(token) => token,
        Err(error) => return GateAnswer::Broken(error.to_string()),
    };
    pull_request_merged_at(locator, &github_api_base(), Some(&token)).await
}

pub(crate) async fn pull_request_merged_at(
    locator: &str,
    api_base: &str,
    token: Option<&GithubAuth>,
) -> GateAnswer {
    let Some((repository, number)) = locator.rsplit_once('#').filter(|(repository, number)| {
        repository_parts(repository).is_some() && number.parse::<u64>().is_ok()
    }) else {
        return GateAnswer::Broken(format!(
            "`{locator}` is not a pull request; write OWNER/REPO#NUMBER"
        ));
    };
    let Some(token) = token.filter(|token| token.is_valid()) else {
        return GateAnswer::Broken(GITHUB_AUTH_REMEDY.into());
    };
    let url = format!("{api_base}/repos/{repository}/pulls/{number}");
    let pull = match github_json(&github_client(), url, token, Duration::ZERO).await {
        Ok(payload) => payload.value,
        Err(error) => return lookup_failure(&error, &format!("pull request {locator}")),
    };
    pull_request_answer(locator, &pull)
}

fn pull_request_answer(locator: &str, pull: &Value) -> GateAnswer {
    if pull["merged"] == true {
        let commit = pull["merge_commit_sha"]
            .as_str()
            .unwrap_or("an unknown commit");
        let at = pull["merged_at"].as_str().unwrap_or("an unknown time");
        return GateAnswer::Pass(format!("{locator} merged as {commit} at {at}"));
    }
    if pull["state"] == "closed" {
        return GateAnswer::Broken(format!(
            "{locator} closed without merging, so the gate cannot pass; reopen it or revise the gate"
        ));
    }
    let draft = if pull["draft"] == true { " draft" } else { "" };
    GateAnswer::NotYet(format!("{locator} is an open{draft} pull request"))
}

/// Whether the check run or commit status named `check` passed on `reference`, a commit or a
/// branch of `repository` (`OWNER/REPO`).
pub async fn check_passed(repository: &str, reference: &str, check: &str) -> GateAnswer {
    let token = match github_auth().await {
        Ok(token) => token,
        Err(error) => return GateAnswer::Broken(error.to_string()),
    };
    check_passed_at(repository, reference, check, &github_api_base(), Some(&token)).await
}

pub(crate) async fn check_passed_at(
    repository: &str,
    reference: &str,
    check: &str,
    api_base: &str,
    token: Option<&GithubAuth>,
) -> GateAnswer {
    if repository_parts(repository).is_none() {
        return GateAnswer::Broken(format!(
            "`{repository}` is not a repository; write OWNER/REPO"
        ));
    }
    let Some(token) = token.filter(|token| token.is_valid()) else {
        return GateAnswer::Broken(GITHUB_AUTH_REMEDY.into());
    };
    let client = github_client();
    let base = format!("{api_base}/repos/{repository}");
    let reference_path = urlencoding::encode(reference);
    let commit = match github_json(
        &client,
        format!("{base}/commits/{reference_path}"),
        token,
        Duration::ZERO,
    )
    .await
    {
        Ok(payload) => payload.value,
        Err(error) if http_status(&error).is_some_and(|status| matches!(status, 404 | 422)) => {
            // A missing commit or branch may still be pushed; a missing repository never answers.
            return match github_json(&client, base.clone(), token, Duration::ZERO).await {
                Ok(_) => GateAnswer::NotYet(format!("{repository} has no `{reference}` yet")),
                Err(error) => lookup_failure(&error, &format!("repository {repository}")),
            };
        }
        Err(error) => return lookup_failure(&error, &format!("`{reference}` in {repository}")),
    };
    let Some(sha) = commit["sha"].as_str().map(str::to_owned) else {
        return GateAnswer::NotYet(format!("GitHub named no commit for `{reference}`"));
    };
    let check_query = urlencoding::encode(check);
    let runs = match github_json(
        &client,
        format!("{base}/commits/{sha}/check-runs?check_name={check_query}&per_page=100"),
        token,
        Duration::ZERO,
    )
    .await
    {
        Ok(payload) => payload.value,
        Err(error) => return lookup_failure(&error, &format!("check runs on {sha}")),
    };
    let statuses = match github_json(
        &client,
        format!("{base}/commits/{sha}/status?per_page=100"),
        token,
        Duration::ZERO,
    )
    .await
    {
        Ok(payload) => payload.value,
        Err(error) => return lookup_failure(&error, &format!("statuses on {sha}")),
    };
    check_answer(reference, &sha, check, &runs, &statuses)
}

/// The answer for `check` from a commit's check runs and combined status. A success in either
/// passes; otherwise the newest check run, then the status, says what is still missing.
fn check_answer(
    reference: &str,
    sha: &str,
    check: &str,
    runs: &Value,
    statuses: &Value,
) -> GateAnswer {
    let short = &sha[..sha.len().min(12)];
    let on = if reference == sha || sha.starts_with(reference) {
        short.to_owned()
    } else {
        format!("`{reference}` ({short})")
    };
    let mut runs = runs["check_runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|run| run["name"] == check)
        .collect::<Vec<_>>();
    runs.sort_by_key(|run| run["id"].as_u64().unwrap_or_default());
    let status = statuses["statuses"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|status| status["context"] == check);
    let run_passed = runs
        .last()
        .is_some_and(|run| run["status"] == "completed" && run["conclusion"] == "success");
    if run_passed || status.is_some_and(|status| status["state"] == "success") {
        return GateAnswer::Pass(format!("{check} passed on {on}"));
    }
    if let Some(run) = runs.last() {
        return GateAnswer::NotYet(if run["status"] == "completed" {
            let conclusion = run["conclusion"].as_str().unwrap_or("no conclusion");
            format!("{check} finished with {conclusion} on {on}")
        } else {
            let state = run["status"]
                .as_str()
                .unwrap_or("pending")
                .replace('_', " ");
            format!("{check} is {state} on {on}")
        });
    }
    if let Some(status) = status {
        let state = status["state"].as_str().unwrap_or("pending");
        return GateAnswer::NotYet(format!("{check} reports {state} on {on}"));
    }
    GateAnswer::NotYet(format!(
        "no check or status named {check} has reported on {on} yet"
    ))
}

fn repository_parts(repository: &str) -> Option<(&str, &str)> {
    let (owner, name) = repository.split_once('/')?;
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (valid(owner) && valid(name)).then_some((owner, name))
}

fn http_status(error: &anyhow::Error) -> Option<u16> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .and_then(reqwest::Error::status)
        .map(|status| status.as_u16())
}

/// A failed lookup: no access or no such thing breaks the gate; anything passing is not yet.
fn lookup_failure(error: &anyhow::Error, what: &str) -> GateAnswer {
    if error.downcast_ref::<ProviderRateLimit>().is_some() {
        return GateAnswer::NotYet(format!("GitHub rate-limited the read of {what}"));
    }
    if let Some(forbidden) = error.downcast_ref::<ProviderForbidden>() {
        return GateAnswer::Broken(format!(
            "GitHub refused to show {what}: {}",
            forbidden.message
        ));
    }
    if error.downcast_ref::<ProviderUnauthenticated>().is_some() {
        return GateAnswer::Broken(format!(
            "GitHub did not accept st's token for {what}: {GITHUB_AUTH_REMEDY}"
        ));
    }
    match http_status(error) {
        Some(404 | 410) => {
            GateAnswer::Broken(format!("GitHub has no {what}, or st's token cannot see it"))
        }
        Some(status) if (400..500).contains(&status) => {
            GateAnswer::Broken(format!("GitHub refused the read of {what} (HTTP {status})"))
        }
        _ => GateAnswer::NotYet(format!("GitHub did not answer for {what}: {error:#}")),
    }
}

#[cfg(test)]
mod tests {
    use crate::github_http::GithubAuth;
    use super::{
        GateAnswer, check_answer, check_passed_at, pull_request_answer, pull_request_merged_at,
    };
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    /// A GitHub stand-in that answers each path with a fixed status and body.
    async fn github(routes: BTreeMap<&'static str, (u16, Value)>) -> String {
        let routes = std::sync::Arc::new(routes);
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let routes = routes.clone();
            async move {
                let (status, body) = routes
                    .get(uri.path())
                    .cloned()
                    .unwrap_or((404, json!({"message": "Not Found"})));
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    axum::Json(body),
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        base
    }

    #[tokio::test]
    async fn lookups_pass_wait_or_break_as_github_answers() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let base = github(BTreeMap::from([
            (
                "/repos/acme/app/pulls/42",
                (200, json!({"state": "closed", "merged": true, "merge_commit_sha": sha})),
            ),
            (
                "/repos/acme/app/pulls/43",
                (200, json!({"state": "open", "merged": false})),
            ),
            ("/repos/acme/app", (200, json!({"full_name": "acme/app"}))),
            ("/repos/acme/app/commits/main", (200, json!({"sha": sha}))),
            (
                "/repos/acme/app/commits/0123456789abcdef0123456789abcdef01234567/check-runs",
                (200, json!({"check_runs": [
                    {"id": 7, "name": "linux-gate", "status": "completed", "conclusion": "success"}
                ]})),
            ),
            (
                "/repos/acme/app/commits/0123456789abcdef0123456789abcdef01234567/status",
                (200, json!({"statuses": []})),
            ),
        ]))
        .await;
        let auth = GithubAuth::test("test-token");
        let token = Some(&auth);
        assert!(matches!(
            pull_request_merged_at("acme/app#42", &base, token).await,
            GateAnswer::Pass(reason) if reason.contains(sha)
        ));
        assert!(matches!(
            pull_request_merged_at("acme/app#43", &base, token).await,
            GateAnswer::NotYet(_)
        ));
        assert!(matches!(
            pull_request_merged_at("acme/app#44", &base, token).await,
            GateAnswer::Broken(reason) if reason.contains("GitHub has no pull request acme/app#44")
        ));
        assert!(matches!(
            pull_request_merged_at("acme/app#42", &base, None).await,
            GateAnswer::Broken(reason) if reason.contains("no token")
        ));
        assert!(matches!(
            check_passed_at("acme/app", "main", "linux-gate", &base, token).await,
            GateAnswer::Pass(reason) if reason == "linux-gate passed on `main` (0123456789ab)"
        ));
        // A branch not pushed yet may still come; a repository st cannot see never answers.
        assert!(matches!(
            check_passed_at("acme/app", "release", "linux-gate", &base, token).await,
            GateAnswer::NotYet(reason) if reason == "acme/app has no `release` yet"
        ));
        assert!(matches!(
            check_passed_at("acme/missing", "main", "linux-gate", &base, token).await,
            GateAnswer::Broken(reason) if reason.contains("repository acme/missing")
        ));
    }

    #[test]
    fn a_pull_request_passes_once_merged_and_breaks_when_closed_unmerged() {
        let locator = "acme/app#42";
        assert!(matches!(
            pull_request_answer(locator, &json!({"state": "open", "merged": false, "draft": true})),
            GateAnswer::NotYet(reason) if reason == "acme/app#42 is an open draft pull request"
        ));
        assert!(matches!(
            pull_request_answer(locator, &json!({
                "state": "closed", "merged": true,
                "merge_commit_sha": "abc123", "merged_at": "2026-10-03T08:00:00Z"
            })),
            GateAnswer::Pass(reason) if reason.contains("merged as abc123")
        ));
        assert!(matches!(
            pull_request_answer(locator, &json!({"state": "closed", "merged": false})),
            GateAnswer::Broken(reason) if reason.contains("closed without merging")
        ));
    }

    #[test]
    fn a_check_passes_on_a_successful_run_or_status_and_otherwise_says_what_it_waits_for() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let none = json!({"check_runs": [], "statuses": []});
        let runs = |status: &str, conclusion: Option<&str>| {
            json!({"check_runs": [
                {"id": 1, "name": "linux-gate", "status": "completed", "conclusion": "failure"},
                {"id": 2, "name": "linux-gate", "status": status, "conclusion": conclusion},
                {"id": 3, "name": "other", "status": "completed", "conclusion": "success"}
            ]})
        };
        assert_eq!(
            check_answer(
                "main",
                sha,
                "linux-gate",
                &runs("completed", Some("success")),
                &none
            ),
            GateAnswer::Pass("linux-gate passed on `main` (0123456789ab)".into())
        );
        assert_eq!(
            check_answer("main", sha, "linux-gate", &runs("in_progress", None), &none),
            GateAnswer::NotYet("linux-gate is in progress on `main` (0123456789ab)".into())
        );
        assert_eq!(
            check_answer(
                sha,
                sha,
                "linux-gate",
                &runs("completed", Some("failure")),
                &none
            ),
            GateAnswer::NotYet("linux-gate finished with failure on 0123456789ab".into())
        );
        let statuses = json!({"statuses": [{"context": "st/ci", "state": "success"}]});
        assert_eq!(
            check_answer(sha, sha, "st/ci", &none, &statuses),
            GateAnswer::Pass("st/ci passed on 0123456789ab".into())
        );
        assert!(matches!(
            check_answer(sha, sha, "st/ci", &none, &none),
            GateAnswer::NotYet(reason) if reason.contains("no check or status named st/ci")
        ));
    }
}
