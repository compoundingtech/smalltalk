//! Summaries of the append-only command logs. A report may combine logs copied from several hosts.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const SCHEMA: &str = "st3.recorder.command.v1";

#[derive(Debug, Deserialize)]
struct Call {
    schema: String,
    time: DateTime<Utc>,
    host: String,
    actor: String,
    program: String,
    args: Vec<String>,
    exit_code: Option<i32>,
    signal: Option<i32>,
    duration_ms: f64,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Count {
    pub host: String,
    pub actor: String,
    pub command: String,
    pub calls: u64,
    pub failures: u64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct AgentCount {
    pub actor: String,
    pub calls: u64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct AgentCommandCount {
    pub actor: String,
    pub command: String,
    pub calls: u64,
    pub failures: u64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct SlowCall {
    pub time: DateTime<Utc>,
    pub host: String,
    pub actor: String,
    pub command: String,
    pub duration_ms: f64,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    pub files: Vec<PathBuf>,
    pub calls: u64,
    pub skipped_lines: u64,
    pub by_agent_command: Vec<AgentCommandCount>,
    pub by_host_actor_command: Vec<Count>,
    /// An inference from gh's command name, not a count of HTTP requests.
    pub gh_api_candidates_by_agent: Vec<AgentCount>,
    pub failures: Vec<Count>,
    pub slowest: Vec<SlowCall>,
}

pub fn summarize(
    files: Vec<PathBuf>,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    limit: usize,
) -> Result<Report> {
    let mut counts = BTreeMap::<(String, String, String), (u64, u64)>::new();
    let mut agent_counts = BTreeMap::<(String, String), (u64, u64)>::new();
    let mut gh_api = BTreeMap::<String, u64>::new();
    let mut slowest = Vec::new();
    let mut calls = 0;
    let mut skipped_lines = 0;
    for path in &files {
        let file =
            File::open(path).with_context(|| format!("open command log {}", path.display()))?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut index = 0;
        loop {
            line.clear();
            let read = reader
                .read_until(b'\n', &mut line)
                .with_context(|| format!("read {} line {}", path.display(), index + 1))?;
            if read == 0 {
                break;
            }
            index += 1;
            let Ok(call) = serde_json::from_slice::<Call>(&line) else {
                skipped_lines += 1;
                continue;
            };
            if call.schema != SCHEMA || call.time < since || call.time > until {
                continue;
            }
            calls += 1;
            let command = command_name(&call.program, &call.args);
            let failed = call.exit_code != Some(0) || call.signal.is_some();
            let count = counts
                .entry((call.host.clone(), call.actor.clone(), command.clone()))
                .or_default();
            count.0 += 1;
            count.1 += if failed { 1 } else { 0 };
            let agent_count = agent_counts
                .entry((call.actor.clone(), command.clone()))
                .or_default();
            agent_count.0 += 1;
            agent_count.1 += if failed { 1 } else { 0 };
            if call.program == "gh" && gh_api_candidate(&call.args) {
                *gh_api.entry(call.actor.clone()).or_default() += 1;
            }
            if limit > 0 && call.duration_ms.is_finite() && call.duration_ms >= 0.0 {
                slowest.push(SlowCall {
                    time: call.time,
                    host: call.host,
                    actor: call.actor,
                    command,
                    duration_ms: call.duration_ms,
                    exit_code: call.exit_code,
                    signal: call.signal,
                });
                slowest.sort_by(|a, b| {
                    b.duration_ms
                        .total_cmp(&a.duration_ms)
                        .then_with(|| a.time.cmp(&b.time))
                });
                slowest.truncate(limit);
            }
        }
    }
    let mut by_agent_command = agent_counts
        .into_iter()
        .map(|((actor, command), (calls, failures))| AgentCommandCount {
            actor,
            command,
            calls,
            failures,
        })
        .collect::<Vec<_>>();
    by_agent_command.sort_by(|a, b| {
        b.calls
            .cmp(&a.calls)
            .then_with(|| a.actor.cmp(&b.actor))
            .then_with(|| a.command.cmp(&b.command))
    });
    let mut by_host_actor_command = counts
        .into_iter()
        .map(|((host, actor, command), (calls, failures))| Count {
            host,
            actor,
            command,
            calls,
            failures,
        })
        .collect::<Vec<_>>();
    by_host_actor_command.sort_by(|a, b| {
        b.calls
            .cmp(&a.calls)
            .then_with(|| a.host.cmp(&b.host))
            .then_with(|| a.actor.cmp(&b.actor))
            .then_with(|| a.command.cmp(&b.command))
    });
    let mut failures = by_host_actor_command
        .iter()
        .filter(|row| row.failures > 0)
        .cloned()
        .collect::<Vec<_>>();
    failures.sort_by(|a, b| {
        b.failures
            .cmp(&a.failures)
            .then_with(|| a.host.cmp(&b.host))
            .then_with(|| a.actor.cmp(&b.actor))
    });
    let mut gh_api_candidates_by_agent = gh_api
        .into_iter()
        .map(|(actor, calls)| AgentCount { actor, calls })
        .collect::<Vec<_>>();
    gh_api_candidates_by_agent
        .sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.actor.cmp(&b.actor)));
    Ok(Report {
        since,
        until,
        files,
        calls,
        skipped_lines,
        by_agent_command,
        by_host_actor_command,
        gh_api_candidates_by_agent,
        failures,
        slowest,
    })
}

fn command_name(program: &str, args: &[String]) -> String {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if matches!(
            arg.as_str(),
            "-C" | "-c"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--config-env"
                | "-R"
                | "--repo"
                | "--hostname"
        ) {
            index += 2;
        } else if arg.starts_with('-') {
            index += 1;
        } else {
            return format!("{program} {arg}");
        }
    }
    program.to_owned()
}

/// These gh command families normally ask GitHub for data. The recorder cannot see HTTP traffic,
/// so this is intentionally named a candidate count. Failed calls are included; help is excluded.
fn gh_api_candidate(args: &[String]) -> bool {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        return false;
    }
    let command = command_name("gh", args);
    matches!(
        command.as_str(),
        "gh api"
            | "gh pr"
            | "gh issue"
            | "gh repo"
            | "gh release"
            | "gh run"
            | "gh workflow"
            | "gh gist"
            | "gh project"
            | "gh search"
            | "gh ssh-key"
            | "gh gpg-key"
            | "gh secret"
            | "gh variable"
            | "gh label"
            | "gh cache"
            | "gh codespace"
    )
}

pub fn render(report: &Report) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    let _ = writeln!(
        text,
        "Command calls from {} to {}: {} across {} log(s)",
        report.since.to_rfc3339(),
        report.until.to_rfc3339(),
        report.calls,
        report.files.len()
    );
    if report.skipped_lines > 0 {
        let _ = writeln!(text, "Skipped malformed lines: {}", report.skipped_lines);
    }
    let _ = writeln!(text, "\nCalls by agent and command across hosts");
    for row in &report.by_agent_command {
        let _ = writeln!(text, "{:>6}  {:<36}  {}", row.calls, row.actor, row.command);
    }
    let _ = writeln!(text, "\nCalls by host, agent, command");
    for row in &report.by_host_actor_command {
        let _ = writeln!(
            text,
            "{:>6}  {:<18}  {:<36}  {}",
            row.calls, row.host, row.actor, row.command
        );
    }
    let _ = writeln!(
        text,
        "\ngh API-oriented calls by agent (inferred from command, not HTTP traffic)"
    );
    for row in &report.gh_api_candidates_by_agent {
        let _ = writeln!(text, "{:>6}  {}", row.calls, row.actor);
    }
    let _ = writeln!(text, "\nFailures by host, agent, command");
    for row in &report.failures {
        let _ = writeln!(
            text,
            "{:>6}  {:<18}  {:<36}  {}",
            row.failures, row.host, row.actor, row.command
        );
    }
    let _ = writeln!(text, "\nSlowest calls");
    for row in &report.slowest {
        let status = row
            .signal
            .map(|signal| format!("signal {signal}"))
            .unwrap_or_else(|| format!("exit {}", row.exit_code.unwrap_or(1)));
        let _ = writeln!(
            text,
            "{:>10.1} ms  {}  {}  {}  {}  {}",
            row.duration_ms,
            row.time.to_rfc3339(),
            row.host,
            row.actor,
            row.command,
            status
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn combines_hosts_filters_the_period_and_summarizes_failures_and_api_candidates() {
        let root = tempfile::tempdir().unwrap();
        let since = "2026-09-28T00:00:00Z".parse().unwrap();
        let until = "2026-09-29T00:00:00Z".parse().unwrap();
        let mut paths = Vec::new();
        for (host, lines) in [
            (
                "alpha",
                vec![
                    (
                        "2026-09-28T01:00:00Z",
                        "builder",
                        "git",
                        vec!["-C", "repo", "status"],
                        0,
                        8.0,
                    ),
                    (
                        "2026-09-28T02:00:00Z",
                        "builder",
                        "gh",
                        vec!["pr", "list"],
                        1,
                        50.0,
                    ),
                    (
                        "2026-09-27T23:00:00Z",
                        "builder",
                        "gh",
                        vec!["api"],
                        0,
                        99.0,
                    ),
                ],
            ),
            (
                "beta",
                vec![
                    (
                        "2026-09-28T03:00:00Z",
                        "builder",
                        "gh",
                        vec!["api", "repos/example"],
                        0,
                        20.0,
                    ),
                    (
                        "2026-09-28T04:00:00Z",
                        "other",
                        "gh",
                        vec!["--help"],
                        0,
                        2.0,
                    ),
                    (
                        "2026-09-28T05:00:00Z",
                        "builder",
                        "git",
                        vec!["status"],
                        0,
                        3.0,
                    ),
                ],
            ),
        ] {
            let path = root.path().join(host);
            let mut file = File::create(&path).unwrap();
            for (time, actor, program, args, exit_code, duration_ms) in lines {
                writeln!(file, "{}", serde_json::json!({"schema": SCHEMA, "time": time, "host": host, "actor": actor, "program": program, "args": args, "exit_code": exit_code, "signal": null, "duration_ms": duration_ms})).unwrap();
            }
            if host == "beta" {
                file.write_all(b"{\"time\":\"\xff\"}\n").unwrap();
            }
            paths.push(path);
        }
        let report = summarize(paths, since, until, 2).unwrap();
        assert_eq!(report.calls, 5);
        assert_eq!(report.skipped_lines, 1);
        assert_eq!(report.by_agent_command[0].command, "git status");
        assert_eq!(report.by_agent_command[0].calls, 2);
        assert_eq!(
            report.gh_api_candidates_by_agent,
            vec![AgentCount {
                actor: "builder".into(),
                calls: 2
            }]
        );
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].command, "gh pr");
        assert_eq!(
            report
                .by_host_actor_command
                .iter()
                .map(|row| row.host.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "alpha", "beta", "beta", "beta"]
        );
        assert_eq!(
            report
                .slowest
                .iter()
                .map(|row| row.duration_ms)
                .collect::<Vec<_>>(),
            [50.0, 20.0]
        );
    }
}
