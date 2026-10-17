//! Read both service namespaces without ever choosing between conflicting installs.
// arn:lmig:smalltalk:2026-10-16-service-labels-fractal
use super::*;

#[derive(Debug)]
pub struct ConflictingServices(String, String);

impl std::fmt::Display for ConflictingServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "conflicting service units: {} and {}; refusing to choose or change either", self.0, self.1)
    }
}

impl std::error::Error for ConflictingServices {}

fn is_new_family(status: &ServiceStatus) -> bool {
    status.name.starts_with("fractal") || status.name.starts_with("com.compoundingtech.fractal")
}

fn check_conflicts<'a>(statuses: impl IntoIterator<Item = &'a ServiceStatus>) -> Result<()> {
    let mut legacy: Option<&ServiceStatus> = None;
    let mut new: Option<&ServiceStatus> = None;
    for status in statuses {
        if status.installed || status.running {
            if is_new_family(status) {
                new = Some(status);
            } else {
                legacy = Some(status);
            }
        }
    }
    if let (Some(legacy), Some(new)) = (legacy, new) {
        return Err(ConflictingServices(legacy.name.clone(), new.name.clone()).into());
    }
    Ok(())
}

fn select(mut statuses: Vec<ServiceStatus>) -> Result<ServiceStatus> {
    check_conflicts(&statuses)?;
    // Candidate order retains the original CLI target when legacy installs coexist.
    let selected = statuses.iter().position(|status| status.installed || status.running).unwrap_or(0);
    Ok(statuses.swap_remove(selected))
}

fn allow_install(services: &Services) -> Result<()> {
    for status in [&services.daemon, &services.replication] {
        anyhow::ensure!(
            !(status.installed || status.running) || !is_new_family(status),
            "{} already exists; this installer still writes legacy names; refusing to create a second service",
            status.name
        );
    }
    Ok(())
}

pub(super) struct Services {
    pub daemon: ServiceStatus,
    pub replication: ServiceStatus,
}

impl Services {
    pub fn report(self) -> ServiceStatusReport {
        ServiceStatusReport {
            manager: if cfg!(target_os = "linux") { "systemd-user" } else { "launchd-user" },
            services: vec![self.daemon, self.replication],
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn native() -> Result<Services> {
    #[cfg(target_os = "linux")]
    let old = [service_name(), replication_service_name()];
    #[cfg(target_os = "macos")]
    let old = [service_label(), replication_service_label()];
    let resolve = |old: &str| -> Result<ServiceStatus> {
        let new = old.replacen("st3", "fractal", 1);
        #[cfg(target_os = "linux")]
        let mut names = vec![old.to_owned(), new];
        #[cfg(target_os = "macos")]
        let names = vec![old.to_owned(), new];
        // Home Manager writes smalltalk.service rather than the CLI's st3.service.
        #[cfg(target_os = "linux")]
        if crate::instance::current().is_none() && old == service_name() {
            names.push("smalltalk.service".into());
        }
        select(names.iter().map(|name| {
            #[cfg(target_os = "linux")]
            {
                let mut status = systemd_service_status(name)?;
                status.installed |= systemd_path(name)?.is_file();
                Ok(status)
            }
            #[cfg(target_os = "macos")]
            {
                launchd_service_status(name, &launchd_path(name)?)
            }
        }).collect::<Result<Vec<_>>>()?)
    };
    let services = Services { daemon: resolve(old[0])?, replication: resolve(old[1])? };
    check_conflicts([&services.daemon, &services.replication])?;
    Ok(services)
}

#[cfg(target_os = "linux")]
pub(super) fn systemd_path(name: &str) -> Result<PathBuf> {
    Ok(systemd_user_unit_path()?.with_file_name(name))
}

#[cfg(target_os = "macos")]
pub(super) fn launchd_path(label: &str) -> Result<PathBuf> {
    Ok(launch_agent_path()?.with_file_name(format!("{label}.plist")))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn before_install() -> Result<()> {
    allow_install(&native()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(name: &str, installed: bool, running: bool) -> ServiceStatus {
        ServiceStatus { name: name.into(), installed, running, state: String::new() }
    }

    #[test]
    fn service_namespace_presence_matrix() {
        for (old, new) in [("st3.service", "fractal.service"),
            ("st3-replication.service", "fractal-replication.service"),
            ("com.compoundingtech.st3", "com.compoundingtech.fractal"),
            ("com.compoundingtech.st3.replication", "com.compoundingtech.fractal.replication"),
            ("smalltalk.service", "fractal.service"),
            ("st3-instance-try.service", "fractal-instance-try.service")] {
            for (old_exists, new_exists, expected) in [
                (true, false, Some(old)), (false, true, Some(new)),
                (true, true, None), (false, false, Some(old)),
            ] {
                let result = select(vec![status(old, old_exists, false), status(new, new_exists, false)]);
                if let Some(expected) = expected {
                    let result = result.unwrap();
                    assert_eq!(result.name, expected);
                    assert_eq!(result.installed, old_exists || new_exists);
                } else {
                    assert!(result.unwrap_err().to_string().contains("conflicting service units"));
                }
            }
            // Loaded services remain conflicting after their files have disappeared.
            assert!(select(vec![status(old, false, true), status(new, false, true)]).is_err());
            assert!(select(vec![status(old, true, false), status(new, false, true)]).is_err());
        }
    }

    #[test]
    fn legacy_coexistence_preserves_the_cli_target_and_allows_install() {
        let daemon = select(vec![
            status("st3.service", true, false),
            status("fractal.service", false, false),
            status("smalltalk.service", true, true),
        ]).unwrap();
        assert_eq!(daemon.name, "st3.service");
        assert!(allow_install(&Services {
            daemon,
            replication: status("st3-replication.service", false, false),
        }).is_ok());
    }

    #[test]
    fn home_manager_only_install_is_allowed() {
        assert!(allow_install(&Services {
            daemon: status("smalltalk.service", true, true),
            replication: status("st3-replication.service", false, false),
        }).is_ok());
    }

    #[test]
    fn families_conflict_even_across_daemon_and_replication() {
        assert!(check_conflicts([
            &status("smalltalk.service", true, false),
            &status("fractal-replication.service", true, false),
        ]).is_err());
        assert!(allow_install(&Services {
            daemon: status("fractal.service", true, false),
            replication: status("fractal-replication.service", false, false),
        }).is_err());
    }

    #[test]
    fn systemd_absence_and_missing_properties_never_invent_installed_units() {
        for active in ["inactive", "failed"] {
            let reply = format!("LoadState=not-found\nActiveState={active}\nSubState=dead\n");
            let legacy = parse_systemd_service_status("smalltalk.service", &reply).unwrap();
            let new = parse_systemd_service_status("fractal.service", &reply).unwrap();
            assert!(!legacy.installed && !legacy.running);
            assert!(!new.installed && !new.running);
            assert!(!select(vec![legacy, new]).unwrap().installed);
        }
        for reply in [
            "",
            "manager diagnostic, not unit properties",
            "LoadState=not-found\n",
            "LoadState=not-found\nSubState=dead\n",
            "ActiveState=inactive\nSubState=dead\n",
            "LoadState=\nActiveState=inactive\nSubState=dead\n",
            "LoadState=unknown\nActiveState=inactive\nSubState=dead\n",
        ] {
            assert!(parse_systemd_service_status("fractal.service", reply).is_err(), "{reply:?}");
        }
        // A manager-confirmed active service still counts after its unit file disappears.
        let active = parse_systemd_service_status(
            "fractal.service", "LoadState=not-found\nActiveState=active\nSubState=running\n",
        ).unwrap();
        assert!(!active.installed && active.running);
        let loaded = parse_systemd_service_status(
            "st3.service", "LoadState=loaded\nActiveState=inactive\nSubState=dead\n",
        ).unwrap();
        assert!(loaded.installed && !loaded.running);
    }
}
