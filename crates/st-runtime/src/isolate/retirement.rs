//! Complete work-scope ending evidence, separate from a delivered signal.
use super::*;
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read as _, Seek as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::MetadataExt as _;

/// cgroup.events is pollable. Subscribe before the signal so an empty
/// transition cannot be lost between the effect and installing the wait.
struct PopulationEvents(File);

impl PopulationEvents {
    fn open(path: &Path) -> Result<Self> {
        Ok(Self(File::open(path.join("cgroup.events"))?))
    }

    fn populated(&mut self) -> Result<bool> {
        self.0.rewind()?;
        let mut events = String::new();
        (&mut self.0).take(65537).read_to_string(&mut events)?;
        is_populated(&events)
    }

    fn wait_empty(&mut self, within: Duration) -> Result<bool> {
        let deadline = Instant::now() + within;
        loop {
            if !self.populated()? {
                return Ok(true);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            let mut event = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLPRI,
                revents: 0,
            };
            let milliseconds =
                i32::try_from(left.as_millis().saturating_add(1)).unwrap_or(i32::MAX);
            let ready = unsafe { libc::poll(&mut event, 1, milliseconds) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if ready == 0 {
                return Ok(false);
            }
            // kernfs sends POLLERR together with POLLPRI for a changed file.
            // A bare error, invalid descriptor or hangup is unreadable evidence.
            ensure!(
                event.revents & libc::POLLPRI != 0
                    && event.revents & (libc::POLLNVAL | libc::POLLHUP) == 0,
                "work scope population notification is unreadable"
            );
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeRetirementProof {
    pub version: u8,
    pub unit: String,
    pub load_state: String,
    pub active_state: String,
    pub control_group: String,
    pub cgroup: Option<CgroupEnding>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CgroupEnding {
    pub device: u64,
    pub inode: u64,
    pub events: String,
}

fn group_path(unit: &str, value: &str) -> Result<PathBuf> {
    ensure!(
        value.starts_with('/')
            && value.ends_with(&format!("/{unit}"))
            && Path::new(value).components().all(|part| matches!(
                part,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )),
        "work scope has a different control group"
    );
    Ok(Path::new("/sys/fs/cgroup").join(value.trim_start_matches('/')))
}

fn is_populated(events: &str) -> Result<bool> {
    ensure!(events.len() <= 65536, "work scope events are oversized");
    let mut populated = None;
    for line in events.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        ensure!(
            fields.len() == 2 && fields[1].parse::<u64>().is_ok(),
            "work scope events are malformed"
        );
        if fields[0] == "populated" {
            ensure!(
                populated.is_none() && matches!(fields[1], "0" | "1"),
                "work scope population is contradictory"
            );
            populated = Some(fields[1] == "1");
        }
    }
    populated.context("work scope events have no population evidence")
}

impl ScopeRetirementProof {
    pub fn validate(&self, expected_unit: &str) -> Result<()> {
        ensure!(
            self.version == 1
                && self.unit == expected_unit
                && self.unit.starts_with("st3-")
                && !self.unit.starts_with("st3-pty-server-")
                && self.unit.ends_with(".scope")
                && self.unit.len() <= 256
                && self
                    .unit
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
            "work scope ending changed its bound unit"
        );
        match self.load_state.as_str() {
            "not-found" => ensure!(
                self.active_state == "inactive"
                    && self.control_group.is_empty()
                    && self.cgroup.is_none(),
                "unloaded work scope has contradictory evidence"
            ),
            "loaded" => {
                group_path(&self.unit, &self.control_group)?;
                let cgroup = self
                    .cgroup
                    .as_ref()
                    .context("work scope ending lacks its cgroup")?;
                ensure!(
                    cgroup.inode != 0 && !is_populated(&cgroup.events)?,
                    "work scope still holds processes"
                );
            }
            _ => anyhow::bail!("work scope load state is unverified"),
        }
        Ok(())
    }
}

fn properties(unit: &str, output: &str) -> Result<ScopeRetirementProof> {
    let property = |key: &str| -> Result<String> {
        let prefix = format!("{key}=");
        let mut values = output.lines().filter_map(|line| line.strip_prefix(&prefix));
        let value = values.next().context("work scope response is incomplete")?;
        ensure!(
            values.next().is_none(),
            "work scope response is contradictory"
        );
        Ok(value.into())
    };
    let proof = ScopeRetirementProof {
        version: 1,
        unit: unit.into(),
        load_state: property("LoadState")?,
        active_state: property("ActiveState")?,
        control_group: property("ControlGroup")?,
        cgroup: None,
    };
    ensure!(
        matches!(proof.load_state.as_str(), "loaded" | "not-found"),
        "work scope load state is unverified"
    );
    if proof.load_state == "not-found" {
        proof.validate(unit)?;
    } else {
        group_path(unit, &proof.control_group)?;
    }
    Ok(proof)
}

fn observe(unit: &str) -> Result<ScopeRetirementProof> {
    let mut command = Command::new("systemctl");
    command.args([
        "--user",
        "show",
        "--property=LoadState",
        "--property=ActiveState",
        "--property=ControlGroup",
        unit,
    ]);
    let output = crate::pty::output_within(command, SYSTEMCTL_TIMEOUT)?;
    ensure!(
        output.status.success(),
        "work scope manager did not return verified state"
    );
    let mut proof = properties(unit, std::str::from_utf8(&output.stdout)?)?;
    if proof.load_state == "loaded" {
        let path = group_path(unit, &proof.control_group)?;
        let before = std::fs::metadata(&path)?;
        ensure!(before.is_dir(), "work scope cgroup is not a directory");
        let events = std::fs::read_to_string(path.join("cgroup.events"))?;
        is_populated(&events)?;
        let after = std::fs::metadata(&path)?;
        ensure!(
            (before.dev(), before.ino()) == (after.dev(), after.ino()),
            "work scope cgroup changed during capture"
        );
        proof.cgroup = Some(CgroupEnding {
            device: before.dev(),
            inode: before.ino(),
            events,
        });
    }
    Ok(proof)
}

/// Independent physical worker only. The same original request is checked
/// before each scope signal, never while holding a Store writer. Scope stop
/// grace/escalation use the existing shutdown budget, not a start timeout.
pub fn end_scope_with_proof(
    unit: &str,
    guard: &dyn Fn() -> Result<()>,
) -> Result<Option<ScopeRetirementProof>> {
    if mode() != Isolation::Scope || !is_work_scope(unit) {
        return Ok(None);
    }
    let before = observe(unit)?;
    if before.validate(unit).is_ok() {
        guard()?;
        return Ok(Some(before));
    }
    let cgroup = before
        .cgroup
        .as_ref()
        .context("work scope has no readable cgroup")?;
    ensure!(
        is_populated(&cgroup.events)?,
        "work scope has no verified population"
    );
    let path = group_path(unit, &before.control_group)?;
    let mut events = PopulationEvents::open(&path)?;
    guard()?;
    kill_unit(unit, libc::SIGTERM)?;
    if !events.wait_empty(SCOPE_GRACE)? {
        guard()?;
        kill_unit(unit, libc::SIGKILL)?;
        ensure!(
            events.wait_empty(SCOPE_KILL_WAIT)?,
            "work scope survived retirement"
        );
    }
    let after = observe(unit)?;
    after.validate(unit)?;
    if let Some(ending) = &after.cgroup {
        ensure!(
            after.control_group == before.control_group
                && (ending.device, ending.inode) == (cgroup.device, cgroup.inode),
            "work scope identity changed during retirement"
        );
    }
    guard()?;
    Ok(Some(after))
}

#[cfg(test)]
mod tests {
    use super::*;
    const UNIT: &str = "st3-op-example-1-1.scope";
    #[test]
    fn scope_retirement_requires_complete_noncontradictory_manager_and_cgroup_evidence() {
        let gone = properties(
            UNIT,
            "LoadState=not-found\nActiveState=inactive\nControlGroup=\n",
        )
        .unwrap();
        gone.validate(UNIT).unwrap();
        for response in [
            "",
            "LoadState=not-found\nControlGroup=\n",
            "LoadState=not-found\nLoadState=loaded\nActiveState=inactive\nControlGroup=\n",
            "LoadState=error\nActiveState=inactive\nControlGroup=\n",
            "LoadState=not-found\nActiveState=active\nControlGroup=\n",
            "LoadState=loaded\nActiveState=active\nControlGroup=/another.scope\n",
        ] {
            assert!(properties(UNIT, response).is_err(), "{response}");
        }
        let mut empty = properties(
            UNIT,
            &format!("LoadState=loaded\nActiveState=inactive\nControlGroup=/user.slice/{UNIT}\n"),
        )
        .unwrap();
        assert!(empty.validate(UNIT).is_err());
        empty.cgroup = Some(CgroupEnding {
            device: 7,
            inode: 11,
            events: "populated 0\nfrozen 0\n".into(),
        });
        empty.validate(UNIT).unwrap();
        assert!(empty.validate("st3-another.scope").is_err());
        for events in [
            "",
            "frozen 0\n",
            "populated 1\n",
            "populated 0\npopulated 1\n",
            "populated maybe\n",
        ] {
            empty.cgroup.as_mut().unwrap().events = events.into();
            assert!(empty.validate(UNIT).is_err(), "{events}");
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "retirement_native_tests.rs"]
mod native_tests;
