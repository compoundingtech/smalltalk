//! Which permission mode st gives the Claude seats it creates.
//!
//! `claude_permission_mode` in `config.toml` is `auto` or `bypass`. A brand new install's setup
//! writes `auto` into the new file. A config without the key reads as `bypass`, so every install
//! that exists today behaves exactly as it did, and st never adds or rewrites the key in a config
//! that is already there: not on upgrade, setup re-run, doctor, repair, or service install and
//! uninstall. Moving an existing install to auto is the person editing their own config.
//!
//! A seat declared with its own `args` is never touched; only a seat whose mode st chooses
//! (`st agents new`, setup, the clients' create action) takes this setting, and
//! `st agents new --claude-permission-mode` overrides it for one seat. Codex has its own,
//! separate setting.

use std::sync::RwLock;

use serde::{Deserialize, Serialize};

/// The `config.toml` key.
pub const KEY: &str = "claude_permission_mode";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, clap::ValueEnum, Hash)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lowercase")]
pub enum PermissionMode {
    /// Claude's classifier reviews each action; the seat stops and asks after repeated blocks.
    Auto,
    /// Claude runs without permission prompts, as every seat st created before auto mode did.
    Bypass,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Bypass => "bypass",
        }
    }

    /// The Claude Code flags that start a seat in this mode. The `--settings` JSON that goes with
    /// them is `creation::claude_seat_settings`.
    pub fn claude_flags(self) -> &'static [&'static str] {
        match self {
            Self::Auto => &["--permission-mode", "auto"],
            Self::Bypass => &["--dangerously-skip-permissions"],
        }
    }

    /// The mode a seat's launch arguments ask for, when they ask for one of these two.
    pub fn from_claude_args<S: AsRef<str>>(args: &[S]) -> Option<Self> {
        let mut found = None;
        let mut iter = args.iter().map(AsRef::as_ref);
        while let Some(arg) = iter.next() {
            match arg {
                "--dangerously-skip-permissions" => found = Some(Self::Bypass),
                "--permission-mode" => match iter.next() {
                    Some("auto") => found = Some(Self::Auto),
                    Some("bypassPermissions") => found = Some(Self::Bypass),
                    _ => found = None,
                },
                _ => {}
            }
        }
        found
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Where the effective mode came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    /// The key is in `config.toml`.
    Config,
    /// The key is missing, which reads as bypass.
    Missing,
}

/// The mode new Claude seats get on this node, and where it came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Effective {
    pub mode: PermissionMode,
    pub source: Source,
}

impl Effective {
    /// What a missing key means. A later release may change it, after the people who run bypass
    /// today have written the key into their own configs.
    pub const MISSING: PermissionMode = PermissionMode::Bypass;

    pub fn resolve(configured: Option<PermissionMode>) -> Self {
        match configured {
            Some(mode) => Self {
                mode,
                source: Source::Config,
            },
            None => Self {
                mode: Self::MISSING,
                source: Source::Missing,
            },
        }
    }

    /// The line `st doctor` prints.
    pub fn describe(&self) -> String {
        match (self.source, self.mode) {
            (Source::Config, mode) => format!(
                "{mode}: set in config.toml ({KEY} = \"{mode}\"). It applies to Claude seats st creates from now on; seats that already exist keep the arguments they were declared with."
            ),
            (Source::Missing, _) => format!(
                "bypass: the key is missing in config.toml, so new Claude seats run without permission prompts, as before. st does not add it for you; to use auto mode, set {KEY} = \"auto\" in config.toml."
            ),
        }
    }
}

/// The setting the daemon read from `config.toml`, for the code that has no `Config`.
static CONFIGURED: RwLock<Option<PermissionMode>> = RwLock::new(None);

/// Remember the configured setting for [`effective_here`]. Call once, from the daemon's startup.
pub fn configure(configured: Option<PermissionMode>) {
    *CONFIGURED
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = configured;
}

/// The effective mode under the setting the daemon was started with.
pub fn effective_here() -> Effective {
    Effective::resolve(
        *CONFIGURED
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_key_reads_as_bypass_and_says_so() {
        let effective = Effective::resolve(None);
        assert_eq!(effective.mode, PermissionMode::Bypass);
        assert_eq!(effective.source, Source::Missing);
        assert!(
            effective
                .describe()
                .starts_with("bypass: the key is missing in config.toml")
        );
    }

    #[test]
    fn the_key_decides_when_it_is_there() {
        for mode in [PermissionMode::Auto, PermissionMode::Bypass] {
            let effective = Effective::resolve(Some(mode));
            assert_eq!(effective.mode, mode);
            assert_eq!(effective.source, Source::Config);
            assert!(
                effective
                    .describe()
                    .starts_with(&format!("{mode}: set in config.toml"))
            );
        }
    }

    #[test]
    fn launch_arguments_name_their_mode() {
        let mode = |args: &[&str]| PermissionMode::from_claude_args(args);
        assert_eq!(
            mode(&["--dangerously-skip-permissions"]),
            Some(PermissionMode::Bypass)
        );
        assert_eq!(
            mode(&["--permission-mode", "bypassPermissions"]),
            Some(PermissionMode::Bypass)
        );
        assert_eq!(
            mode(&["--permission-mode", "auto", "--settings", "{}"]),
            Some(PermissionMode::Auto)
        );
        assert_eq!(mode(&["--permission-mode", "plan"]), None);
        assert_eq!(mode(&["--model", "opus"]), None);
    }
}
