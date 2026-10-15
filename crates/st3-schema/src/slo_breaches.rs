//! Bounded canonical SLO transition records. Measurements are origin attestations;
//! importing a signature does not independently measure another member's processes.
use super::*;

pub const KIND: &str = "slo.breach-transition";
pub const MAX_OBJECTIVES: usize = 64;
pub const MAX_GROUP_CLAIMS: usize = 16;
pub const MAX_GROUP_BYTES: usize = 32 * 1024;
pub const MAX_RECORD_BYTES: usize = 8 * 1024;
/// Conservative sub-ceiling: full groups plus transition receipts stay below the approved daily ceilings.
pub const MAX_DAILY_OPENINGS: usize = 700;
pub const MAX_DAILY_ONSETS: usize = 832;
pub const DEFAULT_OWNER: &str = "agent/fleet/smalltalk/speed";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub unit: String,
    pub value: Option<f64>,
    pub target: f64,
    pub minimum: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub member: String,
    pub epoch: String,
    pub epoch_started_ms: u64,
    pub sequence: u64,
    pub predecessor_cut: Option<String>,
    pub version: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub observed_at_ms: u64,
    pub window_ms: u64,
    pub covered_ms: u64,
    pub complete: bool,
    pub population: String,
    pub source_cut: String,
    pub checks: Vec<Check>,
    /// Existing bounded counters only. No profiler is activated to fill these.
    pub counters: BTreeMap<String, u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    Missed,
    Met,
    Unknown,
}

impl Observation {
    pub fn decision(&self) -> Decision {
        if self.validate().is_err() || !self.complete || self.covered_ms < self.window_ms {
            return Decision::Unknown;
        }
        let mut missing = false;
        for check in &self.checks {
            let Some(value) = check.value else {
                missing = true;
                continue;
            };
            if (check.minimum && value < check.target)
                || (!check.minimum && value > check.target)
            {
                return Decision::Missed;
            }
        }
        if missing || self.checks.is_empty() { Decision::Unknown } else { Decision::Met }
    }

    pub fn same_policy(&self, other: &Self) -> bool {
        self.version == other.version && self.population == other.population && self.window_ms == other.window_ms
            && self.checks.len() == other.checks.len()
            && self.checks.iter().zip(&other.checks).all(|(a,b)| a.unit == b.unit && a.target == b.target && a.minimum == b.minimum)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.member.is_empty() || self.member.len() > 128 || self.epoch.is_empty() || self.epoch.len() > 128
            || self.sequence == 0 || self.predecessor_cut.as_ref().is_some_and(|cut| cut.is_empty() || cut.len() > 128)
            || self.version.is_empty() || self.version.len() > 128
            || self.population.is_empty() || self.population.len() > 128
            || self.source_cut.is_empty() || self.source_cut.len() > 128
            || self.epoch_started_ms > self.start_ms || self.start_ms >= self.end_ms || self.end_ms > self.observed_at_ms
            || self.window_ms == 0 || self.observed_at_ms > i64::MAX as u64
            || self.covered_ms > self.end_ms - self.start_ms
            || (self.complete && (self.covered_ms != self.window_ms || self.end_ms-self.start_ms != self.window_ms))
            || self.predecessor_cut.as_deref() == Some(self.source_cut.as_str())
            || self.checks.is_empty() || self.checks.len() > 4 || self.counters.len() > 8
        {
            return Err("invalid SLO window, source or coverage".into());
        }
        for check in &self.checks {
            if check.unit.is_empty() || check.unit.len() > 32 || !check.target.is_finite()
                || check.value.is_some_and(|value| !value.is_finite())
            {
                return Err("invalid SLO unit or numeric reading".into());
            }
        }
        if self.counters.keys().any(|key| key.is_empty() || key.len() > 64)
            || serde_json::to_vec(self).map_err(|e| e.to_string())?.len() > 1024
        {
            return Err("SLO reading and counter cut exceed 1 KiB".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    Pending,
    Dispatched,
    OwnerBlocked,
    OwnerUnblocked,
    Recovered,
}

impl Event {
    pub fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Dispatched => "dispatched",
            Self::OwnerBlocked => "owner_blocked",
            Self::OwnerUnblocked => "owner_unblocked",
            Self::Recovered => "recovered",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessDelta {
    pub role: String,
    pub pid: u32,
    pub birth: u64,
    pub cpu_start_ns: u64,
    pub cpu_end_ns: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HealthyWitness {
    pub source_cut: String,
    pub start_ms: u64,
    pub end_ms: u64,
    /// Zero-worker topology is not qualified by this source seam.
    pub expected_workers: u8,
    pub monotonic_start_ns: u64,
    pub monotonic_end_ns: u64,
    pub boundary_uncertainty_ns: u64,
    pub complete_cohort: bool,
    pub positively_idle: bool,
    pub processes: Vec<ProcessDelta>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub fleet: String,
    /// Logical name, not the rotatable signing key or process incarnation.
    pub member: String,
    pub objective: String,
    pub episode: String,
    pub ordinal: u64,
    pub previous_recovery: Option<String>,
    pub owner: String,
    pub event: Event,
    pub onset: Observation,
    pub onset_witness: Option<HealthyWitness>,
    pub healthy: Option<Observation>,
    pub latest_miss: Option<Observation>,
    pub recovery_floor_ms: Option<u64>,
    pub witness: Option<HealthyWitness>,
    pub mission_run: Option<String>,
    pub dispatched_at_ms: Option<u64>,
    /// Intrinsic dispatch decision, never inferred from the absence of a recovery claim.
    pub execution_ready: Option<bool>,
    pub owner_blocked_at_dispatch: Option<bool>,
}

impl Record {
    pub fn subject(&self) -> String { format!("resource/slo-breach/{}", self.episode) }

    pub fn episode_id(fleet: &str, member: &str, objective: &str, previous: Option<&str>) -> String {
        let identity = serde_json::to_vec(&(fleet, member, objective, previous)).expect("string tuple");
        hex::encode(Sha256::digest(identity))
    }

    pub fn validate(&self) -> Result<(), String> {
        for text in [&self.fleet, &self.member, &self.objective] {
            if text.is_empty() || text.len() > 128 || text.chars().any(char::is_control) {
                return Err("invalid logical SLO identity".into());
            }
        }
        if !self.owner.starts_with("agent/") || self.owner.len() > 256
            || self.owner.split('/').any(str::is_empty) || self.owner.chars().any(char::is_control)
            || self.ordinal == 0 || self.ordinal > i64::MAX as u64
            || (self.ordinal == 1) != self.previous_recovery.is_none()
            || self.previous_recovery.as_ref().is_some_and(|id| id.is_empty() || id.len() > 128)
            || self.episode != Self::episode_id(&self.fleet, &self.member, &self.objective, self.previous_recovery.as_deref())
        {
            return Err("invalid SLO owner, lineage or episode identity".into());
        }
        self.onset.validate()?;
        if self.onset.member != self.member { return Err("relabelled remote SLO onset".into()); }
        validate_cpu_window(&self.objective, &self.onset, self.onset_witness.as_ref())?;
        if self.onset.decision() != Decision::Missed {
            return Err("a breach needs an evidenced missed objective".into());
        }
        if self.event == Event::Recovered {
            let healthy = self.healthy.as_ref().ok_or("recovery needs its actual healthy window")?;
            healthy.validate()?;
            let latest = self.latest_miss.as_ref().ok_or("recovery needs the latest qualified missed cut")?;
            latest.validate()?;
            let floor = self.recovery_floor_ms.ok_or("recovery needs the current coverage floor")?;
            if latest.member != self.member || latest.decision() != Decision::Missed
                || latest.end_ms < self.onset.end_ms || !latest.same_policy(&self.onset)
                || floor < latest.end_ms || floor < healthy.epoch_started_ms
                || healthy.member != self.member || healthy.source_cut == latest.source_cut
                || healthy.predecessor_cut.as_deref() != Some(latest.source_cut.as_str())
                || (healthy.epoch == latest.epoch && healthy.sequence <= latest.sequence)
                || healthy.decision() != Decision::Met || healthy.start_ms < floor
                || !healthy.same_policy(&self.onset)
            {
                return Err("partial, stale or unknown evidence cannot recover a breach".into());
            }
            validate_cpu_window(&self.objective, healthy, self.witness.as_ref())?;
            if healthy.epoch == self.onset.epoch
                && let (Some(onset), Some(recovered)) = (&self.onset_witness, &self.witness)
            {
                if recovered.monotonic_start_ns < onset.monotonic_end_ns {
                    return Err("CPU healthy cut overlaps the missed cut".into());
                }
                for process in &recovered.processes {
                    if onset.processes.iter().any(|old| old.pid == process.pid && old.birth == process.birth
                        && (old.role != process.role || process.cpu_start_ns < old.cpu_end_ns))
                    {
                        return Err("CPU counter reset or role change across source cuts".into());
                    }
                }
            }
        } else if self.healthy.is_some() || self.witness.is_some() || self.latest_miss.is_some() || self.recovery_floor_ms.is_some() {
            return Err("only recovery carries a healthy witness".into());
        }
        if self.event == Event::Dispatched {
            if self.mission_run.as_deref() != Some(format!("mission-run/slo/{}", self.episode).as_str())
                || self.dispatched_at_ms.is_none_or(|at| at > i64::MAX as u64)
                || self.execution_ready.is_none() || self.owner_blocked_at_dispatch.is_none()
            {
                return Err("dispatch needs its deterministic mission and rate witness".into());
            }
        } else if self.mission_run.is_some() || self.dispatched_at_ms.is_some() || self.execution_ready.is_some() || self.owner_blocked_at_dispatch.is_some() {
            return Err("only dispatch names its mission opening".into());
        }
        if serde_json::to_vec(self).map_err(|e| e.to_string())?.len() > MAX_RECORD_BYTES {
            return Err("SLO transition exceeds its byte bound".into());
        }
        Ok(())
    }
}

/// Coverage and arithmetic are validated by every reducer. The OS counters remain
/// the authenticated origin's attestation, not independently measured remote truth.
pub fn validate_cpu_window(objective: &str, reading: &Observation, witness: Option<&HealthyWitness>) -> Result<(), String> {
    if objective != "cpu" && objective != "cpu/idle" {
        return if witness.is_none() { Ok(()) } else { Err("non-CPU reading carries a CPU witness".into()) };
    }
    let witness = witness.ok_or("CPU evidence needs the complete process witness")?;
    if witness.source_cut != reading.source_cut || witness.start_ms != reading.start_ms || witness.end_ms != reading.end_ms
        || witness.source_cut.len() > 128 || witness.expected_workers == 0 || witness.expected_workers > 8
        || !witness.complete_cohort || witness.processes.is_empty() || witness.processes.len() > 9
        || witness.monotonic_end_ns <= witness.monotonic_start_ns
        || (objective == "cpu/idle" && reading.window_ms < 3_600_000)
        || (objective == "cpu/idle" && !witness.positively_idle)
    {
        return Err("CPU coverage or idle eligibility is UNKNOWN".into());
    }
    let mut identities = std::collections::BTreeSet::new();
    let mut daemons = 0;
    let mut workers = 0;
    let mut cpu_ns = 0u128;
    for process in &witness.processes {
        if !matches!(process.role.as_str(), "daemon" | "replication-worker")
            || process.pid == 0 || process.birth == 0 || process.cpu_end_ns < process.cpu_start_ns
            || !identities.insert((process.pid, process.birth))
        {
            return Err("invalid or duplicate process role/birth/counter".into());
        }
        daemons += usize::from(process.role == "daemon");
        workers += usize::from(process.role == "replication-worker");
        cpu_ns += u128::from(process.cpu_end_ns - process.cpu_start_ns);
    }
    if daemons != 1 || workers != usize::from(witness.expected_workers) {
        return Err("CPU witness omits a required worker or has an invalid daemon cohort".into());
    }
    let elapsed = witness.monotonic_end_ns - witness.monotonic_start_ns;
    // UTC endpoints have millisecond resolution. The counter interval must be
    // that same window, not an independently longer averaging interval.
    if elapsed / 1_000_000 != reading.end_ms - reading.start_ms {
        return Err("CPU wall and monotonic counter windows differ".into());
    }
    let certain = elapsed.checked_sub(witness.boundary_uncertainty_ns)
        .filter(|certain| *certain >= reading.window_ms.saturating_mul(1_000_000))
        .ok_or("uncertain CPU boundaries do not cover the full window")?;
    let longest = elapsed.checked_add(witness.boundary_uncertainty_ns).ok_or("CPU interval overflow")?;
    let upper = cpu_ns as f64 / certain as f64;
    let lower = cpu_ns as f64 / longest as f64;
    let target = if objective == "cpu/idle" { 0.5 } else { 1.0 };
    if reading.checks.len() != 1 || reading.checks[0].unit != "cores"
        || reading.checks[0].minimum || reading.checks[0].target != target
        || reading.checks[0].value.is_none_or(|value|
            (value - cpu_ns as f64 / elapsed as f64).abs() > 1e-9)
        || (reading.decision() == Decision::Met && upper > target)
        || (reading.decision() == Decision::Missed && lower <= target)
    {
        return Err("CPU reading, target or boundary uncertainty is inconsistent".into());
    }
    Ok(())
}

pub fn validate_claim(subject: &str, kind: &str, body: &Value, origin: &str, actor: Option<&str>) -> Result<(), String> {
    if kind != KIND { return Ok(()); }
    let record: Record = serde_json::from_value(body["fields"]["record"].clone()).map_err(|e| e.to_string())?;
    record.validate()?;
    if record.member != origin || subject != record.subject() || actor != Some("daemon/runtime") {
        return Err("SLO observation must be authored by its own authenticated member runtime".into());
    }
    Ok(())
}
