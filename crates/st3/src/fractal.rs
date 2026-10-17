//! Additive program-name routing; the existing CLI and UI keep their own identity.
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[derive(Debug, Eq, PartialEq)]
pub enum Dispatch {
    Cli,
    Web,
}

/// Rewrite only the new program's UI spelling. All legacy invocations pass through untouched.
pub fn dispatch(arguments: &mut [OsString]) -> Dispatch {
    let fractal = arguments
        .first()
        .and_then(|program| Path::new(program).file_name())
        == Some(OsStr::new("fractal"));
    // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-16-fractal-binary-aliases — DELETE at contraction — https://github.com/compoundingtech/smalltalk/issues/3032
    // st and st3 retain their existing CLI parser, display strings and bare-program behavior.
    if !fractal {
        return Dispatch::Cli;
    }
    // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-16-fractal-binary-aliases
    match arguments.get(1).and_then(|argument| argument.to_str()) {
        Some("tui") => arguments[1] = "ui".into(),
        Some("web") => return Dispatch::Web,
        _ => {}
    }
    Dispatch::Cli
}

/// Only exact installer-created symlinks belong to this installation.
pub fn owns_alias(path: &Path) -> bool {
    std::fs::read_link(path)
        .is_ok_and(|target| target == Path::new("st3") || target == path.with_file_name("st3"))
}

pub fn may_remove(path: &Path) -> bool {
    path.file_name() != Some(OsStr::new("fractal")) || owns_alias(path)
}

/// Extend old setup manifests when an upgrade installed the new alias beside st3.
/// Recheck recorded aliases too: a foreign replacement never belongs to uninstall.
pub fn installed_binaries(mut files: Vec<PathBuf>) -> Vec<PathBuf> {
    let recorded = files.len();
    for index in 0..recorded {
        if files[index].file_name() == Some(OsStr::new("st3")) {
            let alias = files[index].with_file_name("fractal");
            if owns_alias(&alias) && !files.contains(&alias) {
                files.push(alias);
            }
        }
    }
    files.retain(|path| may_remove(path));
    files
}

pub fn web(arguments: &[OsString]) -> ExitCode {
    if arguments.len() > 2 {
        if arguments.len() == 3 && matches!(arguments[2].to_str(), Some("--help" | "-h")) {
            st3::cli_println!("Usage: fractal web\n\nOpen the running web UI (FRACTAL_WEB_URL; default http://127.0.0.1:5173). This command does not start a server.");
            return ExitCode::SUCCESS;
        }
        eprintln!("Usage: fractal web");
        return ExitCode::from(2);
    }
    let url = match std::env::var("FRACTAL_WEB_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => "http://127.0.0.1:5173".into(),
        Err(error) => {
            eprintln!("fractal web: FRACTAL_WEB_URL: {error}");
            return ExitCode::from(2);
        }
    };
    // A URL is an opener argument, never a shell command or an option.
    if !reqwest::Url::parse(&url)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
    {
        eprintln!("fractal web: FRACTAL_WEB_URL must be an HTTP or HTTPS URL");
        return ExitCode::from(2);
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let opened = Command::new(opener)
        .arg(&url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !opened {
        st3::cli_println!("{url}");
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(input: &[&str]) -> (Dispatch, Vec<OsString>) {
        let mut arguments = input.iter().map(OsString::from).collect::<Vec<_>>();
        (dispatch(arguments.as_mut_slice()), arguments)
    }

    #[test]
    fn fractal_dispatch_no_arguments_keeps_onboarding_and_terminal_guards() {
        let (dispatch, arguments) = route(&["/bin/fractal"]);
        assert_eq!(dispatch, Dispatch::Cli);
        assert_eq!(arguments, vec![OsString::from("/bin/fractal")]);
        for (stdin, stdout, seat, expected) in [
            (true, true, false, true),
            (false, true, false, false),
            (true, false, false, false),
            (true, true, true, false),
        ] {
            assert_eq!(
                crate::plain_ui_requested(arguments.len(), stdin, stdout, seat),
                expected
            );
        }
    }

    #[test]
    fn fractal_dispatch_tui_reuses_ui_arguments() {
        assert_eq!(
            route(&["fractal", "tui", "--help"]),
            (
                Dispatch::Cli,
                vec!["fractal".into(), "ui".into(), "--help".into()]
            )
        );
    }

    #[test]
    fn fractal_dispatch_web() {
        assert_eq!(route(&["fractal", "web"]).0, Dispatch::Web);
    }

    #[test]
    fn fractal_dispatch_cli_passthrough_and_legacy_are_unchanged() {
        for input in [
            &["fractal", "work", "ls"][..],
            &["fractal", "agents", "ls"],
            &["st"],
            &["st3"],
            &["st", "web"],
            &["st3", "tui"],
        ] {
            assert_eq!(
                route(input),
                (Dispatch::Cli, input.iter().map(OsString::from).collect())
            );
        }
    }

    #[test]
    fn installed_alias_is_discovered_for_old_manifests_and_only_removed_while_owned() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let st3 = directory.path().join("st3");
        let alias = directory.path().join("fractal");
        let legacy_files = vec![st3.clone(), directory.path().join("st")];
        for target in [PathBuf::from("st3"), st3] {
            symlink(&target, &alias).unwrap();
            let upgraded = installed_binaries(legacy_files.clone());
            assert_eq!(upgraded.last(), Some(&alias));
            assert!(may_remove(&alias));
            assert_eq!(installed_binaries(upgraded.clone()), upgraded);
            std::fs::remove_file(&alias).unwrap();
        }
        for target in ["foreign", "/missing/foreign"] {
            symlink(target, &alias).unwrap();
            let mut recorded = legacy_files.clone();
            recorded.push(alias.clone());
            assert_eq!(installed_binaries(recorded), legacy_files);
            assert!(!may_remove(&alias));
            std::fs::remove_file(&alias).unwrap();
        }
        std::fs::write(&alias, "foreign").unwrap();
        assert!(!may_remove(&alias));
        assert_eq!(installed_binaries(legacy_files.clone()), legacy_files);
        std::fs::remove_file(&alias).unwrap();
        std::fs::create_dir(&alias).unwrap();
        assert!(!may_remove(&alias));
    }
}
