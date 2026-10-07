//! Files a command reads, passed by the caller. A command run as a profile must never read a
//! file of the sekrets user's (the profile's own login, for one), so the options that make a tool
//! read a file accept only the caller's standard input or a file the caller passed. The caller's
//! side opens each such file itself, as the caller, passes it as a descriptor, and writes a
//! placeholder in its argument; the gateway swaps in the descriptor's number, so the command reads
//! `/dev/fd/N`. Callers keep writing `gh pr edit 7 --body-file /tmp/notes.md`.

use std::fs::File;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::path::Path;

use anyhow::{Context as _, Result, bail};

/// The placeholder for passed file `index`, which the gateway turns into `/dev/fd/N`.
fn placeholder(index: usize) -> String {
    format!("/dev/fd/@{index}")
}

/// The options of `gh` that read a file, for its command group: options whose whole value names
/// a file, and options whose value reads a file after `@`.
fn gh_file_options(group: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match group {
        "api" => (&["--input"], &["-F", "--field"]),
        "workflow" => (&[], &["-F", "--field"]),
        "variable" | "secret" => (&["--env-file", "-f"], &[]),
        _ => (&["--body-file", "-F"], &[]),
    }
}

/// Open each file `argv` asks a tool to read and replace its path with a placeholder. Paths are
/// relative to `cwd`. Standard input (`-`) and descriptors stay as they are. Only tools sekrets
/// knows are rewritten; any other tool's file options are judged as written.
pub fn pass_files(argv: &mut [String], cwd: &Path) -> Result<Vec<OwnedFd>> {
    let mut files = Vec::new();
    if argv.first().map(String::as_str) != Some("gh") {
        return Ok(files);
    }
    let group = argv.get(1).cloned().unwrap_or_default();
    let (file_options, field_options) = gh_file_options(&group);
    let mut index = 1;
    while index < argv.len() {
        let arg = argv[index].clone();
        for (option, field) in file_options
            .iter()
            .map(|option| (*option, false))
            .chain(field_options.iter().map(|option| (*option, true)))
        {
            // Where this argument's value is: the next argument, or after `=` or the letter.
            let (value_index, prefix) = if arg == option {
                (index + 1, String::new())
            } else if option.starts_with("--") && arg.starts_with(&format!("{option}=")) {
                (index, format!("{option}="))
            } else if !option.starts_with("--") && arg.len() > 2 && arg.starts_with(option) {
                (index, option.to_owned())
            } else {
                continue;
            };
            let Some(value) = argv.get(value_index).cloned() else {
                break;
            };
            let value = value[if value_index == index {
                prefix.len()
            } else {
                0
            }..]
                .to_owned();
            let (before, path) = if field {
                match value.split_once("=@") {
                    Some((key, path)) => (format!("{key}=@"), path.to_owned()),
                    None => match value.strip_prefix('@') {
                        Some(path) => ("@".to_owned(), path.to_owned()),
                        None => break,
                    },
                }
            } else {
                (String::new(), value.clone())
            };
            if path == "-" || path.starts_with("/dev/fd/") {
                break;
            }
            let file = File::open(cwd.join(&path))
                .with_context(|| format!("open {path} to pass it to the command"))?;
            if !file.metadata()?.is_file() {
                bail!("{path} is not a file; a command reads only a file you pass it");
            }
            let replaced = format!("{before}{}", placeholder(files.len()));
            files.push(OwnedFd::from(file));
            argv[value_index] = if value_index == index {
                format!("{prefix}{replaced}")
            } else {
                replaced
            };
            break;
        }
        index += 1;
    }
    Ok(files)
}

/// Turn each placeholder in `argv` into the descriptor number of the passed file it names. Every
/// passed file must be used and every placeholder must name one.
pub fn substitute(argv: &mut [String], files: &[OwnedFd]) -> Result<(), String> {
    let mut used = vec![false; files.len()];
    for arg in argv.iter_mut() {
        while let Some(start) = arg.find("/dev/fd/@") {
            let digits = arg[start + "/dev/fd/@".len()..]
                .bytes()
                .take_while(u8::is_ascii_digit)
                .count();
            let number = &arg[start + "/dev/fd/@".len()..start + "/dev/fd/@".len() + digits];
            let Some(index) = number.parse::<usize>().ok().filter(|i| *i < files.len()) else {
                return Err(format!(
                    "an argument names a file that was not passed: {arg}"
                ));
            };
            used[index] = true;
            let real = format!("/dev/fd/{}", files[index].as_raw_fd());
            arg.replace_range(start..start + "/dev/fd/@".len() + digits, &real);
        }
    }
    if used.iter().any(|used| !used) {
        return Err("a passed file is named by no argument".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn gh_file_arguments_become_passed_files() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("notes.md"), "body").unwrap();
        for (given, expected) in [
            (
                "gh pr edit 7 --body-file notes.md",
                "gh pr edit 7 --body-file /dev/fd/@0",
            ),
            (
                "gh pr create --body-file=notes.md",
                "gh pr create --body-file=/dev/fd/@0",
            ),
            (
                "gh issue comment 3 -F notes.md",
                "gh issue comment 3 -F /dev/fd/@0",
            ),
            ("gh pr create -Fnotes.md", "gh pr create -F/dev/fd/@0"),
            (
                "gh api repos/x/y --input notes.md",
                "gh api repos/x/y --input /dev/fd/@0",
            ),
            (
                "gh api graphql -F body=@notes.md",
                "gh api graphql -F body=@/dev/fd/@0",
            ),
            ("gh api graphql -F n=12", "gh api graphql -F n=12"),
            ("gh pr create --body-file -", "gh pr create --body-file -"),
            ("gh pr view 7 --json body", "gh pr view 7 --json body"),
            ("git commit -F notes.md", "git commit -F notes.md"),
        ] {
            let mut args = argv(given);
            let files = pass_files(&mut args, directory.path()).unwrap();
            assert_eq!(args.join(" "), expected, "{given}");
            assert_eq!(
                files.len(),
                usize::from(expected.contains("/dev/fd/@")),
                "{given}"
            );
        }
        let mut missing = argv("gh pr edit 7 --body-file absent.md");
        assert!(pass_files(&mut missing, directory.path()).is_err());
    }

    #[test]
    fn the_gateway_swaps_in_descriptor_numbers_and_refuses_strays() {
        let file: OwnedFd = tempfile::tempfile().unwrap().into();
        let number = file.as_raw_fd();
        let mut args = argv("gh api graphql -F body=@/dev/fd/@0 --body-file=/dev/fd/@0");
        substitute(&mut args, std::slice::from_ref(&file)).unwrap();
        assert_eq!(
            args.join(" "),
            format!("gh api graphql -F body=@/dev/fd/{number} --body-file=/dev/fd/{number}")
        );
        assert!(substitute(&mut argv("gh x /dev/fd/@1"), std::slice::from_ref(&file)).is_err());
        assert!(substitute(&mut argv("gh pr view"), std::slice::from_ref(&file)).is_err());
    }
}
