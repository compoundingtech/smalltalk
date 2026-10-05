//! What an `st` command run inside an exec gate tells the daemon about itself.
//!
//! A gate's exit code is all st reads of its answer, and a pipeline hides a refused `st` call:
//! `st missions ls --limit 500 | grep -q NAME` exits 1 whether the name is absent or st refused
//! the limit. st runs each gate check with [`ENV`] naming a file. An `st` command that st refused,
//! or that printed a listing with more items left, appends one line to that file, and the daemon
//! marks a check whose file has a line broken whatever its exit code.

use std::io::Write as _;
use std::path::Path;

/// The file an `st` command in a gate check appends its refusal or partial listing to.
pub const ENV: &str = "ST_GATE_REPORT";

/// At most this many report lines reach the gate's result and attention item.
const MAX_LINES: usize = 20;

/// Append `line` to the current gate check's report. Outside a gate check this does nothing,
/// and a report st cannot write changes nothing about the command itself.
pub fn note(line: &str) {
    let Some(path) = std::env::var_os(ENV).filter(|path| !path.is_empty()) else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let line = line.replace('\n', " ");
    let _ = writeln!(file, "{line}");
}

/// Note that this command listed only part of a collection: a gate that decides from the
/// listing would decide from items it never saw.
pub fn note_partial_listing(shown: usize) {
    note(&format!(
        "`{}` listed {shown} items and more exist; a gate must not decide from part of a listing, so name the subject, filter the listing, or page it to the end",
        command_line()
    ));
}

/// Note that st refused this command, with st's reason.
pub fn note_refusal(reason: &str) {
    note(&format!("st refused `{}`: {reason}", command_line()));
}

/// This process's command line, as a shell would show it.
pub fn command_line() -> String {
    let mut arguments = std::env::args();
    let program = arguments
        .next()
        .map(|program| {
            Path::new(&program)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or(program)
        })
        .unwrap_or_else(|| "st".into());
    std::iter::once(program)
        .chain(arguments.map(|argument| shell_quote(&argument)))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn shell_quote(argument: &str) -> String {
    if !argument.is_empty()
        && argument
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@,+%".contains(c))
    {
        argument.to_owned()
    } else {
        format!("'{}'", argument.replace('\'', "'\\''"))
    }
}

/// The lines a gate check's `st` commands reported, oldest first and at most [`MAX_LINES`].
pub fn read(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(MAX_LINES)
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn quoting_keeps_plain_arguments_and_quotes_the_rest() {
        assert_eq!(super::shell_quote("--limit"), "--limit");
        assert_eq!(super::shell_quote("doc/fleet/plan"), "doc/fleet/plan");
        assert_eq!(super::shell_quote("two words"), "'two words'");
        assert_eq!(super::shell_quote("it's"), "'it'\\''s'");
        assert_eq!(super::shell_quote(""), "''");
    }

    #[test]
    fn a_report_reads_back_its_lines() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("report");
        std::fs::write(&path, "first\n\n  second  \n").unwrap();
        assert_eq!(super::read(&path), ["first", "second"]);
        assert!(super::read(&root.path().join("missing")).is_empty());
    }
}
