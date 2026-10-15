//! Model accounts: what an `account` declaration says, how a seat's harness block binds one, and
//! which account a pooled seat runs on.
//!
//! An account names a provider login. Until sekrets holds model logins, that login is a credential
//! directory the harness is launched with: `CLAUDE_CONFIG_DIR` for Claude Code, `CODEX_HOME` for
//! Codex. A seat binds one account (`account "ada/claude"`) or a pool (`account-pool "person/ada"`:
//! any of that person's accounts for the harness's provider). A seat with neither runs on the
//! harness's default login, so a fleet that declares no accounts launches exactly as before.
//!
//! Nothing here reads a credential. Only the directory's path is used.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The environment variable that points a harness at its login directory.
pub fn login_environment_name(driver: &str) -> Option<&'static str> {
    match driver {
        "claude" => Some("CLAUDE_CONFIG_DIR"),
        "codex" => Some("CODEX_HOME"),
        _ => None,
    }
}

/// The harness that signs in to `provider`. A provider that no st harness uses yet has none.
pub fn provider_driver(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" | "claude" => Some("claude"),
        "openai" | "codex" => Some("codex"),
        _ => None,
    }
}

/// One host's login directory for an account. A login without a host serves every host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Login {
    pub host: Option<String>,
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountDecl {
    /// The name after `account/`, as a harness block writes it.
    pub name: String,
    pub provider: String,
    pub owner: Option<String>,
    pub plan: Option<String>,
    /// `api-key` when the account pays per token with a key file, else a subscription login.
    pub auth_type: Option<String>,
    /// The harness that reads this account's key, when it is not the provider's default one.
    pub harness: Option<String>,
    /// What an `api-key` account may spend each calendar month, in whole dollars.
    pub monthly_credits_usd: Option<u64>,
    /// Provider labels (`claude/<digest>`) that name this account in older readings and in seats
    /// that were never launched on it. Opaque digests, never an email or a provider account ID.
    pub identities: Vec<String>,
    pub logins: Vec<Login>,
}

impl AccountDecl {
    pub fn subject(&self) -> String {
        format!("account/{}", self.name)
    }

    /// The harness this account is used by: the one it names, else its provider's own.
    pub fn driver(&self) -> Option<&str> {
        self.harness
            .as_deref()
            .or_else(|| provider_driver(&self.provider))
    }

    pub fn is_api_key(&self) -> bool {
        self.auth_type.as_deref() == Some("api-key")
    }

    /// The label this account's usage and limits carry once a seat is launched on it.
    pub fn label(&self) -> Option<String> {
        Some(st_drivers::account::account_label(
            self.driver()?,
            &format!("declared:{}", self.name),
        ))
    }

    /// The directory this account's login lives in on `host`: the login naming the host, else the
    /// login naming none.
    pub fn login_for(&self, host: &str) -> Option<&str> {
        self.logins
            .iter()
            .find(|login| login.host.as_deref() == Some(host))
            .or_else(|| self.logins.iter().find(|login| login.host.is_none()))
            .map(|login| login.path.as_str())
    }
}

fn child<'a>(node: &'a Value, name: &str) -> Option<&'a Value> {
    node.get("children")?
        .as_array()?
        .iter()
        .find(|child| child.get("name").and_then(Value::as_str) == Some(name))
}

fn first_argument(node: &Value) -> Option<&str> {
    node.get("arguments")?.as_array()?.first()?.as_str()
}

/// An account declaration, from its canonical desired body.
pub fn parse_account(subject: &str, desired: &Value) -> Option<AccountDecl> {
    let name = subject.strip_prefix("account/")?;
    let provider = first_argument(child(desired, "provider")?)?;
    let logins = desired
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|node| node.get("name").and_then(Value::as_str) == Some("login"))
        .filter_map(|node| {
            Some(Login {
                host: node
                    .pointer("/properties/host")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                path: first_argument(node)?.to_owned(),
            })
        })
        .collect();
    Some(AccountDecl {
        name: name.to_owned(),
        provider: provider.to_owned(),
        owner: child(desired, "owner")
            .and_then(first_argument)
            .map(str::to_owned),
        plan: child(desired, "plan")
            .and_then(first_argument)
            .map(str::to_owned),
        auth_type: child(desired, "auth-type")
            .and_then(first_argument)
            .map(str::to_owned),
        harness: child(desired, "harness")
            .and_then(first_argument)
            .map(str::to_owned),
        monthly_credits_usd: child(desired, "monthly-credits-usd")
            .and_then(|node| node.get("arguments")?.as_array()?.first())
            .and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
            }),
        identities: desired
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|node| node.get("name").and_then(Value::as_str) == Some("identity"))
            .filter_map(|node| first_argument(node).map(str::to_owned))
            .collect(),
        logins,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Binding {
    /// `account "NAME"`: exactly this account.
    Account(String),
    /// `account-pool "person/NAME"`: any account that person owns for the harness's provider.
    Pool(String),
}

/// A seat's account binding, with the harness that carries it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessBinding {
    pub driver: String,
    pub binding: Binding,
}

/// The binding in an agent's canonical desired body, if its harness block names one.
pub fn harness_binding(desired: &Value) -> Option<HarnessBinding> {
    let harness = child(desired, "harness")?;
    let driver = first_argument(harness)?;
    let binding = if let Some(node) = child(harness, "account") {
        Binding::Account(first_argument(node)?.to_owned())
    } else {
        Binding::Pool(first_argument(child(harness, "account-pool")?)?.to_owned())
    };
    Some(HarnessBinding {
        driver: driver.to_owned(),
        binding,
    })
}

/// `~/` stands for the home of the user the daemon runs as.
pub fn expand_login(path: &str, home: Option<&Path>) -> Option<PathBuf> {
    match path.strip_prefix("~/") {
        Some(rest) => Some(home?.join(rest)),
        None => Some(PathBuf::from(path)),
    }
}

/// The harness a canonical agent body declares, whether or not its block binds an account.
pub fn harness_driver(desired: &Value) -> Option<&str> {
    first_argument(child(desired, "harness")?)
}

/// The declared account whose login directory on `host` is `directory`. A seat that sets its
/// harness's login variable to a declared login runs on that account even when its block binds
/// nothing, so what it reports belongs to the account, whatever identity the provider shows.
pub fn account_by_login<'a>(
    accounts: &'a [AccountDecl],
    driver: &str,
    host: &str,
    directory: &Path,
    home: Option<&Path>,
) -> Option<&'a AccountDecl> {
    let same = |left: &Path, right: &Path| {
        left == right
            || matches!((left.canonicalize(), right.canonicalize()), (Ok(left), Ok(right)) if left == right)
    };
    accounts.iter().find(|account| {
        account.driver() == Some(driver)
            && account
                .login_for(host)
                .and_then(|login| expand_login(login, home))
                .is_some_and(|login| same(&login, directory))
    })
}

/// The login directory a harness uses when nothing sets its login variable.
pub fn default_login_directory(driver: &str, home: Option<&Path>) -> Option<PathBuf> {
    match driver {
        "claude" => Some(home?.join(".claude")),
        "codex" => Some(home?.join(".codex")),
        _ => None,
    }
}

/// The declared account an unbound seat runs on at `host`. A harness with a login directory runs
/// on the account whose login that is: the one its environment sets (`configured`), else the
/// harness's default. A harness that reads a key file (omp) has no directory to choose, so it runs
/// on the one api-key account that names it and has a login on this host.
pub fn unbound_login_account<'a>(
    accounts: &'a [AccountDecl],
    driver: &str,
    host: &str,
    configured: Option<&str>,
    home: Option<&Path>,
) -> Option<&'a AccountDecl> {
    if login_environment_name(driver).is_some() {
        let directory = match configured.filter(|directory| !directory.is_empty()) {
            Some(directory) => expand_login(directory, home)?,
            None => default_login_directory(driver, home)?,
        };
        return account_by_login(accounts, driver, host, &directory, home);
    }
    let mut keyed = accounts.iter().filter(|account| {
        account.is_api_key()
            && account.harness.as_deref() == Some(driver)
            && account.login_for(host).is_some()
    });
    let account = keyed.next()?;
    keyed.next().is_none().then_some(account)
}

/// What a pooled seat's choice reads of one account.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub name: String,
    pub weekly_percent: Option<f64>,
    pub five_hour_percent: Option<f64>,
}

/// The candidate with the most usage left: the lowest weekly percentage, then the lowest 5-hour
/// one, then the name. An account with no reading counts as unused, which is what a window that
/// has reset or an account no seat has used yet is.
pub fn most_usage_left(candidates: &[Candidate]) -> Option<&Candidate> {
    let used = |value: Option<f64>| value.filter(|value| value.is_finite()).unwrap_or(0.0);
    candidates.iter().min_by(|left, right| {
        used(left.weekly_percent)
            .total_cmp(&used(right.weekly_percent))
            .then_with(|| used(left.five_hour_percent).total_cmp(&used(right.five_hour_percent)))
            .then_with(|| left.name.cmp(&right.name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn account() -> Value {
        json!({
            "name": "account",
            "arguments": ["ada/claude-1"],
            "children": [
                {"name": "provider", "arguments": ["anthropic"]},
                {"name": "owner", "arguments": ["person/ada"]},
                {"name": "plan", "arguments": ["max"]},
                {"name": "login", "arguments": ["/srv/logins/ada-1"], "properties": {"host": "alder"}},
                {"name": "login", "arguments": ["~/.claude-ada-1"]},
            ],
        })
    }

    #[test]
    fn an_account_names_its_owner_plan_and_a_login_per_host() {
        let decl = parse_account("account/ada/claude-1", &account()).unwrap();
        assert_eq!(decl.subject(), "account/ada/claude-1");
        assert_eq!(decl.driver(), Some("claude"));
        assert_eq!(decl.owner.as_deref(), Some("person/ada"));
        assert_eq!(decl.plan.as_deref(), Some("max"));
        assert_eq!(decl.login_for("alder"), Some("/srv/logins/ada-1"));
        assert_eq!(decl.login_for("birch"), Some("~/.claude-ada-1"));
        assert_eq!(
            expand_login("~/.claude-ada-1", Some(Path::new("/srv/home-one"))),
            Some(PathBuf::from("/srv/home-one/.claude-ada-1"))
        );
        assert_eq!(expand_login("~/x", None), None);
    }

    #[test]
    fn a_login_directory_names_the_declared_account_that_owns_it_on_that_host() {
        let accounts = [parse_account("account/ada/claude-1", &account()).unwrap()];
        let find = |driver: &str, host: &str, directory: &str| {
            account_by_login(&accounts, driver, host, Path::new(directory), Some(Path::new("/srv/home-one")))
                .map(|account| account.name.as_str())
        };
        assert_eq!(find("claude", "alder", "/srv/logins/ada-1"), Some("ada/claude-1"));
        assert_eq!(find("claude", "birch", "/srv/home-one/.claude-ada-1/"), Some("ada/claude-1"));
        assert_eq!(find("claude", "birch", "/srv/logins/ada-1"), None, "another host's login");
        assert_eq!(find("codex", "alder", "/srv/logins/ada-1"), None, "another harness");
    }

    #[test]
    fn an_unbound_seat_runs_on_the_account_of_its_default_login_or_the_key_file_it_reads() {
        let declare = |name: &str, provider: &str, extra: Vec<Value>, login: Value| {
            let mut children = vec![
                json!({"name": "provider", "arguments": [provider]}),
                login,
            ];
            children.extend(extra);
            parse_account(
                &format!("account/{name}"),
                &json!({"name": "account", "arguments": [name], "children": children}),
            )
            .unwrap()
        };
        let login = |path: &str, host: &str| {
            json!({"name": "login", "arguments": [path], "properties": {"host": host}})
        };
        let accounts = [
            declare("ada/claude-1", "anthropic", vec![], login("~/.claude", "alder")),
            declare("ada/claude-2", "anthropic", vec![], login("~/.claude-two", "alder")),
            declare("ada/codex", "openai", vec![], login("~/.codex", "alder")),
            declare(
                "ada/key",
                "anthropic",
                vec![
                    json!({"name": "auth-type", "arguments": ["api-key"]}),
                    json!({"name": "harness", "arguments": ["omp"]}),
                    json!({"name": "monthly-credits-usd", "arguments": [200]}),
                    json!({"name": "identity", "arguments": ["claude/0123456789abcdef"]}),
                ],
                login("~/.config/anthropic/api-key", "alder"),
            ),
        ];
        let home = Some(Path::new("/srv/home"));
        let find = |driver: &str, host: &str, configured: Option<&str>| {
            unbound_login_account(&accounts, driver, host, configured, home)
                .map(|account| account.name.as_str())
        };
        assert_eq!(find("claude", "alder", None), Some("ada/claude-1"), "the default login");
        assert_eq!(find("claude", "alder", Some("/srv/home/.claude-two")), Some("ada/claude-2"));
        assert_eq!(find("claude", "alder", Some("")), Some("ada/claude-1"), "an empty variable is unset");
        assert_eq!(find("claude", "birch", None), None, "no login on that host");
        assert_eq!(find("codex", "alder", None), Some("ada/codex"));
        assert_eq!(find("omp", "alder", None), Some("ada/key"), "the key file its harness reads");
        assert_eq!(find("omp", "birch", None), None);
        assert_eq!(find("pi", "alder", None), None);

        let key = &accounts[3];
        assert!(key.is_api_key());
        assert_eq!(key.driver(), Some("omp"));
        assert_eq!(key.monthly_credits_usd, Some(200));
        assert_eq!(key.identities, ["claude/0123456789abcdef"]);
        assert_eq!(
            key.label().unwrap(),
            st_drivers::account::account_label("omp", "declared:ada/key")
        );
        assert!(!accounts[0].is_api_key());
    }

    #[test]
    fn a_harness_binds_one_account_or_a_pool_and_nothing_else_binds() {
        let agent = |body: Value| {
            json!({"name": "agent", "arguments": ["a"], "children": [
                {"name": "harness", "arguments": ["claude"], "children": [body]}
            ]})
        };
        assert_eq!(
            harness_binding(&agent(
                json!({"name": "account", "arguments": ["ada/claude-1"]})
            )),
            Some(HarnessBinding {
                driver: "claude".into(),
                binding: Binding::Account("ada/claude-1".into()),
            })
        );
        assert_eq!(
            harness_binding(&agent(
                json!({"name": "account-pool", "arguments": ["person/ada"]})
            ))
            .unwrap()
            .binding,
            Binding::Pool("person/ada".into())
        );
        assert_eq!(
            harness_binding(&agent(json!({"name": "model", "arguments": ["m"]}))),
            None
        );
    }

    #[test]
    fn the_pool_prefers_the_most_usage_left_and_treats_no_reading_as_unused() {
        let candidate = |name: &str, weekly: Option<f64>, five: Option<f64>| Candidate {
            name: name.into(),
            weekly_percent: weekly,
            five_hour_percent: five,
        };
        let pool = [
            candidate("ada/a", Some(60.0), Some(1.0)),
            candidate("ada/b", Some(20.0), Some(90.0)),
            candidate("ada/c", Some(20.0), Some(10.0)),
        ];
        assert_eq!(most_usage_left(&pool).unwrap().name, "ada/c");
        let with_unread = [pool[0].clone(), candidate("ada/z", None, None)];
        assert_eq!(most_usage_left(&with_unread).unwrap().name, "ada/z");
        assert!(most_usage_left(&[]).is_none());
    }
}
