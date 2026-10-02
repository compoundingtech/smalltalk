//! Command policy: which argument vectors a profile, or a grant of it, lets a caller run.
//!
//! Rules match whole arguments, never text inside them. An argument vector is allowed when an
//! allow prefix matches it and no deny rule does. A prefix element is one literal argument, or
//! `*` for exactly one argument of any value. So options before a subcommand (`gh -R x pr
//! create`) match no allow prefix unless one names them. A deny rule with options denies only
//! when one of those options appears after its prefix, which is how a preset refuses the flags
//! that make a tool read a file (`gh pr create --body-file`).

use std::fmt;

use serde::{Deserialize, Serialize};

/// One rule: a prefix of whole arguments, and for a deny rule, the options it refuses.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub prefix: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

impl Rule {
    pub fn prefix(words: &[&str]) -> Self {
        Self {
            prefix: words.iter().map(|word| (*word).to_owned()).collect(),
            options: Vec::new(),
        }
    }

    pub fn options(words: &[&str], options: &[&str]) -> Self {
        Self {
            prefix: words.iter().map(|word| (*word).to_owned()).collect(),
            options: options.iter().map(|option| (*option).to_owned()).collect(),
        }
    }

    /// Parse `gh pr create` or `gh pr create --body-file,-F`: a prefix, then an optional
    /// comma-separated list of options after a space-separated word that starts with `-`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut prefix = Vec::new();
        let mut options = Vec::new();
        for word in text.split_whitespace() {
            if word.starts_with('-') {
                options.extend(word.split(',').filter(|o| !o.is_empty()).map(str::to_owned));
            } else if options.is_empty() {
                prefix.push(word.to_owned());
            } else {
                return Err(format!(
                    "rule `{text}`: options come after the whole prefix, not between its words"
                ));
            }
        }
        if prefix.is_empty() {
            return Err(format!("rule `{text}` names no command"));
        }
        for option in &options {
            if option == "-" || option == "--" {
                return Err(format!("rule `{text}`: `{option}` is not an option"));
            }
        }
        Ok(Self { prefix, options })
    }

    fn matches_prefix(&self, argv: &[String]) -> bool {
        self.prefix.len() <= argv.len()
            && self
                .prefix
                .iter()
                .zip(argv)
                .all(|(word, arg)| word == "*" || word == arg)
    }

    /// The option of this rule that appears after its prefix, if any.
    fn option_in<'a>(&self, argv: &'a [String]) -> Option<&'a str> {
        argv[self.prefix.len()..]
            .iter()
            .find(|arg| {
                self.options
                    .iter()
                    .any(|option| option_matches(option, arg))
            })
            .map(String::as_str)
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.prefix.join(" "))?;
        if !self.options.is_empty() {
            write!(f, " {}", self.options.join(","))?;
        }
        Ok(())
    }
}

/// Whether `arg` uses `option`. A long option matches itself and `--name=value`. A short option
/// matches any single-dash argument that contains its letter, because tools accept bundled short
/// flags (`-dF file`); this refuses some arguments that only look like flags, never fewer.
fn option_matches(option: &str, arg: &str) -> bool {
    if let Some(name) = option.strip_prefix("--") {
        let Some(given) = arg.strip_prefix("--") else {
            return false;
        };
        given == name
            || given
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with('='))
    } else if let Some(letters) = option.strip_prefix('-') {
        arg.starts_with('-')
            && !arg.starts_with("--")
            && letters.chars().all(|letter| arg[1..].contains(letter))
    } else {
        arg == option
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub allow: Vec<Rule>,
    #[serde(default)]
    pub deny: Vec<Rule>,
    /// Presets named when the policy was set, expanded into `allow` and `deny`; kept to show.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    Refused(String),
}

impl Policy {
    /// A policy from presets and extra rules. Unknown presets are an error.
    pub fn build(presets: &[String], allow: &[String], deny: &[String]) -> Result<Self, String> {
        let mut policy = Policy::default();
        for name in presets {
            let preset = preset(name).ok_or_else(|| {
                format!(
                    "unknown preset `{name}`; presets are {}",
                    PRESETS.iter().map(|p| p.0).collect::<Vec<_>>().join(", ")
                )
            })?;
            policy.allow.extend(preset.allow);
            policy.deny.extend(preset.deny);
            policy.presets.push(name.clone());
        }
        for text in allow {
            let rule = Rule::parse(text)?;
            if !rule.options.is_empty() {
                return Err(format!(
                    "allow rule `{text}` lists options; refuse options with a deny rule"
                ));
            }
            policy.allow.push(rule);
        }
        for text in deny {
            policy.deny.push(Rule::parse(text)?);
        }
        Ok(policy)
    }

    /// Allow first, then deny.
    pub fn judge(&self, argv: &[String]) -> Verdict {
        if argv.is_empty() {
            return Verdict::Refused("no command".into());
        }
        if !self.allow.iter().any(|rule| rule.matches_prefix(argv)) {
            return Verdict::Refused(format!(
                "no allow rule matches `{}`",
                shown(argv, self.allow.iter().map(|r| r.prefix.len()).max())
            ));
        }
        for rule in &self.deny {
            if !rule.matches_prefix(argv) {
                continue;
            }
            if rule.options.is_empty() {
                return Verdict::Refused(format!("denied by rule `{rule}`"));
            }
            if let Some(option) = rule.option_in(argv) {
                return Verdict::Refused(format!("option `{option}` denied by rule `{rule}`"));
            }
        }
        Verdict::Allowed
    }

    /// Whether some allow rule lets through every invocation of a command, such as `*` or `gh`
    /// alone, rather than listing subcommands. A grant to an agent must not.
    pub fn broad_allow(&self) -> Option<&Rule> {
        self.allow
            .iter()
            .find(|rule| rule.prefix.len() < 2 || rule.prefix.iter().take(2).any(|w| w == "*"))
    }
}

/// The command as a refusal shows it: no more words than the longest allow prefix plus one, so a
/// refusal never echoes a long argument (a token pasted as an argument, for instance).
fn shown(argv: &[String], longest: Option<usize>) -> String {
    let words = longest.unwrap_or(1).max(1) + 1;
    let mut text = argv
        .iter()
        .take(words)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    if argv.len() > words {
        text.push_str(" …");
    }
    text
}

pub struct Preset {
    pub allow: Vec<Rule>,
    pub deny: Vec<Rule>,
}

/// The presets, with a one-line description each.
pub const PRESETS: &[(&str, &str)] = &[
    (
        "everything",
        "any command; for a person's own profile, with deny rules or the credential presets",
    ),
    (
        "no-credential-printing",
        "denies the gh and git subcommands that print or change stored credentials or run other programs",
    ),
    (
        "gh-read",
        "read pull requests, issues, runs, releases and repositories with gh",
    ),
    (
        "gh-pr",
        "gh-read, plus create, edit, comment on, ready and close pull requests; no file-reading flags",
    ),
    (
        "git-push",
        "git push to a remote; no mirror, prune or custom receive-pack",
    ),
];

/// Options that make gh read a local file into what it sends.
const GH_FILE_OPTIONS: &[&str] = &[
    "--body-file",
    "-F",
    "--template",
    "-T",
    "--recover",
    "--editor",
    "-e",
];

pub fn preset(name: &str) -> Option<Preset> {
    let gh_read = || {
        let mut allow = Vec::new();
        for verb in ["view", "list", "diff", "checks", "status"] {
            allow.push(Rule::prefix(&["gh", "pr", verb]));
        }
        for verb in ["view", "list", "status"] {
            allow.push(Rule::prefix(&["gh", "issue", verb]));
        }
        for verb in ["view", "list"] {
            allow.push(Rule::prefix(&["gh", "run", verb]));
            allow.push(Rule::prefix(&["gh", "release", verb]));
        }
        allow.push(Rule::prefix(&["gh", "repo", "view"]));
        allow
    };
    Some(match name {
        "everything" => Preset {
            allow: vec![Rule::prefix(&["*"])],
            deny: Vec::new(),
        },
        "no-credential-printing" => Preset {
            allow: Vec::new(),
            deny: vec![
                Rule::prefix(&["gh", "auth", "token"]),
                Rule::options(&["gh", "auth", "status"], &["--show-token", "-t"]),
                Rule::prefix(&["gh", "alias"]),
                Rule::prefix(&["gh", "extension"]),
                Rule::prefix(&["gh", "ext"]),
                Rule::prefix(&["gh", "config"]),
                Rule::prefix(&["gh", "api"]),
                Rule::prefix(&["git", "credential"]),
                Rule::prefix(&["git", "credential-store"]),
                Rule::prefix(&["git", "credential-cache"]),
                Rule::prefix(&["git", "config"]),
                Rule::options(&["git"], &["-c", "--config-env", "--exec-path"]),
            ],
        },
        "gh-read" => Preset {
            allow: gh_read(),
            deny: Vec::new(),
        },
        "gh-pr" => {
            let mut allow = gh_read();
            let mut deny = Vec::new();
            for verb in ["create", "edit", "comment", "ready", "close", "reopen"] {
                allow.push(Rule::prefix(&["gh", "pr", verb]));
                deny.push(Rule::options(&["gh", "pr", verb], GH_FILE_OPTIONS));
            }
            Preset { allow, deny }
        }
        "git-push" => Preset {
            allow: vec![Rule::prefix(&["git", "push"])],
            deny: vec![Rule::options(
                &["git", "push"],
                &["--mirror", "--prune", "--receive-pack", "--exec"],
            )],
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    fn policy(presets: &[&str], allow: &[&str], deny: &[&str]) -> Policy {
        Policy::build(
            &presets.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>(),
            &allow.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>(),
            &deny.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn allow_prefixes_match_whole_arguments_only() {
        let policy = policy(&[], &["gh pr create"], &[]);
        assert_eq!(
            policy.judge(&argv("gh pr create --draft")),
            Verdict::Allowed
        );
        assert!(matches!(
            policy.judge(&argv("gh pr creates")),
            Verdict::Refused(_)
        ));
        assert!(matches!(policy.judge(&argv("gh pr")), Verdict::Refused(_)));
        assert!(matches!(
            policy.judge(&argv("ghx pr create")),
            Verdict::Refused(_)
        ));
    }

    #[test]
    fn options_before_the_subcommand_are_refused_unless_allowed() {
        let policy = policy(&["gh-pr"], &[], &[]);
        assert!(matches!(
            policy.judge(&argv("gh -R other/repo pr create")),
            Verdict::Refused(_)
        ));
        assert!(matches!(
            policy.judge(&argv("gh --repo=other/repo pr view 1")),
            Verdict::Refused(_)
        ));
    }

    #[test]
    fn deny_follows_allow_and_options_match_long_short_and_bundled_forms() {
        let policy = policy(&["gh-pr"], &[], &[]);
        assert_eq!(
            policy.judge(&argv("gh pr create --draft --title x")),
            Verdict::Allowed
        );
        for refused in [
            "gh pr create --body-file notes",
            "gh pr create --body-file=notes",
            "gh pr create -F notes",
            "gh pr create -Fnotes",
            "gh pr create -dF notes",
            "gh pr edit 3 --template ../x",
            "gh pr comment 3 --editor",
        ] {
            let Verdict::Refused(reason) = policy.judge(&argv(refused)) else {
                panic!("{refused} was allowed");
            };
            assert!(reason.contains("denied by rule"), "{refused}: {reason}");
        }
        assert!(matches!(
            policy.judge(&argv("gh auth token")),
            Verdict::Refused(_)
        ));
        assert!(matches!(
            policy.judge(&argv("gh api /user")),
            Verdict::Refused(_)
        ));
    }

    #[test]
    fn everything_except_refuses_the_named_ways_to_print_a_credential() {
        let policy = policy(&["everything", "no-credential-printing"], &[], &[]);
        assert_eq!(policy.judge(&argv("gh pr list")), Verdict::Allowed);
        assert_eq!(policy.judge(&argv("gh auth status")), Verdict::Allowed);
        assert_eq!(
            policy.judge(&argv("git push origin HEAD")),
            Verdict::Allowed
        );
        for refused in [
            "gh auth token",
            "gh auth status -t",
            "gh auth status --show-token",
            "gh alias set x y",
            "gh extension install a/b",
            "git -c credential.helper=x push",
            "git credential fill",
        ] {
            assert!(
                matches!(policy.judge(&argv(refused)), Verdict::Refused(_)),
                "{refused} was allowed"
            );
        }
    }

    #[test]
    fn a_refusal_never_echoes_long_arguments() {
        let policy = policy(&[], &["gh pr view"], &[]);
        let Verdict::Refused(reason) =
            policy.judge(&argv("gh secret set NAME --body ghp_abcdefghijklmnop"))
        else {
            panic!("allowed");
        };
        assert!(!reason.contains("ghp_"), "{reason}");
    }

    #[test]
    fn rules_parse_prefixes_then_options() {
        assert_eq!(
            Rule::parse("gh pr create --body-file,-F").unwrap(),
            Rule::options(&["gh", "pr", "create"], &["--body-file", "-F"])
        );
        assert!(Rule::parse("gh --repo x pr").is_err());
        assert!(Rule::parse("--draft").is_err());
        assert!(Policy::build(&[], &["gh pr create --draft".into()], &[]).is_err());
        assert!(Policy::build(&["nope".into()], &[], &[]).is_err());
    }

    #[test]
    fn broad_allow_rules_are_found() {
        assert!(policy(&["everything"], &[], &[]).broad_allow().is_some());
        assert!(policy(&[], &["gh"], &[]).broad_allow().is_some());
        assert!(policy(&[], &["gh *"], &[]).broad_allow().is_some());
        assert!(
            policy(&["gh-pr", "git-push"], &[], &[])
                .broad_allow()
                .is_none()
        );
    }
}
