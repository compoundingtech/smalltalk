//! `sekrets adopt gh`: move a person's agents onto sekrets, one step at a time.
//!
//! Step 1, which this does: every seat's `gh` runs through sekrets with the agent profile
//! granted to it, while the person's own shell and st's daemon keep running gh as before.
//!
//! - The agent profile (`PERSON/agent-gh`) exists, allows the `gh-agent` preset and has a login.
//! - A grant gives it to every agent of the person (`agent/**`) with `gh-agent`.
//! - A `gh` shim in `~/.local/bin` sends a seat's gh (a process with `ST_AGENT` set) through
//!   `sekrets -- gh`, and runs the real gh for everything else. `SEKRETS_DIRECT=1` always runs
//!   the real gh, which stays where it is.
//!
//! Running it again reports each part and repairs what is missing. `sekrets unadopt gh` removes
//! the shim. Later steps (git push through sekrets, then removing the person's own gh login)
//! are separate and come only when the person asks.

use std::io::{IsTerminal as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

use crate::client::{Connection, Streams};
use crate::policy::Policy;
use crate::protocol::{CallerView, Request, RunRequest};

/// The first line after the interpreter in every shim sekrets installs; how it knows its own.
const MARKER: &str = "# sekrets: installed by `sekrets adopt gh`; `sekrets unadopt gh` removes it.";
/// The presets an agent profile and its grant need.
const AGENT_PRESETS: &[&str] = &["gh-agent"];

pub struct Adopt {
    pub socket: PathBuf,
    pub bin_dir: PathBuf,
    pub agent_profile: Option<String>,
    /// Create what is missing without asking.
    pub yes: bool,
    /// The `sekrets` binary the shim runs.
    pub sekrets: PathBuf,
    /// Where to look for the real gh: the person's path.
    pub path: std::ffi::OsString,
}

/// One line of the report: what was checked and how it stands.
pub struct Line {
    pub part: &'static str,
    pub state: State,
    pub detail: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Already,
    Done,
    /// The person must do something; the detail says what.
    Needs,
}

impl std::fmt::Display for Line {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mark = match self.state {
            State::Already => "ok   ",
            State::Done => "done ",
            State::Needs => "todo ",
        };
        write!(f, "{mark} {:<14} {}", self.part, self.detail)
    }
}

/// The shim's text: a seat's gh through sekrets, everything else to the real gh.
pub fn shim(sekrets: &Path, real: &Path) -> String {
    let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
    format!(
        "#!/bin/sh\n{MARKER}\n# A seat's gh (ST_AGENT set) runs through sekrets with the profiles granted to it; everything\n# else runs the real gh as before. SEKRETS_DIRECT=1 always runs the real gh.\nif [ -n \"${{ST_AGENT:-}}\" ] && [ -z \"${{SEKRETS_DIRECT:-}}\" ]; then\n  exec {} -- gh \"$@\"\nfi\nexec {} \"$@\"\n",
        quote(sekrets),
        quote(real),
    )
}

/// Whether the file at `path` is a shim sekrets installed.
fn is_ours(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| text.lines().nth(1) == Some(MARKER))
}

/// The real gh: the first `gh` on the path that is not a shim of ours.
pub fn real_gh(path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|directory| directory.join("gh"))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                && !is_ours(candidate)
        })
}

impl Adopt {
    /// Step 1, reported line by line. `confirm` asks the person before creating anything.
    pub fn gh(&self, confirm: &mut dyn FnMut(&str) -> bool) -> Result<Vec<Line>> {
        let connection = Connection::open(&self.socket)?;
        let CallerView::Person { person } = connection.caller.clone() else {
            bail!(
                "run `sekrets adopt gh` yourself, from a login session (an ssh or terminal login, not a terminal inside st)"
            );
        };
        let name = person.trim_start_matches("person/").to_owned();
        let profile = self
            .agent_profile
            .clone()
            .unwrap_or_else(|| format!("{name}/agent-gh"));
        let wanted = Policy::build(
            &AGENT_PRESETS
                .iter()
                .map(|p| (*p).to_owned())
                .collect::<Vec<_>>(),
            &[],
            &[],
        )
        .map_err(anyhow::Error::msg)?;
        let mut lines = Vec::new();
        let mut ask = |question: &str| self.yes || confirm(question);

        // The agent profile and its policy.
        let shown = connection.manage(&Request::ProfileShow {
            profile: profile.clone(),
        });
        let owned = shown.ok().and_then(|value| {
            value
                .as_array()?
                .iter()
                .find(|item| item["grant"].is_null())
                .map(|item| item["profile"].clone())
        });
        match owned {
            None => {
                if ask(&format!(
                    "Create profile {profile} with the gh-agent preset for your agents?"
                )) {
                    connection.manage(&Request::ProfileCreate {
                        profile: profile.clone(),
                        description: Some("GitHub for agents".into()),
                        policy: wanted.clone(),
                        default: false,
                    })?;
                    lines.push(Line {
                        part: "agent profile",
                        state: State::Done,
                        detail: format!("created {profile} with gh-agent"),
                    });
                } else {
                    lines.push(Line {
                        part: "agent profile",
                        state: State::Needs,
                        detail: format!(
                            "sekrets profile create {profile} --preset gh-agent --description 'GitHub for agents'"
                        ),
                    });
                    return Ok(lines);
                }
            }
            Some(existing) => {
                let presets = existing["policy"]["presets"].clone();
                if has_presets(&presets) {
                    lines.push(Line {
                        part: "agent profile",
                        state: State::Already,
                        detail: format!("{profile} allows gh-agent"),
                    });
                } else if ask(&format!(
                    "Replace {profile}'s policy with the gh-agent preset? Your agents do these gh commands today."
                )) {
                    connection.manage(&Request::PolicySet {
                        profile: profile.clone(),
                        policy: wanted.clone(),
                    })?;
                    lines.push(Line {
                        part: "agent profile",
                        state: State::Done,
                        detail: format!("{profile} now allows gh-agent"),
                    });
                } else {
                    lines.push(Line {
                        part: "agent profile",
                        state: State::Needs,
                        detail: format!("sekrets profile policy {profile} --preset gh-agent"),
                    });
                }
            }
        }

        // A login the profile's commands can use.
        let logged_in = self.profile_works(&profile)?;
        lines.push(if logged_in {
            Line {
                part: "agent login",
                state: State::Already,
                detail: format!("{profile} can use GitHub (gh auth status)"),
            }
        } else {
            Line {
                part: "agent login",
                state: State::Needs,
                detail: format!(
                    "sekrets login gh --profile {profile} -- auth login --hostname github.com --git-protocol https --web"
                ),
            }
        });

        // The grant to every agent of this person.
        let grants = connection.manage(&Request::GrantList)?;
        let granted = grants.as_array().is_some_and(|grants| {
            grants.iter().any(|grant| {
                grant["profile"] == profile.as_str()
                    && grant["grantee"] == "agent/**"
                    && grant["until_unix_ms"].is_null()
                    && has_presets(&grant["policy"]["presets"])
            })
        });
        if granted {
            lines.push(Line {
                part: "agent grant",
                state: State::Already,
                detail: format!("{profile} is granted to agent/** with gh-agent"),
            });
        } else if ask(&format!(
            "Grant {profile} to all your agents (agent/**) with gh-agent, with no expiry?"
        )) {
            connection.manage(&Request::GrantAdd {
                profile: profile.clone(),
                to: "agent/**".into(),
                policy: wanted.clone(),
                until_unix_ms: None,
            })?;
            lines.push(Line {
                part: "agent grant",
                state: State::Done,
                detail: format!("granted {profile} to agent/** with gh-agent"),
            });
        } else {
            lines.push(Line {
                part: "agent grant",
                state: State::Needs,
                detail: format!("sekrets grant {profile} --to 'agent/**' --preset gh-agent"),
            });
        }

        // The shim, last: only once agents have something to run with.
        let ready = lines.iter().all(|line| line.state != State::Needs);
        lines.push(self.install_shim(ready)?);
        Ok(lines)
    }

    /// Whether `gh auth status` succeeds through the profile, run as the person.
    fn profile_works(&self, profile: &str) -> Result<bool> {
        let connection = Connection::open(&self.socket)?;
        let null = std::fs::File::open("/dev/null")?;
        let status = connection.run_in(
            RunRequest {
                profile: Some(profile.to_owned()),
                argv: vec!["gh".into(), "auth".into(), "status".into()],
                ..RunRequest::default()
            },
            Path::new("/"),
            Streams::Fds([null.as_raw_fd(), null.as_raw_fd(), null.as_raw_fd()]),
        );
        Ok(matches!(status, Ok(0)))
    }

    fn install_shim(&self, ready: bool) -> Result<Line> {
        let target = self.bin_dir.join("gh");
        let Some(real) = real_gh(&self.path) else {
            return Ok(Line {
                part: "gh shim",
                state: State::Needs,
                detail: "no gh on your path to fall back to; install gh first".into(),
            });
        };
        if real.parent() == Some(self.bin_dir.as_path()) {
            return Ok(Line {
                part: "gh shim",
                state: State::Needs,
                detail: format!(
                    "{} is your own gh, not a sekrets shim; move it elsewhere on your path first",
                    target.display()
                ),
            });
        }
        let text = shim(&self.sekrets, &real);
        if std::fs::read_to_string(&target).is_ok_and(|existing| existing == text) {
            return Ok(Line {
                part: "gh shim",
                state: State::Already,
                detail: format!("{} sends a seat's gh through sekrets", target.display()),
            });
        }
        if !ready {
            return Ok(Line {
                part: "gh shim",
                state: State::Needs,
                detail: "waits for the parts above, so no seat loses gh".into(),
            });
        }
        std::fs::create_dir_all(&self.bin_dir)?;
        let staged = self.bin_dir.join(".gh.sekrets.tmp");
        std::fs::write(&staged, &text)?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&staged, &target)
            .with_context(|| format!("install {}", target.display()))?;
        Ok(Line {
            part: "gh shim",
            state: State::Done,
            detail: format!(
                "{} sends a seat's gh through sekrets; your shell and st's daemon run {} as before",
                target.display(),
                real.display()
            ),
        })
    }

    /// Remove the shim, if it is ours. Profiles and grants stay.
    pub fn unadopt_gh(&self) -> Result<Line> {
        let target = self.bin_dir.join("gh");
        if !target.exists() {
            return Ok(Line {
                part: "gh shim",
                state: State::Already,
                detail: format!("{} is not there", target.display()),
            });
        }
        if !is_ours(&target) {
            bail!("{} is not a sekrets shim; leaving it", target.display());
        }
        std::fs::remove_file(&target)?;
        Ok(Line {
            part: "gh shim",
            state: State::Done,
            detail: format!(
                "removed {}; seats run gh directly again. Profiles and grants are unchanged",
                target.display()
            ),
        })
    }
}

fn has_presets(presets: &Value) -> bool {
    AGENT_PRESETS.iter().all(|wanted| {
        presets
            .as_array()
            .is_some_and(|presets| presets.iter().any(|p| p == wanted))
    })
}

/// Ask on the terminal; no terminal means no.
pub fn ask_terminal(question: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).is_ok() && matches!(answer.trim(), "y" | "Y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shim_sends_only_seats_through_sekrets() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real-gh");
        std::fs::write(&real, "#!/bin/sh\necho \"real $*\"\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fake_sekrets = directory.path().join("sekrets");
        std::fs::write(&fake_sekrets, "#!/bin/sh\necho \"sekrets $*\"\n").unwrap();
        std::fs::set_permissions(&fake_sekrets, std::fs::Permissions::from_mode(0o755)).unwrap();
        let shim_path = directory.path().join("gh");
        std::fs::write(&shim_path, shim(&fake_sekrets, &real)).unwrap();
        std::fs::set_permissions(&shim_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_ours(&shim_path));
        let run = |env: &[(&str, &str)]| {
            let output = std::process::Command::new(&shim_path)
                .args(["pr", "view", "7"])
                .env_remove("ST_AGENT")
                .env_remove("SEKRETS_DIRECT")
                .envs(env.iter().copied())
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap()
        };
        assert_eq!(run(&[]), "real pr view 7\n");
        assert_eq!(
            run(&[("ST_AGENT", "agent/fleet/fixture-web/builder")]),
            "sekrets -- gh pr view 7\n"
        );
        assert_eq!(
            run(&[
                ("ST_AGENT", "agent/fleet/fixture-web/builder"),
                ("SEKRETS_DIRECT", "1")
            ]),
            "real pr view 7\n"
        );
        // The real gh is found past the shim.
        let path = std::env::join_paths([directory.path()]).unwrap();
        assert_eq!(real_gh(&path), None, "only the shim is named gh here");
    }
}
