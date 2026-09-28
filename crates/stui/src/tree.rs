use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct MissionsTree {
    #[serde(default)]
    pub runs: Vec<Run>,
    #[serde(default)]
    pub standing_queues: Vec<SeatQueue>,
    #[serde(default)]
    pub unstarted_missions: Vec<UnstartedMission>,
    #[serde(default)]
    pub agents: Vec<Seat>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Run {
    pub id: String,
    pub mission: String,
    pub state: String,
    #[serde(default)]
    pub steps: Vec<Step>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Step {
    pub id: String,
    pub name: String,
    pub state: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SeatQueue {
    pub agent_id: String,
    #[serde(default)]
    pub current_work_ids: Vec<String>,
    pub next_work_id: Option<String>,
    #[serde(default)]
    pub runs: Vec<QueuedRun>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct QueuedRun {
    pub mission_run_id: String,
    pub state: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct UnstartedMission {
    pub id: String,
    pub title: String,
    pub state: String,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Seat {
    pub id: String,
    pub name: String,
    pub host_id: Option<String>,
    pub seat_kind: Option<String>,
    pub driver: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub state: String,
    pub harness_state: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    Run(String),
    Seat(String),
}

impl MissionsTree {
    pub fn from_response(response: serde_json::Value) -> Result<Self> {
        let value = response
            .get("value")
            .cloned()
            .context("missions tree response has no value")?;
        serde_json::from_value(value).context("decode missions tree")
    }

    pub fn targets(&self) -> Vec<Target> {
        self.runs
            .iter()
            .map(|run| Target::Run(run.id.clone()))
            .chain(
                self.standing_queues
                    .iter()
                    .map(|queue| Target::Seat(queue.agent_id.clone())),
            )
            .collect()
    }

    pub fn target_label(&self, target: &Target) -> String {
        match target {
            Target::Run(id) => self
                .runs
                .iter()
                .find(|run| &run.id == id)
                .map(|run| format!("◈ {}", run.mission))
                .unwrap_or_else(|| id.clone()),
            Target::Seat(id) => self
                .agents
                .iter()
                .find(|agent| &agent.id == id)
                .map(|agent| format!("◇ {}", agent.name))
                .unwrap_or_else(|| id.clone()),
        }
    }

    pub fn contains(&self, target: &Target) -> bool {
        match target {
            Target::Run(id) => self.runs.iter().any(|run| &run.id == id),
            Target::Seat(id) => self.standing_queues.iter().any(|seat| &seat.agent_id == id),
        }
    }

    pub fn overview_lines(&self) -> Vec<String> {
        let mut lines = vec!["RUNNING MISSIONS".into()];
        if self.runs.is_empty() {
            lines.push("  none".into());
        }
        for run in &self.runs {
            lines.push(format!("  {}", run_line(run)));
        }
        lines.push(String::new());
        lines.push("STANDING QUEUES".into());
        if self.standing_queues.is_empty() {
            lines.push("  none".into());
        }
        for queue in &self.standing_queues {
            lines.push(format!("  {}: {}", queue.agent_id, queue_line(queue)));
        }
        lines.push(String::new());
        lines.push("UNSTARTED MISSIONS".into());
        if self.unstarted_missions.is_empty() {
            lines.push("  none".into());
        }
        for mission in &self.unstarted_missions {
            let draft = if mission.state == "draft" {
                " (draft)"
            } else {
                ""
            };
            lines.push(format!("  {}{draft}", mission.title));
        }
        lines.push(String::new());
        lines.push("AGENTS BY HOST".into());
        if self.agents.is_empty() {
            lines.push("  none".into());
        }
        let mut hosts = BTreeMap::<&str, BTreeMap<&str, Vec<&Seat>>>::new();
        for agent in &self.agents {
            hosts
                .entry(agent.host_id.as_deref().unwrap_or("unknown"))
                .or_default()
                .entry(agent.seat_kind.as_deref().unwrap_or("standing"))
                .or_default()
                .push(agent);
        }
        for (host, kinds) in hosts {
            lines.push(format!("  {host}"));
            for kind in ["standing", "mission"] {
                if let Some(agents) = kinds.get(kind) {
                    lines.push(format!("    {kind}"));
                    for agent in agents {
                        lines.push(format!("      {}", seat_line(agent)));
                    }
                }
            }
        }
        lines
    }

    pub fn detail_lines(&self, target: &Target) -> Vec<String> {
        match target {
            Target::Run(id) => self.runs.iter().find(|run| &run.id == id).map_or_else(
                || vec!["Run no longer in the active tree".into()],
                |run| {
                    let mut lines = vec![
                        "MISSION RUN".into(),
                        run.mission.clone(),
                        run.id.clone(),
                        format!("State  {}", run.state),
                        String::new(),
                        "STEPS".into(),
                    ];
                    for step in &run.steps {
                        lines.push(format!("  {}  {}  ·  {}", step.state, step.name, step.id));
                    }
                    lines
                },
            ),
            Target::Seat(id) => self
                .standing_queues
                .iter()
                .find(|queue| &queue.agent_id == id)
                .map_or_else(
                    || vec!["Seat no longer in the standing tree".into()],
                    |queue| {
                        let mut lines = vec!["STANDING SEAT".into(), id.clone()];
                        if let Some(agent) = self.agents.iter().find(|agent| &agent.id == id) {
                            lines.push(seat_line(agent));
                        }
                        lines.push(String::new());
                        lines.push(format!(
                            "Current  {}",
                            list_or_none(&queue.current_work_ids)
                        ));
                        lines.push(format!(
                            "Next  {}",
                            queue.next_work_id.as_deref().unwrap_or("none")
                        ));
                        lines.push("QUEUED RUNS".into());
                        for run in &queue.runs {
                            lines.push(format!("  {}  ·  {}", run.mission_run_id, run.state));
                        }
                        lines
                    },
                ),
        }
    }
}

fn list_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items.join(", ")
    }
}

fn run_line(run: &Run) -> String {
    let completed = run
        .steps
        .iter()
        .filter(|step| step.state == "completed")
        .count();
    let active = run
        .steps
        .iter()
        .find(|step| step.state == "claimed")
        .or_else(|| run.steps.iter().find(|step| step.state == "ready"));
    let pending = run
        .steps
        .iter()
        .filter(|step| {
            step.state != "completed" && active.is_none_or(|active| active.id != step.id)
        })
        .map(|step| step.name.as_str())
        .collect::<Vec<_>>();
    format!(
        "{}: {} → {}  ({completed} done)",
        run.mission,
        active.map(|step| step.name.as_str()).unwrap_or("waiting"),
        if pending.is_empty() {
            "done".into()
        } else {
            pending.join(" → ")
        }
    )
}

fn queue_line(queue: &SeatQueue) -> String {
    let waiting = queue
        .runs
        .iter()
        .filter(|run| run.state == "waiting")
        .map(|run| run.mission_run_id.as_str())
        .collect::<Vec<_>>();
    format!(
        "current {} · next {} · waiting {}",
        list_or_none(&queue.current_work_ids),
        queue.next_work_id.as_deref().unwrap_or("none"),
        if waiting.is_empty() {
            "none".into()
        } else {
            waiting.join(", ")
        }
    )
}

fn seat_line(agent: &Seat) -> String {
    let state = if agent.harness_state.as_deref() == Some("working") {
        "working"
    } else if agent.state == "running" {
        "idle"
    } else {
        "waiting"
    };
    format!(
        "{}  {} / {} / {}  {state}",
        agent.name,
        agent.driver.as_deref().unwrap_or("unknown"),
        agent.model.as_deref().unwrap_or("default"),
        agent.effort.as_deref().unwrap_or("default")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_shows_runs_queues_unstarted_and_agents() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../st3/tests/fixtures/missions-tree.json"))
                .unwrap();
        let tree = MissionsTree::from_response(fixture).unwrap();
        let overview = tree.overview_lines().join("\n");
        assert!(overview.contains("mission/atlas: build → review  (1 done)"));
        assert!(overview.contains("current step-run/atlas/build · next step-run/boron/check"));
        assert!(overview.contains("cedar"));
        assert!(overview.contains("orbit/standing  codex / gpt-6-sol / medium  working"));
        let targets = tree.targets();
        assert_eq!(targets.len(), 2);
        assert!(
            tree.detail_lines(&targets[0])
                .join("\n")
                .contains("step-run/atlas/review")
        );
        assert!(
            tree.detail_lines(&targets[1])
                .join("\n")
                .contains("mission-run/boron/1")
        );
    }
}
