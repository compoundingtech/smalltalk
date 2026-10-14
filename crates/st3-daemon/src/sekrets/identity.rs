//! Who is calling the gateway, checked three ways:
//!
//! 1. The kernel: the socket's peer credentials give the caller's Unix user and process, and the
//!    process's cgroup says where it runs. Root's gateway configuration maps each Unix user to a
//!    person. A process in that user's login session scope (`session-N.scope`, made by logind) is
//!    the person: an unprivileged process cannot move itself into one.
//! 2. The daemon: a process in the user's service manager may be a seat. The person's daemon
//!    signs, with its node key, which agent runs in that process's cgroup, for this process and a
//!    nonce the gateway issued. The person registers that node key from a login session.
//! 3. The chain: the attestation names the person the agent works for, which must be the person
//!    the Unix user maps to.
//!
//! The agent's own word counts for nothing. While seats run as their person's Unix user, a seat
//! that reads the node key or moves itself into another seat's cgroup can pass for another seat
//! of the same person; it can never pass for the person or for anyone else's seat. Seats under
//! their own Unix user (plan phase 8) close that.

use std::fs;

use serde::{Deserialize, Serialize};

/// What a daemon signs about one process.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Statement {
    pub version: u32,
    /// The node whose daemon signed, `host/NAME`.
    pub node: String,
    pub person: String,
    pub agent: String,
    /// The declaration revision the seat runs, when the daemon knows it.
    #[serde(default)]
    pub revision: Option<String>,
    pub cgroup: String,
    pub pid: i32,
    /// The process's start time in clock ticks since boot, so a reused pid does not match.
    pub pid_start: u64,
    pub nonce: String,
    pub issued_at_unix_ms: i64,
}

pub const STATEMENT_VERSION: u32 = 1;
const ATTESTATION_DOMAIN: &str = "st.sekrets.attestation.v1";
/// How long an attestation counts after the daemon signs it.
pub const ATTESTATION_WINDOW_MS: i64 = 60_000;

pub fn signing_message(statement: &str) -> Vec<u8> {
    format!("{ATTESTATION_DOMAIN}\n{statement}").into_bytes()
}

/// The cgroup v2 path of a process, such as `/user.slice/user-1000.slice/session-4.scope`.
pub fn process_cgroup(pid: i32) -> Option<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::to_owned)
}

/// A process's start time in clock ticks since boot: field 22 of `/proc/PID/stat`.
pub fn process_start(pid: i32) -> Option<u64> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, after) = text.rsplit_once(") ")?;
    after.split_whitespace().nth(19)?.parse().ok()
}

/// Where a process of Unix user `uid` runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    /// A login session logind made: the person at a terminal or over ssh.
    Session,
    /// Somewhere in the user's own service manager, where seats run.
    UserManager,
    Elsewhere,
}

pub fn placement(uid: u32, cgroup: &str) -> Placement {
    let slice = format!("/user.slice/user-{uid}.slice/");
    let Some(rest) = cgroup.strip_prefix(&slice) else {
        return Placement::Elsewhere;
    };
    if let Some(scope) = rest.strip_prefix("session-")
        && let Some(name) = scope.strip_suffix(".scope")
        && !name.is_empty()
        && !name.contains('/')
    {
        return Placement::Session;
    }
    if rest.starts_with(&format!("user@{uid}.service/")) {
        return Placement::UserManager;
    }
    Placement::Elsewhere
}

/// Whether the signature verifies and the statement is about this process, now, for this nonce.
pub fn check_statement(
    statement: &Statement,
    pid: i32,
    pid_start: Option<u64>,
    cgroup: &str,
    nonce: &str,
    now_unix_ms: i64,
) -> Result<(), String> {
    if statement.version != STATEMENT_VERSION {
        return Err(format!(
            "attestation version {} is not {STATEMENT_VERSION}",
            statement.version
        ));
    }
    if statement.nonce != nonce {
        return Err("attestation is for another request".into());
    }
    if statement.pid != pid || Some(statement.pid_start) != pid_start {
        return Err("attestation is for another process".into());
    }
    if statement.cgroup != cgroup {
        return Err(format!(
            "attestation names cgroup {} but the caller runs in {cgroup}",
            statement.cgroup
        ));
    }
    if (now_unix_ms - statement.issued_at_unix_ms).abs() > ATTESTATION_WINDOW_MS {
        return Err("attestation is stale".into());
    }
    if !(statement.agent.starts_with("agent/") || statement.agent.starts_with("host/")) {
        return Err(format!(
            "attestation names {}, not an agent or a node",
            statement.agent
        ));
    }
    Ok(())
}

/// Whether `principal` matches a grant pattern: `*` matches within one path segment and `**`
/// matches any number of segments.
pub fn principal_matches(pattern: &str, principal: &str) -> bool {
    fn segments(pattern: &[&str], principal: &[&str]) -> bool {
        match (pattern.first(), principal.first()) {
            (None, None) => true,
            (Some(&"**"), _) => {
                segments(&pattern[1..], principal)
                    || (!principal.is_empty() && segments(pattern, &principal[1..]))
            }
            (Some(word), Some(part)) => {
                within(word.as_bytes(), part.as_bytes()) && segments(&pattern[1..], &principal[1..])
            }
            _ => false,
        }
    }
    fn within(pattern: &[u8], text: &[u8]) -> bool {
        match pattern.first() {
            None => text.is_empty(),
            Some(b'*') => (0..=text.len()).any(|skip| within(&pattern[1..], &text[skip..])),
            Some(byte) => text.first() == Some(byte) && within(&pattern[1..], &text[1..]),
        }
    }
    let pattern = pattern.split('/').collect::<Vec<_>>();
    let principal = principal.split('/').collect::<Vec<_>>();
    segments(&pattern, &principal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_tells_login_sessions_from_the_user_manager() {
        assert_eq!(
            placement(1000, "/user.slice/user-1000.slice/session-4.scope"),
            Placement::Session
        );
        assert_eq!(
            placement(1000, "/user.slice/user-1000.slice/session-c12.scope"),
            Placement::Session
        );
        assert_eq!(
            placement(
                1000,
                "/user.slice/user-1000.slice/user@1000.service/app.slice/st3-x-1-2.scope"
            ),
            Placement::UserManager
        );
        assert_eq!(
            placement(1000, "/user.slice/user-1001.slice/session-4.scope"),
            Placement::Elsewhere
        );
        assert_eq!(
            placement(1000, "/user.slice/user-1000.slice/session-4.scope/child"),
            Placement::Elsewhere
        );
        assert_eq!(
            placement(1000, "/system.slice/sshd.service"),
            Placement::Elsewhere
        );
    }

    #[test]
    fn this_process_has_a_cgroup_and_a_start_time() {
        let pid = std::process::id() as i32;
        assert!(process_start(pid).is_some());
        if std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
            assert!(process_cgroup(pid).is_some());
        }
    }

    #[test]
    fn statements_bind_process_nonce_cgroup_and_time() {
        let statement = Statement {
            version: STATEMENT_VERSION,
            node: "host/example".into(),
            person: "person/ada".into(),
            agent: "agent/fleet/fixture-web/builder".into(),
            revision: None,
            cgroup: "/a".into(),
            pid: 7,
            pid_start: 99,
            nonce: "n".into(),
            issued_at_unix_ms: 1_000_000,
        };
        assert!(check_statement(&statement, 7, Some(99), "/a", "n", 1_000_500).is_ok());
        assert!(check_statement(&statement, 8, Some(99), "/a", "n", 1_000_500).is_err());
        assert!(check_statement(&statement, 7, Some(98), "/a", "n", 1_000_500).is_err());
        assert!(check_statement(&statement, 7, Some(99), "/b", "n", 1_000_500).is_err());
        assert!(check_statement(&statement, 7, Some(99), "/a", "m", 1_000_500).is_err());
        assert!(check_statement(&statement, 7, Some(99), "/a", "n", 1_100_000).is_err());
        let person = Statement {
            agent: "person/ada".into(),
            ..statement
        };
        assert!(check_statement(&person, 7, Some(99), "/a", "n", 1_000_500).is_err());
    }

    #[test]
    fn grant_patterns_match_segments() {
        assert!(principal_matches(
            "agent/fleet/fixture-web/**",
            "agent/fleet/fixture-web/team/builder"
        ));
        assert!(principal_matches(
            "agent/fleet/fixture-web/*/builder",
            "agent/fleet/fixture-web/team/builder"
        ));
        assert!(!principal_matches(
            "agent/fleet/fixture-web/*",
            "agent/fleet/fixture-web/team/builder"
        ));
        assert!(principal_matches(
            "agent/fleet/fixture-web/team/build*",
            "agent/fleet/fixture-web/team/builder"
        ));
        assert!(!principal_matches(
            "agent/fleet/fixture-web/**",
            "person/ada"
        ));
        assert!(principal_matches("person/ada", "person/ada"));
        assert!(!principal_matches("person/ada", "person/alex"));
    }
}
