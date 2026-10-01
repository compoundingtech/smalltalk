//! The paying account behind a harness, as an opaque and stable label.
//!
//! Usage is attributed to the account that paid for each response, at the time of the response.
//! The label is `PROVIDER/` plus a short digest of the provider's own account identity, so the same
//! account reads the same on every host and harness version while the graph, the OpenTelemetry
//! export and the CLI never carry an email address or a provider account ID. A harness that does
//! not expose its account leaves responses without one; st reports those as an unknown account
//! instead of guessing.

use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

/// `PROVIDER/` and the first 16 hex digits of a domain-separated digest of `identity`.
pub fn account_label(provider: &str, identity: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"st.account.v1\0");
    hash.update(provider.as_bytes());
    hash.update(b"\0");
    hash.update(identity.as_bytes());
    let digest = hash.finalize();
    let hex = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{provider}/{hex}")
}

/// The Claude account the selected Claude config is signed in to: its organization and account
/// UUIDs from `oauthAccount`. Nothing else in the config is kept. `None` when the config is
/// unreadable or names no OAuth account (for example an API-key login).
pub fn claude_account() -> Option<String> {
    claude_account_at(&crate::pretrust::config_path().ok()?)
}

pub fn claude_account_at(config: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct Config {
        #[serde(rename = "oauthAccount")]
        oauth_account: Option<OauthAccount>,
    }
    #[derive(Deserialize)]
    struct OauthAccount {
        #[serde(rename = "accountUuid")]
        account_uuid: Option<String>,
        #[serde(rename = "organizationUuid")]
        organization_uuid: Option<String>,
    }
    let bytes = fs::read(config).ok()?;
    let account = serde_json::from_slice::<Config>(&bytes)
        .ok()?
        .oauth_account?;
    let account_uuid = account.account_uuid.filter(|value| !value.is_empty())?;
    let organization = account.organization_uuid.unwrap_or_default();
    Some(account_label(
        "claude",
        &format!("{organization}:{account_uuid}"),
    ))
}

/// [`claude_account`], remembered in `agent_dir` against the config's size and modification time.
/// Claude's config is large and every hook process would otherwise parse it again.
pub fn claude_account_cached(agent_dir: &Path) -> Option<String> {
    let config = crate::pretrust::config_path().ok()?;
    claude_account_cached_at(agent_dir, &config)
}

pub fn claude_account_cached_at(agent_dir: &Path, config: &Path) -> Option<String> {
    #[derive(Deserialize, Serialize, PartialEq)]
    struct Cached {
        config: String,
        length: u64,
        modified_ns: u128,
        account: Option<String>,
    }
    let metadata = fs::metadata(config).ok()?;
    let key = Cached {
        config: config.to_string_lossy().into_owned(),
        length: metadata.len(),
        modified_ns: metadata
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos(),
        account: None,
    };
    let cache = agent_dir.join(".harness-account");
    if let Some(cached) = fs::read(&cache)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Cached>(&bytes).ok())
        .filter(|cached| {
            cached.config == key.config
                && cached.length == key.length
                && cached.modified_ns == key.modified_ns
        })
    {
        return cached.account;
    }
    let account = claude_account_at(config);
    let fresh = Cached { account, ..key };
    if let Ok(bytes) = serde_json::to_vec(&fresh) {
        let _ = fs::write(&cache, bytes);
    }
    fresh.account
}

/// The Codex account from an app-server `account/read` result. A ChatGPT login is identified by
/// its email and an API-key login by its kind alone, since the key itself is never exposed.
pub fn codex_account_from_read(result: &Value) -> Option<String> {
    let account = result.get("account")?;
    match account.get("type").and_then(Value::as_str)? {
        "chatgpt" => {
            let email = account.get("email").and_then(Value::as_str)?;
            (!email.is_empty()).then(|| account_label("codex", &email.to_lowercase()))
        }
        "apiKey" => Some(account_label("codex", "api-key")),
        other => Some(account_label("codex", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn labels_are_opaque_stable_and_provider_scoped() {
        let label = account_label("claude", "org-a:account-a");
        assert_eq!(label, account_label("claude", "org-a:account-a"));
        assert!(label.starts_with("claude/"));
        assert_eq!(label.len(), "claude/".len() + 16);
        assert!(!label.contains("account-a"));
        assert_ne!(label, account_label("claude", "org-a:account-b"));
        assert_ne!(
            label.trim_start_matches("claude/"),
            account_label("codex", "org-a:account-a").trim_start_matches("codex/")
        );
    }

    #[test]
    fn claude_account_reads_only_the_oauth_identity_and_follows_config_changes() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join(".claude.json");
        let agent_dir = directory.path().join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(
            &config,
            json!({"projects": {}, "oauthAccount": {"accountUuid": "account-a", "organizationUuid": "org-a", "emailAddress": "person@example.com"}}).to_string(),
        )
        .unwrap();
        let first = claude_account_cached_at(&agent_dir, &config).unwrap();
        assert_eq!(first, account_label("claude", "org-a:account-a"));
        let cached = fs::read_to_string(agent_dir.join(".harness-account")).unwrap();
        assert!(
            !cached.contains("example.com"),
            "the cache keeps only the label"
        );

        fs::write(
            &config,
            json!({"oauthAccount": {"accountUuid": "account-b", "organizationUuid": "org-a"}, "padding": "changed length"}).to_string(),
        )
        .unwrap();
        assert_eq!(
            claude_account_cached_at(&agent_dir, &config).unwrap(),
            account_label("claude", "org-a:account-b")
        );

        fs::write(&config, json!({"primaryApiKey": "unused"}).to_string()).unwrap();
        assert_eq!(claude_account_cached_at(&agent_dir, &config), None);
    }

    #[test]
    fn codex_account_uses_the_login_identity_never_a_secret() {
        let chatgpt = json!({"account": {"type": "chatgpt", "email": "Person@Example.com", "planType": "pro"}, "requiresOpenaiAuth": true});
        assert_eq!(
            codex_account_from_read(&chatgpt).unwrap(),
            account_label("codex", "person@example.com")
        );
        assert_eq!(
            codex_account_from_read(&json!({"account": {"type": "apiKey"}})).unwrap(),
            account_label("codex", "api-key")
        );
        assert_eq!(codex_account_from_read(&json!({"account": null})), None);
    }
}
