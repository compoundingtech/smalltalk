//! What one pane shows, addressed by a key naming its kind and subject: `mission:mission/…`,
//! `agent:agent/…`, `list:missions`. Today's screens draw through these, and glasses will keep
//! the keys, so any view can be opened in any rectangle by its key alone.

/// The lists the sidebar shows, in tab order.
const LISTS: [&str; 6] = ["home", "agents", "missions", "fleet", "usage", "worktrees"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pane {
    /// A tab's list, as the sidebar shows it, by tab index.
    List(usize),
    /// A Home item's card, by attention id.
    Home(Option<String>),
    /// An agent's conversation and details.
    Agent(Option<String>),
    /// An agent's attached terminal.
    Terminal(String),
    Mission(Option<String>),
    /// A mission's whole declaration.
    Declaration(Option<String>),
    /// A fleet machine, by its graph id (`machine/NAME`).
    Machine(Option<String>),
    Worktree(Option<String>),
    /// Token spend: one group's (`agent/…`, `mission/…`, `model/…` as the Usage list names
    /// them), or the whole period's.
    Usage(Option<String>),
    /// The new mission form.
    NewMission,
}

impl Pane {
    pub fn key(&self) -> String {
        let with = |kind: &str, subject: &Option<String>| {
            format!("{kind}:{}", subject.as_deref().unwrap_or_default())
        };
        match self {
            Pane::List(tab) => format!("list:{}", LISTS.get(*tab).unwrap_or(&LISTS[0])),
            Pane::Home(subject) => with("home", subject),
            Pane::Agent(subject) => with("agent", subject),
            Pane::Terminal(agent) => format!("terminal:{agent}"),
            Pane::Mission(subject) => with("mission", subject),
            Pane::Declaration(subject) => with("declaration", subject),
            Pane::Machine(subject) => with("machine", subject),
            Pane::Worktree(subject) => with("worktree", subject),
            Pane::Usage(subject) => with("usage", subject),
            Pane::NewMission => "new-mission:".into(),
        }
    }

    /// The pane a key names, or `None` for a key no stui knows.
    pub fn parse(key: &str) -> Option<Pane> {
        let (kind, subject) = key.split_once(':')?;
        let subject = (!subject.is_empty()).then(|| subject.to_owned());
        Some(match kind {
            "list" => Pane::List(
                LISTS
                    .iter()
                    .position(|list| Some(*list) == subject.as_deref())?,
            ),
            "home" => Pane::Home(subject),
            "agent" => Pane::Agent(subject),
            "terminal" => Pane::Terminal(subject?),
            "mission" => Pane::Mission(subject),
            "declaration" => Pane::Declaration(subject),
            "machine" => Pane::Machine(subject),
            "worktree" => Pane::Worktree(subject),
            "usage" => Pane::Usage(subject),
            "new-mission" => Pane::NewMission,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pane_survives_its_key() {
        let some = |id: &str| Some(id.to_owned());
        for pane in [
            Pane::List(0),
            Pane::List(2),
            Pane::Home(some("attention/review-1")),
            Pane::Agent(some("agent/example/harbor/keeper")),
            Pane::Agent(None),
            Pane::Terminal("agent/example/harbor/keeper".into()),
            Pane::Mission(some("mission/example/harbor/audit")),
            Pane::Declaration(some("mission/example/harbor/audit")),
            Pane::Machine(some("machine/harbor")),
            Pane::Worktree(None),
            Pane::Usage(some("mission/example/harbor/audit")),
            Pane::Usage(None),
            Pane::NewMission,
        ] {
            assert_eq!(
                Pane::parse(&pane.key()),
                Some(pane.clone()),
                "{}",
                pane.key()
            );
        }
        assert_eq!(Pane::List(1).key(), "list:agents");
        assert_eq!(Pane::parse("list:nowhere"), None);
        assert_eq!(Pane::parse("terminal:"), None, "a terminal needs its agent");
        assert_eq!(Pane::parse("a later kind:subject"), None);
        // The new-agent form is gone; a stored tab for it is dropped like any unknown kind.
        assert_eq!(Pane::parse("new-agent:"), None);
    }
}
