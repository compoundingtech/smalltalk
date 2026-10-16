//! `st demo play FILE`: a seeded, scripted demo of st in the terminal interface, in its own
//! instance, the same every time and without a single model token.
//!
//! A demo is one KDL file: a `demo` header, a `seed` (the agents, missions, runs, conversations,
//! messages and documents the story starts from) and `beats`. A beat does one visible thing,
//! such as an agent's message arriving, a pane opening, a step moving or an alert appearing,
//! after a delay or when the presenter presses the advance key. See docs/st3/demos.md.
//!
//! The scripted agents are real st agents whose process does nothing. Every beat is the `st`
//! command a real seat would run (`st ui open`, `st work claim`, `st work ask`, a timeline claim
//! for what the agent says), run as that agent. Only the presenter's own hands, the advance key
//! and typing into the message input, reach the interface directly.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail, ensure};
use kdl::{KdlDocument, KdlNode, KdlValue};
use serde_json::{Value, json};

/// The graph declarations a seed may hold besides the demo's own actions.
const GRAPH_NODES: &[&str] = &["agent", "mission", "person"];

/// One demo file, checked.
#[derive(Clone, Debug, PartialEq)]
pub struct Demo {
    pub name: String,
    pub title: Option<String>,
    /// `person/NAME`: whose terminal interface the demo plays in.
    pub me: String,
    /// The name this machine has in the demo.
    pub machine: String,
    /// The key that moves to the next beat (`f8` unless the file names another).
    pub advance: String,
    /// The `st apply` publication: `version 2` and the seed's agents, missions and people.
    pub graph: String,
    /// Every scripted agent, `agent/NAME`.
    pub agents: Vec<String>,
    pub seed: Vec<Action>,
    pub beats: Vec<Beat>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Beat {
    pub wait: Wait,
    pub action: Action,
    /// Where the beat is in the file, for the presenter's log.
    pub line: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Wait {
    /// Until the presenter presses the advance key.
    Key,
    /// This long after the previous beat; the advance key cuts it short.
    For(Duration),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Tab,
    Right,
    Below,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepVerb {
    Claim,
    Progress,
    Complete,
    Fail,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Show an agent, mission or machine, as a seat does with `st ui open`.
    Open {
        subject: String,
        place: Place,
        focus: bool,
        by: Option<String>,
    },
    /// The agent says something in its conversation; in a beat it streams in.
    Say {
        agent: String,
        text: String,
        ago: Option<Duration>,
        words_per_second: Option<f64>,
    },
    /// The agent runs a tool, which finishes after `takes`.
    Tool {
        agent: String,
        name: String,
        input: Option<String>,
        output: Option<String>,
        failed: bool,
        takes: Duration,
        ago: Option<Duration>,
    },
    /// An st message between agents, or from the person to an agent.
    Send {
        from: String,
        to: String,
        subject: Option<String>,
        text: String,
        attach: Vec<PathBuf>,
    },
    /// Start a run of a seeded mission, as the person.
    Run { mission: String, id: String },
    /// A mission step moves: claimed, progressed, completed or failed by its agent.
    Step {
        verb: StepVerb,
        agent: String,
        run: String,
        step: String,
        summary: Option<String>,
    },
    /// An alert: the agent asks the person a question with named answers.
    Ask {
        agent: String,
        title: String,
        question: String,
        why: String,
        answers: Vec<Answer>,
        /// Ask from a claimed step (`run` and `step`) instead of a run of its own.
        step: Option<(String, String)>,
    },
    /// Store a document under a name.
    Document { name: String, file: PathBuf },
    /// Type into the focused agent's message input, at a person's pace, and maybe send it.
    Type {
        text: String,
        per_second: f64,
        send: bool,
    },
    /// Nothing: a beat that only waits.
    Pause,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    pub id: String,
    pub label: String,
    pub outcome: String,
    pub consequence: String,
}

impl Action {
    /// What the presenter's log calls this beat.
    pub fn describe(&self) -> String {
        match self {
            Action::Open { subject, .. } => format!("open {subject}"),
            Action::Say { agent, .. } => format!("{agent} says"),
            Action::Tool { agent, name, .. } => format!("{agent} runs {name}"),
            Action::Send { from, to, .. } => format!("{from} messages {to}"),
            Action::Run { id, .. } => format!("run {id} starts"),
            Action::Step {
                verb, agent, step, ..
            } => format!("{agent} {verb:?} {step}").to_lowercase(),
            Action::Ask { agent, title, .. } => format!("{agent} asks: {title}"),
            Action::Document { name, .. } => format!("document {name}"),
            Action::Type { send, .. } => {
                if *send {
                    "type and send".into()
                } else {
                    "type".into()
                }
            }
            Action::Pause => "pause".into(),
        }
    }
}

/// Read and check a demo file. Relative paths in it are relative to the file.
pub fn load(path: &Path) -> Result<Demo> {
    let source =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let base = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    parse(&source, &base).with_context(|| format!("{} is not a demo st can play", path.display()))
}

pub fn parse(source: &str, base: &Path) -> Result<Demo> {
    let document: KdlDocument = source.parse().map_err(|error: kdl::KdlError| {
        anyhow::anyhow!("{}", kdl_error(&error))
    })?;
    let line_of = |node: &KdlNode| line(source, node.span().offset());
    let mut header = None;
    let mut seed_nodes = None;
    let mut beat_nodes = None;
    for node in document.nodes() {
        match node.name().value() {
            "version" => {}
            "demo" => set_once(&mut header, node, "demo")?,
            "seed" => set_once(&mut seed_nodes, node, "seed")?,
            "beats" => set_once(&mut beat_nodes, node, "beats")?,
            other => bail!(
                "line {}: `{other}` is not part of a demo; a demo has `demo`, `seed` and `beats`",
                line_of(node)
            ),
        }
    }
    let header = header.context("a demo starts with `demo \"NAME\" { … }`")?;
    let name = first_string(header).context("`demo` needs a name, as in demo \"release-day\"")?;
    let header_children = children(header);
    let me = child_value(header_children, "me")?
        .context("`demo` needs `me \"NAME\"`: the invented person whose screen this is")?;
    let me = qualified("person", &me);
    let machine = child_value(header_children, "machine")?.unwrap_or_else(|| "studio".into());
    crate::instance::validate(&machine)
        .map_err(|_| anyhow::anyhow!("`machine` is 1 to 24 lowercase letters, digits and hyphens"))?;
    let advance = child_value(header_children, "advance")?.unwrap_or_else(|| "f8".into());
    stui::playback::Advance::parse(&advance).map_err(anyhow::Error::msg)?;
    let title = child_value(header_children, "title")?;
    for node in header_children {
        ensure!(
            ["me", "machine", "advance", "title"].contains(&node.name().value()),
            "line {}: `demo` takes me, machine, advance and title, not `{}`",
            line_of(node),
            node.name().value()
        );
    }

    let mut graph = KdlDocument::new();
    let mut agents = Vec::new();
    let mut seed = Vec::new();
    for node in seed_nodes.map(children).unwrap_or_default() {
        let at = line_of(node);
        if GRAPH_NODES.contains(&node.name().value()) {
            if node.name().value() == "agent" {
                let agent = first_string(node)
                    .with_context(|| format!("line {at}: `agent` needs a name"))?;
                ensure!(
                    agent.contains('/') && !agent.starts_with("agent/"),
                    "line {at}: name an agent with a slash and without agent/, as in agent \"harbor/builder\""
                );
                agents.push(format!("agent/{agent}"));
            }
            graph.nodes_mut().push(node.clone());
            continue;
        }
        let action = action(node, base).with_context(|| format!("line {at}"))?;
        ensure!(
            !matches!(action, Action::Open { .. } | Action::Type { .. } | Action::Pause),
            "line {at}: `{}` is a beat; the seed is what is there before the demo starts",
            node.name().value()
        );
        seed.push(action);
    }
    ensure!(
        !agents.is_empty(),
        "the seed declares no agents: add one, as in agent \"harbor/builder\" {{ name \"Builder\" }}"
    );

    let mut beats = Vec::new();
    for node in beat_nodes.map(children).unwrap_or_default() {
        let at = line_of(node);
        let wait = match node.get("after") {
            None => Wait::Key,
            Some(value) => {
                let value = value
                    .as_string()
                    .with_context(|| format!("line {at}: `after` is \"key\" or a time such as \"1.5s\""))?;
                if value == "key" {
                    Wait::Key
                } else {
                    Wait::For(duration(value).with_context(|| format!("line {at}"))?)
                }
            }
        };
        let action = action(node, base).with_context(|| format!("line {at}"))?;
        beats.push(Beat {
            wait,
            action,
            line: at,
        });
    }
    ensure!(!beats.is_empty(), "the demo has no beats");

    let known: BTreeSet<&str> = agents.iter().map(String::as_str).collect();
    for action in seed.iter().chain(beats.iter().map(|beat| &beat.action)) {
        for agent in action.agents() {
            ensure!(
                known.contains(agent.as_str()),
                "{} is not an agent of this demo's seed (it declares {})",
                agent,
                agents.join(", ")
            );
        }
    }

    let mut published = format!("version 2\n\n{graph}");
    if !published.ends_with('\n') {
        published.push('\n');
    }
    Ok(Demo {
        name,
        title,
        me,
        machine,
        advance,
        graph: published,
        agents,
        seed,
        beats,
    })
}

impl Action {
    /// The scripted agents this action acts as.
    fn agents(&self) -> Vec<String> {
        match self {
            Action::Say { agent, .. }
            | Action::Tool { agent, .. }
            | Action::Step { agent, .. }
            | Action::Ask { agent, .. } => vec![agent.clone()],
            Action::Send { from, .. } if from.starts_with("agent/") => vec![from.clone()],
            Action::Open { by: Some(by), .. } => vec![by.clone()],
            _ => Vec::new(),
        }
    }
}

fn action(node: &KdlNode, base: &Path) -> Result<Action> {
    let name = node.name().value();
    let allowed: &[&str] = match name {
        "open" | "focus" => &["after", "split", "by"],
        "say" => &["after", "ago", "speed"],
        "tool" => &["after", "ago", "input", "output", "failed", "takes"],
        "send" => &["after", "from", "to", "subject", "attach"],
        "run" => &["after", "id"],
        "claim" | "progress" | "complete" | "fail" => &["after", "run", "step"],
        "ask" => &["after", "title", "why", "run", "step"],
        "document" => &["after", "file"],
        "type" => &["after", "speed", "send"],
        "pause" => &["after"],
        other => bail!(
            "`{other}` is not a demo action; use open, focus, say, tool, send, run, claim, progress, complete, fail, ask, document, type or pause"
        ),
    };
    for entry in node.entries() {
        if let Some(key) = entry.name() {
            ensure!(
                allowed.contains(&key.value()),
                "`{name}` takes {}, not `{}`",
                allowed.join(", "),
                key.value()
            );
        }
    }
    let arguments = node
        .entries()
        .iter()
        .filter(|entry| entry.name().is_none())
        .map(|entry| {
            entry
                .value()
                .as_string()
                .map(str::to_owned)
                .with_context(|| format!("`{name}` takes text in quotes"))
        })
        .collect::<Result<Vec<_>>>()?;
    let string = |key: &str| -> Result<Option<String>> {
        node.get(key)
            .map(|value| {
                value
                    .as_string()
                    .map(str::to_owned)
                    .with_context(|| format!("`{key}` takes text in quotes"))
            })
            .transpose()
    };
    let flag = |key: &str| -> Result<bool> {
        node.get(key)
            .map(|value| {
                value
                    .as_bool()
                    .with_context(|| format!("`{key}` is #true or #false"))
            })
            .transpose()
            .map(Option::unwrap_or_default)
    };
    let ago = string("ago")?.map(|value| duration(&value)).transpose()?;
    let positional = |count: usize, shape: &str| -> Result<&[String]> {
        ensure!(arguments.len() == count, "write it as {shape}");
        Ok(&arguments[..])
    };
    let text = |index: usize| -> String { unindent(&arguments[index]) };
    Ok(match name {
        "open" | "focus" => {
            let [subject] = positional(1, &format!("{name} \"agent/NAME\""))? else {
                unreachable!()
            };
            let place = match string("split")?.as_deref() {
                None => Place::Tab,
                Some("right") => Place::Right,
                Some("below") => Place::Below,
                Some(other) => bail!("`split` is \"right\" or \"below\", not `{other}`"),
            };
            stui_subject(subject)?;
            Action::Open {
                subject: subject.clone(),
                place,
                focus: true,
                by: string("by")?.map(|by| qualified("agent", &by)),
            }
        }
        "say" => {
            positional(2, "say \"agent/NAME\" \"what it says\"")?;
            let words_per_second = string("speed")?
                .map(|value| rate(&value, "words"))
                .transpose()?;
            Action::Say {
                agent: qualified("agent", &arguments[0]),
                text: text(1),
                ago,
                words_per_second,
            }
        }
        "tool" => {
            positional(2, "tool \"agent/NAME\" \"Bash\"")?;
            Action::Tool {
                agent: qualified("agent", &arguments[0]),
                name: arguments[1].clone(),
                input: string("input")?,
                output: string("output")?.map(|output| unindent(&output)),
                failed: flag("failed")?,
                takes: string("takes")?
                    .map(|value| duration(&value))
                    .transpose()?
                    .unwrap_or(Duration::from_millis(1500)),
                ago,
            }
        }
        "send" => {
            positional(1, "send from=\"agent/A\" to=\"agent/B\" \"the message\"")?;
            let from = string("from")?.context("`send` needs from=\"agent/NAME\" or from=\"person/NAME\"")?;
            let to = string("to")?.context("`send` needs to=\"agent/NAME\"")?;
            ensure!(
                to.starts_with("agent/"),
                "people have no inbox: send to an agent/…"
            );
            ensure!(
                from.starts_with("agent/") || from.starts_with("person/"),
                "`from` is agent/NAME or person/NAME"
            );
            Action::Send {
                from,
                to,
                subject: string("subject")?,
                text: text(0),
                attach: string("attach")?
                    .map(|file| vec![base.join(file)])
                    .unwrap_or_default(),
            }
        }
        "run" => {
            positional(1, "run \"MISSION\" id=\"MISSION/RUN\"")?;
            let mission = arguments[0]
                .strip_prefix("mission/")
                .unwrap_or(&arguments[0])
                .to_owned();
            let id = string("id")?.context("`run` needs id=\"MISSION/RUN\"")?;
            ensure!(
                id.starts_with(&format!("{mission}/")),
                "a run id starts with its mission: id=\"{mission}/NAME\""
            );
            Action::Run { mission, id }
        }
        "claim" | "progress" | "complete" | "fail" => {
            let verb = match name {
                "claim" => StepVerb::Claim,
                "progress" => StepVerb::Progress,
                "complete" => StepVerb::Complete,
                _ => StepVerb::Fail,
            };
            ensure!(
                (1..=2).contains(&arguments.len()),
                "write it as {name} \"agent/NAME\" run=\"MISSION/RUN\" step=\"STEP\" \"what happened\""
            );
            Action::Step {
                verb,
                agent: qualified("agent", &arguments[0]),
                run: run_id(&string("run")?.context("needs run=\"MISSION/RUN\"")?),
                step: string("step")?.context("needs step=\"STEP\"")?,
                summary: (arguments.len() == 2).then(|| text(1)),
            }
        }
        "ask" => {
            positional(2, "ask \"agent/NAME\" title=\"…\" \"the question\" { answer … }")?;
            let mut answers = Vec::new();
            for answer in node.children().map(KdlDocument::nodes).unwrap_or_default() {
                ensure!(
                    answer.name().value() == "answer",
                    "`ask` holds `answer \"ID\" \"Label\"` lines"
                );
                let values = answer
                    .entries()
                    .iter()
                    .filter(|entry| entry.name().is_none())
                    .filter_map(|entry| entry.value().as_string())
                    .collect::<Vec<_>>();
                let [id, label] = values[..] else {
                    bail!("write an answer as answer \"ok\" \"Ship it\"");
                };
                let outcome = answer
                    .get("outcome")
                    .and_then(KdlValue::as_string)
                    .unwrap_or(if answers.is_empty() { "accept" } else { "decline" })
                    .to_owned();
                ensure!(
                    ["accept", "decline", "defer"].contains(&outcome.as_str()),
                    "an answer's outcome is accept, decline or defer"
                );
                answers.push(Answer {
                    id: id.into(),
                    label: label.into(),
                    outcome,
                    consequence: answer
                        .get("then")
                        .and_then(KdlValue::as_string)
                        .unwrap_or("Nothing else happens; this is a demo.")
                        .into(),
                });
            }
            ensure!(
                (2..=4).contains(&answers.len()),
                "`ask` needs two to four answers"
            );
            let step = match (string("run")?, string("step")?) {
                (Some(run), Some(step)) => Some((run_id(&run), step)),
                (None, None) => None,
                _ => bail!("`ask` names both run= and step=, or neither"),
            };
            let question = text(1);
            Action::Ask {
                agent: qualified("agent", &arguments[0]),
                title: string("title")?.unwrap_or_else(|| question.clone()),
                why: string("why")?
                    .unwrap_or_else(|| "Only you can decide this.".into()),
                question,
                answers,
                step,
            }
        }
        "document" => {
            positional(1, "document \"NAME\" file=\"notes.md\"")?;
            let file = base.join(string("file")?.context("`document` needs file=\"PATH\"")?);
            Action::Document {
                name: qualified("doc", &arguments[0]),
                file,
            }
        }
        "type" => {
            positional(1, "type \"what the person types\"")?;
            Action::Type {
                text: arguments[0].clone(),
                per_second: string("speed")?
                    .map(|value| rate(&value, "chars"))
                    .transpose()?
                    .unwrap_or(14.0),
                send: flag("send")?,
            }
        }
        "pause" => {
            positional(0, "pause after=\"2s\"")?;
            Action::Pause
        }
        _ => unreachable!(),
    })
}

fn stui_subject(subject: &str) -> Result<()> {
    ensure!(
        ["agent/", "mission/", "mission-run/", "machine/", "doc/"]
            .iter()
            .any(|prefix| subject.starts_with(prefix)),
        "open shows agent/…, mission/…, mission-run/…, machine/… or doc/…, not {subject}"
    );
    Ok(())
}

fn run_id(run: &str) -> String {
    run.strip_prefix("mission-run/").unwrap_or(run).to_owned()
}

fn qualified(kind: &str, name: &str) -> String {
    if name.starts_with(&format!("{kind}/")) {
        name.to_owned()
    } else {
        format!("{kind}/{name}")
    }
}

fn set_once<'a>(slot: &mut Option<&'a KdlNode>, node: &'a KdlNode, name: &str) -> Result<()> {
    ensure!(slot.is_none(), "a demo has one `{name}`");
    *slot = Some(node);
    Ok(())
}

fn children(node: &KdlNode) -> &[KdlNode] {
    node.children().map(KdlDocument::nodes).unwrap_or_default()
}

fn first_string(node: &KdlNode) -> Option<String> {
    node.entries()
        .iter()
        .find(|entry| entry.name().is_none())
        .and_then(|entry| entry.value().as_string())
        .map(str::to_owned)
}

fn child_value(nodes: &[KdlNode], name: &str) -> Result<Option<String>> {
    let mut found = nodes.iter().filter(|node| node.name().value() == name);
    let value = found
        .next()
        .map(|node| first_string(node).with_context(|| format!("`{name}` takes text in quotes")))
        .transpose()?;
    ensure!(found.next().is_none(), "`{name}` appears twice");
    Ok(value)
}

fn line(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())].matches('\n').count() + 1
}

fn kdl_error(error: &kdl::KdlError) -> String {
    error
        .diagnostics
        .first()
        .map(|diagnostic| {
            diagnostic
                .message
                .clone()
                .or_else(|| diagnostic.help.clone())
                .unwrap_or_else(|| "it is not valid KDL".into())
        })
        .unwrap_or_else(|| error.to_string())
}

/// `1.5s`, `400ms`, `2m`, `3h`.
fn duration(value: &str) -> Result<Duration> {
    let value = value.trim();
    let (number, unit) = value
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|at| value.split_at(at))
        .with_context(|| format!("`{value}` needs a unit: ms, s, m or h"))?;
    let number: f64 = number
        .parse()
        .with_context(|| format!("`{value}` is not a time such as 1.5s"))?;
    let seconds = match unit {
        "ms" => number / 1000.0,
        "s" => number,
        "m" => number * 60.0,
        "h" => number * 3600.0,
        _ => bail!("`{value}` needs a unit: ms, s, m or h"),
    };
    ensure!(seconds.is_finite() && seconds >= 0.0, "`{value}` is not a time");
    Ok(Duration::from_secs_f64(seconds))
}

/// `12/s`: so many words (or characters) a second.
fn rate(value: &str, what: &str) -> Result<f64> {
    let number = value
        .strip_suffix("/s")
        .and_then(|number| number.trim().parse::<f64>().ok())
        .filter(|number| *number > 0.0 && *number <= 1000.0)
        .with_context(|| format!("`speed` is {what} a second, as in \"12/s\""))?;
    Ok(number)
}

/// Text as the file has it, without blank edges. KDL already removes a multi-line string's
/// indentation, up to its closing quotes.
fn unindent(text: &str) -> String {
    text.trim().to_owned()
}

// ---- Playing -------------------------------------------------------------------------------

/// Where a demo keeps what outlives its daemon: the daemon's pid and the presenter's log.
fn demo_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("st3-demo")
}

/// Reset the instance, seed the demo, then play it in the terminal interface `show` opens.
/// Returns once the presenter quits the interface; the daemon and the agents stop with it.
pub fn play(
    demo: Demo,
    instance: &str,
    show: impl FnOnce(stui::playback::Playback) -> Result<()>,
) -> Result<()> {
    use std::io::IsTerminal as _;
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "a demo plays in the terminal interface: run it in a terminal"
    );
    let exe = std::env::current_exe().context("cannot find this st executable")?;
    let dir = demo_dir();
    fs::create_dir_all(&dir)?;
    stop_daemon(&dir)?;
    let config = crate::config::Config::default();
    // A daemon this player did not start (the instance's service) owns the graph: leave it be.
    ensure!(
        std::os::unix::net::UnixStream::connect(&config.socket).is_err(),
        "the {instance} instance's daemon is running and a demo runs its own: stop it with \
         st --instance {instance} service stop, or play the demo in another instance"
    );
    reset_state(&config)?;
    write_config(&demo)?;
    println!("Resetting the {instance} instance and starting its daemon…");
    let daemon = Daemon::start(&exe, &dir)?;
    let runner = Runner::new(exe.clone(), &demo, &dir)?;
    let outcome = (|| {
        runner.wait_for_daemon()?;
        println!("Seeding {}…", demo.title.as_deref().unwrap_or(&demo.name));
        runner.seed(&demo)?;
        println!(
            "Playing. Press {} for each beat that waits for a key; Ctrl+Q ends the demo.",
            demo.advance.to_uppercase()
        );
        let advance = stui::playback::Advance::parse(&demo.advance).map_err(anyhow::Error::msg)?;
        let (playback, player) = stui::playback::link(advance);
        let beats = demo.beats.clone();
        let playing = std::thread::spawn(move || runner.play(&beats, player));
        let shown = show(playback);
        let played = playing
            .join()
            .map_err(|_| anyhow::anyhow!("the demo player stopped unexpectedly"))?;
        shown?;
        Ok::<_, anyhow::Error>(played)
    })();
    drop(daemon);
    let failures = outcome?;
    if failures.is_empty() {
        println!("The demo ended. Run the same command to play it again from the start.");
    } else {
        eprintln!("Some beats failed; the log is {}:", dir.join("play.log").display());
        for failure in failures {
            eprintln!("  {failure}");
        }
    }
    Ok(())
}

/// Remove the instance's graph and sockets, so the demo starts from nothing.
fn reset_state(config: &crate::config::Config) -> Result<()> {
    if config.state_dir.exists() {
        fs::remove_dir_all(&config.state_dir)
            .with_context(|| format!("cannot reset {}", config.state_dir.display()))?;
    }
    for socket in [&config.socket, &config.client_gateway_socket] {
        let _ = fs::remove_file(socket);
    }
    // The interface's cache: its last picture of the graph and unsent drafts of the last play.
    if let Some(cache) = crate::instance::current().map(|instance| instance.cache_home().join("st3"))
        && cache.exists()
    {
        fs::remove_dir_all(&cache).with_context(|| format!("cannot reset {}", cache.display()))?;
    }
    Ok(())
}

/// The instance's config: the demo's person and machine name, nothing else.
fn write_config(demo: &Demo) -> Result<()> {
    let path = crate::config::Config::default_path();
    fs::create_dir_all(path.parent().context("the config path has no folder")?)?;
    let config = format!(
        "# Written by st demo play for {}; each play rewrites it.\nnode = {}\nperson = {}\naccess_mode = \"auto\"\n",
        demo.name,
        toml_string(&demo.machine),
        toml_string(&demo.me),
    );
    fs::write(&path, config).with_context(|| format!("cannot write {}", path.display()))
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("a string serializes")
}

/// The demo's own daemon: started by the player, stopped when the demo ends.
struct Daemon {
    child: Child,
    pid_file: PathBuf,
}

impl Daemon {
    fn start(exe: &Path, dir: &Path) -> Result<Self> {
        let log = fs::File::create(dir.join("daemon.log"))?;
        let mut command = Command::new(exe);
        command.arg("up");
        if let Some(pty) = pty_beside(exe) {
            command.arg("--pty-binary").arg(pty);
        }
        command
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        scrub(&mut command);
        let child = command.spawn().context("cannot start the demo's daemon")?;
        let pid_file = dir.join("daemon.pid");
        fs::write(&pid_file, child.id().to_string())?;
        Ok(Self { child, pid_file })
    }
}

/// The `pty` the agents run in: beside st, as an install puts it, or on this PATH. Without
/// either, `st up` looks on the login shell's PATH itself.
fn pty_beside(exe: &Path) -> Option<PathBuf> {
    let beside = exe.parent().map(|dir| dir.join("pty"));
    beside.into_iter().chain(
        std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).map(|dir| dir.join("pty")).collect::<Vec<_>>())
            .unwrap_or_default(),
    )
    .find(|candidate| candidate.is_file())
}

impl Drop for Daemon {
    fn drop(&mut self) {
        terminate(self.child.id());
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.pid_file);
    }
}

/// Stop the daemon an earlier play left running (a crashed or still open demo).
fn stop_daemon(dir: &Path) -> Result<()> {
    let pid_file = dir.join("daemon.pid");
    let Ok(pid) = fs::read_to_string(&pid_file) else {
        return Ok(());
    };
    if let Ok(pid) = pid.trim().parse::<u32>()
        && is_demo_daemon(pid)
    {
        terminate(pid);
    }
    let _ = fs::remove_file(pid_file);
    Ok(())
}

/// Whether `pid` is still an `st up` of this user: a reused pid is left alone.
fn is_demo_daemon(pid: u32) -> bool {
    let Ok(output) = Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
    else {
        return false;
    };
    let args = String::from_utf8_lossy(&output.stdout);
    args.split_whitespace().any(|word| word == "up")
        && args.split_whitespace().next().is_some_and(|program| program.contains("st"))
}

/// SIGTERM, and SIGKILL if it is still there after ten seconds.
fn terminate(pid: u32) {
    let pid = pid as libc::pid_t;
    // SAFETY: kill has no memory preconditions; the pid is ours or gone.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        // SAFETY: as above; signal 0 only asks whether the process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        let mut status = 0;
        // SAFETY: a non-blocking wait for our own child, or ECHILD for another's.
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        std::thread::sleep(Duration::from_millis(50));
    }
    // SAFETY: as above.
    unsafe { libc::kill(pid, libc::SIGKILL) };
}

/// The st environment a seat leaves in its process: a demo command acts only as the agent or
/// person it names.
fn scrub(command: &mut Command) {
    for name in [
        "ST_AGENT",
        "ST3_INCARNATION",
        "ST3_ENDPOINT",
        "ST3_PERSON",
        "ST_WORKSPACE",
    ] {
        command.env_remove(name);
    }
}

/// Runs each action as the `st` command a seat would run.
struct Runner {
    exe: PathBuf,
    me: String,
    log: std::sync::Mutex<fs::File>,
    workspace: PathBuf,
    counter: std::sync::atomic::AtomicU64,
    /// Each stored document's reference (`doc/NAME@HASH`), which its pane opens.
    documents: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
    /// Each timeline entry's sequence, and the last sequence each agent's timeline used.
    sequences: std::sync::Mutex<(
        std::collections::BTreeMap<String, u64>,
        std::collections::BTreeMap<String, u64>,
    )>,
}

impl Runner {
    fn new(exe: PathBuf, demo: &Demo, dir: &Path) -> Result<Self> {
        let workspace = crate::config::Config::default()
            .work_root
            .unwrap_or_else(|| crate::instance::current().map_or_else(|| PathBuf::from("."), |instance| instance.work_dir()))
            .join("demo");
        fs::create_dir_all(&workspace)?;
        Ok(Self {
            exe,
            me: demo.me.clone(),
            log: std::sync::Mutex::new(fs::File::create(dir.join("play.log"))?),
            workspace,
            counter: Default::default(),
            documents: Default::default(),
            sequences: Default::default(),
        })
    }

    fn note(&self, text: &str) {
        if let Ok(mut log) = self.log.lock() {
            let _ = writeln!(log, "{} {text}", chrono::Utc::now().format("%H:%M:%S%.3f"));
        }
    }

    /// Run `st ARGS` as `actor` (an agent, or the person when `None`) and return its output.
    fn st(&self, actor: Option<&str>, args: &[&str]) -> Result<String> {
        let mut command = Command::new(&self.exe);
        command.args(args).stdin(Stdio::null());
        scrub(&mut command);
        if let Some(agent) = actor.filter(|actor| actor.starts_with("agent/")) {
            command.env("ST_AGENT", agent);
        }
        let output = command
            .output()
            .with_context(|| format!("cannot run st {}", args.join(" ")))?;
        let said = String::from_utf8_lossy(&output.stdout).into_owned();
        self.note(&format!("st {} → {}", args.join(" "), said.trim()));
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            self.note(&format!("  failed: {}", error.trim()));
            bail!("st {} failed: {}", args.first().unwrap_or(&""), error.trim());
        }
        Ok(said)
    }

    fn json(&self, actor: Option<&str>, args: &[&str]) -> Result<Value> {
        let mut args = args.to_vec();
        args.push("--json");
        let said = self.st(actor, &args)?;
        serde_json::from_str(&said).context("st answered with something other than JSON")
    }

    fn wait_for_daemon(&self) -> Result<()> {
        self.st(None, &["agents", "ls", "--daemon-wait", "60"])
            .context("the demo's daemon did not start; its log is in the instance's state/st3-demo/daemon.log")?;
        Ok(())
    }

    fn seed(&self, demo: &Demo) -> Result<()> {
        let graph = demo_dir().join("seed.kdl");
        fs::write(&graph, self.with_stand_ins(demo)?)?;
        self.st(
            None,
            &["apply", graph.to_str().context("the state path is not UTF-8")?, "--as", &self.me, "--no-gate-check"],
        )?;
        for agent in &demo.agents {
            self.incarnation(agent, Duration::from_secs(30)).with_context(|| {
                format!(
                    "{agent} did not start; `st --instance NAME doctor` says why (st starts agents through your login shell, which must find pty)"
                )
            })?;
            self.harness(agent, "idle")?;
        }
        for action in &demo.seed {
            self.act(action, None)
                .with_context(|| format!("seeding: {}", action.describe()))?;
        }
        Ok(())
    }

    /// The seed's agents with the process each runs in place of a harness: one that waits for
    /// the demo to end and does nothing else.
    fn with_stand_ins(&self, demo: &Demo) -> Result<String> {
        let mut document: KdlDocument = demo.graph.parse().map_err(|error: kdl::KdlError| {
            anyhow::anyhow!("{}", kdl_error(&error))
        })?;
        let exe = self.exe.to_str().context("the st path is not UTF-8")?.to_owned();
        let player = std::process::id().to_string();
        for node in document.nodes_mut() {
            if node.name().value() != "agent" {
                continue;
            }
            let name = first_string(node).unwrap_or_default();
            let body = node.ensure_children();
            body.nodes_mut().retain(|child| {
                !["command", "argv", "workspace", "harness"].contains(&child.name().value())
            });
            let mut workspace = KdlNode::new("workspace");
            workspace.push(
                self.workspace
                    .join(name.replace('/', "-"))
                    .to_string_lossy()
                    .into_owned(),
            );
            body.nodes_mut().push(workspace);
            let mut argv = KdlNode::new("argv");
            for word in [exe.as_str(), "demo", "seat", "--player", player.as_str()] {
                argv.push(word.to_owned());
            }
            body.nodes_mut().push(argv);
        }
        for node in document.nodes() {
            if node.name().value() == "agent"
                && let Some(name) = first_string(node)
            {
                fs::create_dir_all(self.workspace.join(name.replace('/', "-")))?;
            }
        }
        Ok(document.to_string())
    }

    /// The agent's running incarnation, waiting for it to start.
    fn incarnation(&self, agent: &str, wait: Duration) -> Result<String> {
        let started = Instant::now();
        loop {
            if let Ok(value) = self.json(None, &["agents", "show", agent])
                && let Some(incarnation) = value["value"]["incarnation_id"].as_str()
                && value["value"]["state"].as_str().is_some_and(|state| state != "stopped")
            {
                return Ok(incarnation.to_owned());
            }
            ensure!(started.elapsed() < wait, "{agent} has no running process");
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// What the agent's harness is doing, as a driver reports it: `idle` or `working`. The
    /// driver is named `demo`, so st reads the agent's conversation from its timeline.
    fn harness(&self, agent: &str, state: &str) -> Result<()> {
        let incarnation = self.incarnation(agent, Duration::from_secs(5))?;
        let state = format!("state={state}");
        let incarnation = format!("incarnation_id={}", json!(incarnation));
        let at = format!("observed_at_ms={}", now_ms());
        self.st(
            Some(agent),
            &[
                "claim", agent, "harness.observed", "--actor", agent, "--field", &state, "--field",
                "driver=demo", "--field", &incarnation, "--field", &at,
            ],
        )
        .map(drop)
    }

    /// The step's subject in the run's current generation.
    fn step_subject(&self, run: &str, step: &str) -> Result<String> {
        let started = Instant::now();
        loop {
            let value = self.json(None, &["missions", "show", &format!("mission-run/{run}")])?;
            if let Some(found) = value["steps"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|candidate| candidate["step"] == step)
                .and_then(|candidate| candidate["subject"].as_str())
            {
                return Ok(found.to_owned());
            }
            ensure!(
                started.elapsed() < Duration::from_secs(10),
                "mission-run/{run} has no step {step}"
            );
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn next(&self) -> u64 {
        self.counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// One harness timeline entry of `agent`: what its conversation pane shows.
    #[allow(clippy::too_many_arguments)]
    fn timeline(
        &self,
        agent: &str,
        incarnation: &str,
        entry: &str,
        revision: u64,
        role: &str,
        entry_type: &str,
        last: bool,
        body: Value,
        at_ms: u64,
    ) -> Result<()> {
        let operation = match (revision, last) {
            (1, _) => "append",
            (_, true) => "finalize",
            _ => "replace",
        };
        // Each entry has the next place in its agent's conversation, as a driver assigns it, and
        // keeps it through its revisions.
        let sequence = {
            let mut sequences = self
                .sequences
                .lock()
                .map_err(|_| anyhow::anyhow!("the timeline sequences are poisoned"))?;
            let (entries, next) = &mut *sequences;
            *entries.entry(entry.to_owned()).or_insert_with(|| {
                let place = next.entry(agent.to_owned()).or_insert(0);
                *place += 1;
                *place
            })
        };
        let fields = [
            format!("operation={operation}"),
            format!("entry_id={entry}"),
            format!("sequence={sequence}"),
            format!("revision={revision}"),
            format!("role={role}"),
            format!("entry_type={entry_type}"),
            format!("final={last}"),
            format!("body={body}"),
            "driver=claude".to_owned(),
            format!("incarnation_id={}", json!(incarnation)),
            format!("observed_at_unix_ms={at_ms}"),
        ];
        let key = format!("demo:{entry}:{revision}");
        let mut args = vec!["claim", agent, "harness.timeline", "--actor", agent];
        for field in &fields {
            args.push("--field");
            args.push(field);
        }
        args.extend(["--idempotency-key", &key]);
        self.st(Some(agent), &args).map(drop)
    }

    /// Do one action. `player` is there during the beats, for pacing; the seed has none.
    fn act(&self, action: &Action, player: Option<&Pacing>) -> Result<()> {
        match action {
            Action::Open {
                subject,
                place,
                focus,
                by,
            } => {
                // A document opens as the version the demo stored.
                let subject = match self.documents.lock() {
                    Ok(documents) if subject.starts_with("doc/") && !subject.contains('@') => {
                        documents.get(subject).cloned().with_context(|| {
                            format!("{subject} is not a document this demo stored")
                        })?
                    }
                    _ => subject.clone(),
                };
                let mut args = vec!["ui", "open", subject.as_str(), "--wait", "15"];
                match place {
                    Place::Tab => {}
                    Place::Right => args.push("--split"),
                    Place::Below => args.extend(["--split", "--below"]),
                }
                if !focus {
                    args.push("--keep-focus");
                }
                self.st(by.as_deref(), &args).map(drop)
            }
            Action::Say {
                agent,
                text,
                ago,
                words_per_second,
            } => {
                let incarnation = self.incarnation(agent, Duration::from_secs(5))?;
                let at = at_ms(*ago);
                let id = format!("demo-{}-{}", std::process::id(), self.next());
                self.timeline(
                    agent,
                    &incarnation,
                    &format!("{id}:message"),
                    1,
                    "assistant",
                    "message",
                    true,
                    json!({"message_id": id}),
                    at,
                )?;
                let content = format!("{id}:content");
                let body = |text: &str| json!({"media_type": "text/plain", "text": text});
                let Some(pacing) = player.filter(|_| ago.is_none()) else {
                    return self.timeline(
                        agent, &incarnation, &content, 1, "assistant", "content", true, body(text), at,
                    );
                };
                // Streams the way a harness does: a growing entry, a few words at a time.
                let rate = words_per_second.unwrap_or(9.0);
                let words: Vec<&str> = text.split_inclusive(char::is_whitespace).collect();
                let chunk = ((rate / 4.0).ceil() as usize).max(1);
                let mut shown = String::new();
                for (revision, group) in (1..).zip(words.chunks(chunk)) {
                    shown.extend(group.iter().copied());
                    let last = revision as usize * chunk >= words.len();
                    self.timeline(
                        agent,
                        &incarnation,
                        &content,
                        revision,
                        "assistant",
                        "content",
                        last,
                        body(if last { text } else { shown.trim_end() }),
                        now_ms(),
                    )?;
                    if !last {
                        pacing.sleep(Duration::from_secs_f64(group.len() as f64 / rate))?;
                    }
                }
                Ok(())
            }
            Action::Tool {
                agent,
                name,
                input,
                output,
                failed,
                takes,
                ago,
            } => {
                let incarnation = self.incarnation(agent, Duration::from_secs(5))?;
                let id = format!("demo-{}-{}", std::process::id(), self.next());
                let at = at_ms(*ago);
                self.timeline(
                    agent,
                    &incarnation,
                    &format!("{id}:call"),
                    1,
                    "assistant",
                    "tool_call",
                    true,
                    json!({"call_id": id, "name": name, "arguments": input.as_ref().map(|input| json!({"command": input})).unwrap_or(Value::Null)}),
                    at,
                )?;
                if let Some(pacing) = player.filter(|_| ago.is_none()) {
                    pacing.sleep(*takes)?;
                }
                self.timeline(
                    agent,
                    &incarnation,
                    &format!("{id}:result"),
                    1,
                    "tool",
                    "tool_result",
                    true,
                    json!({"call_id": id, "status": if *failed {"error"} else {"success"}, "media_type": "text/plain", "content": output.clone().unwrap_or_default()}),
                    if ago.is_some() { at + takes.as_millis() as u64 } else { now_ms() },
                )
            }
            Action::Send {
                from,
                to,
                subject,
                text,
                attach,
            } => {
                let mut args = vec!["conversations", "send", to.as_str(), "--from", from.as_str(), "--body", text.as_str()];
                if let Some(subject) = subject {
                    args.extend(["--subject", subject.as_str()]);
                }
                let files = attach
                    .iter()
                    .map(|file| file.to_str().context("an attachment path is not UTF-8"))
                    .collect::<Result<Vec<_>>>()?;
                for file in &files {
                    args.extend(["--attach", *file]);
                }
                self.st(Some(from), &args).map(drop)
            }
            Action::Run { mission, id } => {
                let workspace = self.workspace.to_string_lossy().into_owned();
                self.st(
                    None,
                    &[
                        "missions", "start", mission.as_str(), "--id", id,
                        "--workspace", &workspace, "--as", &self.me,
                    ],
                )
                .map(drop)
            }
            Action::Step {
                verb,
                agent,
                run,
                step,
                summary,
            } => {
                let incarnation = self.incarnation(agent, Duration::from_secs(5))?;
                let subject = self.step_subject(run, step)?;
                let verb = match verb {
                    StepVerb::Claim => "claim",
                    StepVerb::Progress => "progress",
                    StepVerb::Complete => "complete",
                    StepVerb::Fail => "fail",
                };
                if verb == "claim" {
                    // A step whose dependency just completed becomes ready a moment later.
                    self.st(
                        Some(agent),
                        &["trace", "wait", subject.as_str(), "--for", "ready", "--timeout", "30s"],
                    )?;
                }
                let mut args = vec![
                    "work", verb, subject.as_str(), "--as", agent.as_str(), "--incarnation",
                    incarnation.as_str(),
                ];
                if let Some(summary) = summary {
                    args.extend(["--summary", summary.as_str()]);
                    if verb == "fail" {
                        args.extend(["--reason", summary.as_str()]);
                    }
                }
                if verb == "complete" {
                    args.extend(["--evidence", "demo/scripted"]);
                }
                self.st(Some(agent), &args)?;
                self.harness(agent, if matches!(verb, "claim" | "progress") { "working" } else { "idle" })
            }
            Action::Ask {
                agent,
                title,
                question,
                why,
                answers,
                step,
            } => {
                let request = json!({
                    "version": 1,
                    "type": "decision",
                    "question": question,
                    "why_person": why,
                    "answers": answers.iter().map(|answer| json!({
                        "id": answer.id,
                        "label": answer.label,
                        "outcome": answer.outcome,
                        "consequence": answer.consequence,
                    })).collect::<Vec<_>>(),
                });
                let n = self.next();
                let file = demo_dir().join(format!("ask-{n}.json"));
                fs::write(&file, serde_json::to_vec_pretty(&request)?)?;
                let file = file.to_string_lossy().into_owned();
                let key = format!("demo-ask-{}-{n}", std::process::id());
                let from = match step {
                    Some((run, step)) => vec![
                        "--step".to_owned(),
                        self.step_subject(run, step)?,
                        "--incarnation".to_owned(),
                        self.incarnation(agent, Duration::from_secs(5))?,
                    ],
                    None => vec!["--new-run".to_owned(), format!("demo-ask-{n}")],
                };
                let mut args = vec![
                    "work", "ask", "--for", self.me.as_str(), "--title", title.as_str(), "--reason",
                    why.as_str(), "--request", file.as_str(), "--idempotency-key", key.as_str(),
                    "--as", agent.as_str(),
                ];
                args.extend(from.iter().map(String::as_str));
                self.st(Some(agent), &args).map(drop)
            }
            Action::Document { name, file } => {
                let file = file.to_str().context("a document path is not UTF-8")?;
                let reference = self.st(None, &["documents", "put", file, "--as", name])?;
                if let Ok(mut documents) = self.documents.lock() {
                    documents.insert(name.clone(), reference.trim().to_owned());
                }
                Ok(())
            }
            Action::Type {
                text,
                per_second,
                send,
            } => {
                let pacing = player.context("typing happens in a beat")?;
                let mut rng = Jitter::new(self.next());
                for character in text.chars() {
                    pacing.type_text(character.to_string())?;
                    // A person types unevenly: a little faster and slower around the pace.
                    let pause = (1.0 / per_second) * (0.6 + 0.8 * rng.next());
                    pacing.sleep(Duration::from_secs_f64(pause))?;
                }
                if *send {
                    pacing.sleep(Duration::from_millis(350))?;
                    pacing.send()?;
                }
                Ok(())
            }
            Action::Pause => Ok(()),
        }
    }

    /// Play the beats in order until the last one or until the presenter quits.
    fn play(&self, beats: &[Beat], player: stui::playback::Player) -> Vec<String> {
        let pacing = Pacing { player };
        let mut failures = Vec::new();
        for (index, beat) in beats.iter().enumerate() {
            let waited = match beat.wait {
                Wait::Key => pacing.key(),
                Wait::For(delay) => pacing.sleep_or_key(delay),
            };
            if waited.is_err() {
                return failures;
            }
            self.note(&format!(
                "beat {} (line {}): {}",
                index + 1,
                beat.line,
                beat.action.describe()
            ));
            match self.act(&beat.action, Some(&pacing)) {
                Ok(()) => {}
                Err(_) if pacing.ended() => return failures,
                Err(error) => failures.push(format!(
                    "beat {} (line {}), {}: {error:#}",
                    index + 1,
                    beat.line,
                    beat.action.describe()
                )),
            }
        }
        self.note("the last beat played");
        failures
    }
}

/// The player's side of the interface: the advance key and the typing.
struct Pacing {
    player: stui::playback::Player,
}

/// The presenter quit the interface.
#[derive(Debug)]
struct Ended;

impl std::fmt::Display for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the demo ended")
    }
}

impl std::error::Error for Ended {}

impl Pacing {
    fn ended(&self) -> bool {
        self.player.ended()
    }

    fn key(&self) -> Result<()> {
        self.player.pressed.recv().map_err(|_| Ended.into())
    }

    /// Wait `delay`, or less when the presenter presses the advance key.
    fn sleep_or_key(&self, delay: Duration) -> Result<()> {
        match self.player.pressed.recv_timeout(delay) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => Ok(()),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Ended.into()),
        }
    }

    /// Wait `delay` inside a beat; a key press here belongs to the next beat.
    fn sleep(&self, delay: Duration) -> Result<()> {
        std::thread::sleep(delay);
        ensure!(!self.ended(), Ended);
        Ok(())
    }

    fn type_text(&self, text: String) -> Result<()> {
        self.player
            .typed
            .send(stui::playback::Typed::Text(text))
            .map_err(|_| Ended.into())
    }

    fn send(&self) -> Result<()> {
        self.player
            .typed
            .send(stui::playback::Typed::Send)
            .map_err(|_| Ended.into())
    }
}

/// A small fixed sequence, so a demo types the same way every time.
struct Jitter(u64);

impl Jitter {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407) | 1)
    }

    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

fn at_ms(ago: Option<Duration>) -> u64 {
    now_ms().saturating_sub(ago.map_or(0, |ago| ago.as_millis() as u64))
}

/// `st demo seat`: the process a scripted agent runs. It does nothing and ends with the player.
pub fn seat(player: u32) -> Result<()> {
    wait_for_exit(player)
}

#[cfg(target_os = "linux")]
fn wait_for_exit(pid: u32) -> Result<()> {
    // SAFETY: pidfd_open takes a pid and flags and returns a new descriptor or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) } as libc::c_int;
    ensure!(fd >= 0, "the demo player {pid} is gone");
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: one valid pollfd; a pidfd becomes readable when the process exits.
        let ready = unsafe { libc::poll(&mut poll, 1, -1) };
        if ready > 0 {
            break;
        }
        if ready < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            break;
        }
    }
    // SAFETY: fd is ours.
    unsafe { libc::close(fd) };
    Ok(())
}

#[cfg(target_os = "macos")]
fn wait_for_exit(pid: u32) -> Result<()> {
    // SAFETY: kqueue returns a new descriptor or -1.
    let queue = unsafe { libc::kqueue() };
    ensure!(queue >= 0, "cannot watch the demo player");
    let change = libc::kevent {
        ident: pid as usize,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let mut event = change;
    // SAFETY: one change and room for one event; blocks until the process exits, and returns at
    // once with an error when it is already gone.
    unsafe { libc::kevent(queue, &change, 1, &mut event, 1, std::ptr::null()) };
    // SAFETY: queue is ours.
    unsafe { libc::close(queue) };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: &str = r#"
version 2
demo "small" {
  title "A small demo"
  me "robin"
  machine "studio"
}
seed {
  agent "harbor/builder" { name "Builder"; }
  agent "harbor/reviewer" { name "Reviewer"; }
  mission "harbor/release" state="ready" {
    goal "Ship it."
    step "build" { assigned-to "agent/harbor/builder"; goal "Build it."; }
  }
  run "harbor/release" id="harbor/release/2-3"
  say "agent/harbor/builder" ago="20m" "Morning."
  send from="agent/harbor/builder" to="agent/harbor/reviewer" "Ready when you are."
}
beats {
  open "agent/harbor/builder"
  say "agent/harbor/builder" after="1.5s" """
    Two lines,
      one indented.
    """
  claim "agent/harbor/builder" after="key" run="harbor/release/2-3" step="build"
  type "ship it" send=#true speed="20/s"
  ask "agent/harbor/reviewer" title="Release?" "Ship 2.3 today?" {
    answer "yes" "Ship it"
    answer "no" "Wait" outcome="decline"
  }
  pause after="2s"
}
"#;

    fn test_runner(workspace: &Path) -> Runner {
        Runner {
            exe: PathBuf::from("/opt/st/st3"),
            me: "person/robin".into(),
            log: std::sync::Mutex::new(tempfile::tempfile().unwrap()),
            workspace: workspace.to_path_buf(),
            counter: Default::default(),
            documents: Default::default(),
            sequences: Default::default(),
        }
    }

    #[test]
    fn a_demo_file_reads_as_its_seed_and_beats() {
        let demo = parse(SMALL, Path::new("/demos")).unwrap();
        assert_eq!(demo.name, "small");
        assert_eq!(demo.me, "person/robin");
        assert_eq!(demo.machine, "studio");
        assert_eq!(demo.advance, "f8");
        assert_eq!(demo.agents, ["agent/harbor/builder", "agent/harbor/reviewer"]);
        assert!(demo.graph.starts_with("version 2\n"));
        assert!(demo.graph.contains("mission \"harbor/release\""));
        assert!(!demo.graph.contains("say"), "{}", demo.graph);
        assert_eq!(demo.seed.len(), 3);
        assert_eq!(
            demo.seed[1],
            Action::Say {
                agent: "agent/harbor/builder".into(),
                text: "Morning.".into(),
                ago: Some(Duration::from_secs(1200)),
                words_per_second: None,
            }
        );
        assert_eq!(demo.beats.len(), 6);
        assert_eq!(demo.beats[0].wait, Wait::Key);
        assert_eq!(demo.beats[1].wait, Wait::For(Duration::from_millis(1500)));
        assert_eq!(
            demo.beats[1].action,
            Action::Say {
                agent: "agent/harbor/builder".into(),
                text: "Two lines,\n  one indented.".into(),
                ago: None,
                words_per_second: None,
            }
        );
        assert_eq!(demo.beats[2].wait, Wait::Key);
        assert!(matches!(
            &demo.beats[3].action,
            Action::Type { send: true, per_second, .. } if *per_second == 20.0
        ));
        let Action::Ask { answers, .. } = &demo.beats[4].action else {
            panic!("{:?}", demo.beats[4]);
        };
        assert_eq!(answers[0].outcome, "accept");
        assert_eq!(answers[1].outcome, "decline");
        // What st apply reads is the publication with each agent's stand-in.
        let root = tempfile::tempdir().unwrap();
        crate::graph::parse_intent(&test_runner(root.path()).with_stand_ins(&demo).unwrap(), "studio")
            .unwrap();
    }

    #[test]
    fn a_demo_names_only_agents_it_declares() {
        let wrong = SMALL.replace(
            "open \"agent/harbor/builder\"",
            "say \"agent/harbor/ghost\" \"Boo\"",
        );
        let error = format!("{:#}", parse(&wrong, Path::new(".")).unwrap_err());
        assert!(error.contains("agent/harbor/ghost is not an agent"), "{error}");
    }

    #[test]
    fn mistakes_are_named_with_their_line() {
        for (change, expected) in [
            (("pause after=\"2s\"", "pause after=\"soon\""), "is not a time"),
            (("pause after=\"2s\"", "pause after=\"2\""), "needs a unit"),
            (("pause after=\"2s\"", "dance"), "not a demo action"),
            (("pause after=\"2s\"", "open \"attention/x\""), "open shows agent/"),
            (("pause after=\"2s\"", "say \"agent/harbor/builder\" when=\"now\" \"x\""), "takes after, ago, speed"),
            (("me \"robin\"", "me \"robin\"\n  advance \"space\""), "advance key"),
            (("  run \"harbor/release\"", "  open \"agent/harbor/builder\"\n  run \"harbor/release\""), "is a beat"),
        ] {
            let source = SMALL.replacen(change.0, change.1, 1);
            let error = format!("{:#}", parse(&source, Path::new(".")).unwrap_err());
            assert!(error.contains(expected), "{change:?}: {error}");
        }
        let error = format!(
            "{:#}",
            parse(&SMALL.replace("pause after=\"2s\"", "dance"), Path::new(".")).unwrap_err()
        );
        assert!(error.contains("line 31"), "{error}");
    }

    #[test]
    fn the_seats_stand_ins_wait_for_the_player_in_the_demos_workspace() {
        let demo = parse(SMALL, Path::new(".")).unwrap();
        let root = tempfile::tempdir().unwrap();
        let graph = test_runner(root.path()).with_stand_ins(&demo).unwrap();
        let document: KdlDocument = graph.parse().unwrap();
        let builder = document
            .nodes()
            .iter()
            .find(|node| node.name().value() == "agent")
            .unwrap();
        let argv = children(builder)
            .iter()
            .find(|child| child.name().value() == "argv")
            .unwrap()
            .entries()
            .iter()
            .map(|entry| entry.value().as_string().unwrap().to_owned())
            .collect::<Vec<_>>();
        let player = std::process::id().to_string();
        assert_eq!(argv, ["/opt/st/st3", "demo", "seat", "--player", player.as_str()]);
        let workspace = children(builder)
            .iter()
            .filter(|child| child.name().value() == "workspace")
            .collect::<Vec<_>>();
        assert_eq!(workspace.len(), 1, "{graph}");
        assert!(root.path().join("harbor-builder").is_dir());
        crate::graph::parse_intent(&graph, "studio").unwrap();
    }

    #[test]
    fn times_and_rates_read_as_people_write_them() {
        assert_eq!(duration("1.5s").unwrap(), Duration::from_millis(1500));
        assert_eq!(duration("400ms").unwrap(), Duration::from_millis(400));
        assert_eq!(duration("2m").unwrap(), Duration::from_secs(120));
        assert!(duration("2").is_err());
        assert_eq!(rate("12/s", "words").unwrap(), 12.0);
        assert!(rate("fast", "words").is_err());
    }

    #[test]
    fn a_seat_stand_in_ends_when_the_player_does() {
        let mut child = Command::new("sleep").arg("0.2").spawn().unwrap();
        let pid = child.id();
        let waiter = std::thread::spawn(move || seat(pid));
        std::thread::sleep(Duration::from_millis(50));
        child.wait().unwrap();
        waiter.join().unwrap().unwrap();
    }

    #[test]
    fn the_example_demos_are_playable_files() {
        let dir = Path::new(test_env!("CARGO_MANIFEST_DIR")).join("../../examples/demos");
        let mut found = 0;
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|extension| extension == "kdl") {
                let demo = load(&path).unwrap();
                let root = tempfile::tempdir().unwrap();
                let graph = test_runner(root.path()).with_stand_ins(&demo).unwrap();
                crate::graph::parse_intent(&graph, &demo.machine)
                    .unwrap_or_else(|error| panic!("{}: {error:?}", path.display()));
                for action in demo.seed.iter().chain(demo.beats.iter().map(|beat| &beat.action)) {
                    match action {
                        Action::Send { attach, .. } => {
                            for file in attach {
                                assert!(file.is_file(), "{}", file.display());
                            }
                        }
                        Action::Document { file, .. } => assert!(file.is_file(), "{}", file.display()),
                        _ => {}
                    }
                }
                found += 1;
            }
        }
        assert!(found >= 2, "two example demos in {}", dir.display());
    }
}
