# Demos: a scripted st in the terminal interface

`st demo play FILE` plays a demo of st in the terminal interface. A demo is one file: what the
fleet holds when it starts (agents, missions and runs, conversations, messages, documents) and a
list of beats. Each beat does one visible thing, such as an agent's message arriving, a pane
opening, a mission step moving or an alert appearing. It fires after a delay or when you press
the advance key, so you set the pace while you record.

The agents are scripted, not harnesses. A demo uses no model tokens and plays the same way
every time.

```sh
st --instance demo demo play examples/demos/release-day.kdl
```

A demo always plays in an [instance](instances.md), so your own fleet is never touched. Each
play erases that instance's graph, starts its daemon, seeds the demo and opens the interface. To
start over, quit with **Ctrl+Q** and run the same command again. The instance needs no setup
first: the play writes its config (the demo's person and machine name) and leaves its services
alone. A play runs its own daemon; if the instance's service is running, it says so and changes
nothing.

A demo erases only an instance a demo made. The first play marks the instance with a
`.st-demo` file in `~/.st-instance/NAME`. If the instance already holds a graph, an interface
cache or a config without that mark, the play lists them and stops without touching anything.
Play in a new instance instead, or pass `--reset` to erase them and make the instance the demo's:

```sh
st --instance demo demo play --reset examples/demos/release-day.kdl
```

Two examples are in [`examples/demos`](../../examples/demos):

- `release-day.kdl`: two agents talk while a release mission moves from build to review, and the
  reviewer opens the release notes.
- `an-alert-needs-an-answer.kdl`: an agent raises an alert that only the person can answer, and
  the person types a reply.

`st demo check FILE` reads a demo without playing it and lists its beats with their lines.

Run it from your own terminal. Inside an agent seat, st binds every command to that seat, and
the demo cannot act as its own agents and person. The demo's agents start through your login
shell, like any seat, so that shell must find `pty` on its `PATH`; `st --instance demo doctor`
names anything missing.

## While it plays

- **F8** (or the file's `advance` key) fires the next beat that waits for a key. During a timed
  wait it fires the next beat at once.
- Everything else in the interface works as usual: you can click an alert to answer it, open
  panes and scroll. The advance key never reaches the interface itself.
- **Ctrl+Q** ends the demo. The demo's daemon and its agents stop with the interface.
- When a beat fails, the demo keeps going. After you quit, st lists the failed beats, and
  `~/.st-instance/NAME/state/st3-demo/play.log` records every command the beats ran.

## The file

```kdl
version 2

demo "release-day" {
  title "Two agents talk while a release mission moves"
  me "robin"          // the person whose screen this is: person/robin
  machine "studio"    // this machine's name in the demo (default studio)
  advance "f8"        // f1…f12, pagedown, or ctrl- and a letter (default f8)
}

seed {
  agent "copperwren/builder" { name "Builder"; }
  mission "copperwren/release" state="ready" {
    goal "Release Copperwren 2.3."
    step "build" { assigned-to "agent/copperwren/builder"; goal "Build it."; }
  }
  say "agent/copperwren/builder" ago="20m" "Morning. I'll start when the release run is up."
  run "copperwren/release" id="copperwren/release/2-3"
}

beats {
  open "agent/copperwren/builder"
  claim "agent/copperwren/builder" after="1.5s" run="copperwren/release/2-3" step="build"
  say "agent/copperwren/builder" after="1s" "Picked up the build step."
}
```

The `seed` holds `agent`, `mission` and `person` declarations, written as for
[`st apply`](kdl-lifecycle.md), plus actions that run before the interface opens. An agent's
name has a slash (`copperwren/builder` is `agent/copperwren/builder`). st gives each agent a
process that does nothing, in place of a harness, and a folder in the instance's work root.

A declaration holds only names and the shape of a mission, and nothing that runs:

| Declaration | Properties | Children |
| --- | --- | --- |
| `agent` | none | `name`, `description`, `role`, `type` |
| `person` | none | `name` |
| `mission` | `state` | `goal`, `constraint`, `assigned-to`, `available-to`, `step` |
| `step` | `timeout` | `title`, `goal`, `constraint`, `assigned-to`, `available-to`, `depends-on` |

Anything else, such as a harness, `command`, `env` or `gate`, is refused with its line.

The `beats` run in order once the interface is open. A beat without `after` waits for the
advance key. `after="1.5s"` fires that long after the previous beat (`ms`, `s`, `m` and `h`
work), and `after="key"` says outright that the beat waits for the key.

Text can span lines in a KDL multi-line string (`"""`); its common indentation is removed.
Agents' text is Markdown, as in a real conversation.

## Actions

Each action is the `st` command a real seat would run, run as the agent it names.

| Action | What it shows | Written as |
| --- | --- | --- |
| `open` | An agent, mission, mission run, machine or document in a tab, or a split with `split="right"` or `"below"`. This is [`st ui open`](onboarding-guide.md); `by="agent/…"` names the agent shown as opening it. A document opens as `doc/NAME`, the version the demo stored. | `open "agent/NAME" split="right" by="agent/OTHER"` |
| `focus` | The same as `open`. For a pane that is already open, it moves the focus there. | `focus "agent/NAME"` |
| `say` | The agent's message in its conversation. A beat streams it in at `speed` words a second (default `9/s`). In the seed, `ago="20m"` dates it in the past. | `say "agent/NAME" "text"` |
| `tool` | A tool call in the agent's conversation that finishes after `takes` (default 1.5s), with `input`, `output` and `failed=#true`. | `tool "agent/NAME" "Bash" input="make test" output="ok"` |
| `send` | An st message from an agent, or from the person, to an agent. It appears as a message card in the conversations of both agents. `subject` and `attach="image.png"` are optional. | `send from="agent/A" to="agent/B" "text"` |
| `run` | Starts a run of a seeded mission, as the person. | `run "MISSION" id="MISSION/RUN"` |
| `claim`, `progress`, `complete`, `fail` | A mission step moves, done by the agent it is assigned to. The optional text is the summary. | `progress "agent/NAME" run="MISSION/RUN" step="build" "text"` |
| `ask` | An alert above the person's message input, with two to four answers. With `run` and `step`, the agent asks from the step it has claimed. | see below |
| `document` | Stores a file as the st document `doc/NAME`, which an `open` beat then shows as Markdown. | `document "release-notes" file="notes.md"` |
| `type` | Types into the focused agent's message input at `speed` characters a second (default `14/s`). `send=#true` then sends it to the agent as the person. Beats only. | `type "Ship it." send=#true` |
| `pause` | Nothing; it only waits. Beats only. | `pause after="3s"` |

An alert:

```kdl
ask "agent/marrowpine/migrator" title="Run the invoice migration now?" "Run it now, or tonight at 02:00?" {
  answer "tonight" "Tonight at 02:00" then="The migrator schedules it for 02:00."
  answer "now" "Run it now" outcome="decline"
}
```

`why="…"` says why only the person can decide. Answers default to `accept` for the first and
`decline` for the rest (`outcome` is `accept`, `decline` or `defer`). `then` is the consequence
shown beside the answer. Answering the alert in the interface records the answer; nothing
follows from it unless a later beat shows it.

Paths in `attach` and `file` are relative to the demo file, and stay in its folder or below it.
A path that starts with `/`, goes up with `..`, or is a link to a file elsewhere is refused, so a
demo cannot show a file from elsewhere on the machine.

## Public screencasts

Demos are recorded and shown in public. Invent every name in them: the person, the machine,
the agents, the repositories and what they say. The example demos use invented products
(Copperwren, Marrowpine) and an invented person (Robin).
