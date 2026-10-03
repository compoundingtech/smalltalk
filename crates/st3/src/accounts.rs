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
    pub logins: Vec<Login>,
}

impl AccountDecl {
    pub fn subject(&self) -> String {
        format!("account/{}", self.name)
    }

    pub fn driver(&self) -> Option<&'static str> {
        provider_driver(&self.provider)
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
