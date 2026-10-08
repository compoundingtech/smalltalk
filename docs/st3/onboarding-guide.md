# The Smalltalk guide

Smalltalk runs agents as durable seats and records missions, messages and decisions
in a graph. This guide is bundled into the executable so the expert can help on a
machine without a source checkout. Its version is pinned by the onboarding mission.

## Check the installation

Read `st --json doctor`, `st machines`, `st service status` and `st agents ls` before
describing what is running. Setup stores the person and machine names, installs st
and pty when needed, and offers a user service. Providers use their own accounts;
being installed does not prove that a provider is logged in. A login request belongs
in the person's own provider terminal. Never ask for or type a password.

When a provider is absent, install one and run `st setup` again. The supported
harness names are claude, codex, opencode, pi and omp. `st agents new --help` describes
provider and workspace options. OpenCode also needs the person's provider/model
choice. Follow the installed command's help rather than inventing a model name.

Claude seats use the user-owned st channel. Machine approval policy is optional:
without it st uses the development-channel flag and admits its startup dialog.
Provider or organization channel restrictions can still prevent delivery. MCP
initialization alone is not proof that a message reached a seat; check the read
receipt. An idle unscoped Claude session exposes no st tools or notifications.

## Seats and messages

A seat is a named agent with a workspace and harness. Work and messages survive
daemon restarts. New seats are made through `st agents new`; inspect that command's
help and let the person choose the project directory and harness. Examples use
invented names such as person/ada and agent/garden/worker.

Send a message with `st conversations send agent/garden/worker --from person/ada
--subject 'Garden note' --body 'Draft a private garden note.'`. Read its status and
reply in the same conversation. Put long text in a graph document and send its
immutable reference. A tool invocation or a running process does not prove that
the agent read the message; use the recorded receipt.

`st agents show`, `st agents queue` and `st agents workspace` answer what a seat is
doing and where it works. `st agents restart` recovers a stuck harness while keeping
its declaration. `st agents stop` leaves it stopped until explicitly started.
Suspend and resume preserve a quiet seat's native conversation on the same machine;
they refuse busy seats. The built-in expert stays available after onboarding, and
setup does not resurrect it after the person stops it. `st setup --onboarding`
explicitly starts another onboarding run.

Stopping the expert does not cancel its mission. To start a fresh lesson, cancel the
old active onboarding run through Missions first, then use `st setup --onboarding`.
Ordinary setup preserves stopped seats and all previous runs. The first plain `st`
opens the expert conversation once; subsequent navigation remains yours.

## Home and the terminal interface

Plain `st` opens the terminal interface. Home shows decisions, approvals, questions
and failures that need the person. Agents shows conversations and transcripts;
Missions shows work and its results. Ctrl+K opens the palette, Ctrl+H opens Home,
Ctrl+Q quits the interface while work keeps running. Ctrl+] attaches to a selected
seat's terminal and Ctrl+\ detaches. Raw terminal keys belong to the harness while
attached. Use a choice or decision on Home for a person-only answer; do not leave
questions solely in a harness terminal.

## Missions and evidence

A mission describes the outcome, constraints, steps and checks. Publication does
not start a run. Show a small KDL example, preview it with `st apply --dry-run`, and
ask the person for a decision before starting sample work. The sample stays private
in the onboarding scratch workspace. It should include a review the person answers
from Home. Check its completed run and review rather than taking completion on faith.

The installed st skill explains claiming steps, storing evidence, asking for human
answers and finishing work. Use it for exact command syntax. The graph is authority:
read current claims, steps, run history and immutable documents instead of guessing
or keeping a second local checklist. Optional phone, second-machine and GitHub steps
can be skipped. Do not repeat a skipped question in that onboarding run.

## Keeping work running

Check the user service with `st service status`. On Linux, lingering lets it start
before login after a reboot; when unavailable, setup prints the exact loginctl
command for the person. On macOS, inspect `st service permissions` and explain the
permissions the person must grant. A detached daemon fallback stops on reboot.
Never run sudo or try to automate a password or operating-system permission prompt.

The phone pairs through `st devices`; it reads and acts through a paired machine,
and runs no seat or replica itself. A second machine joins through a fleet invite
only when the person asks. GitHub integration is optional; absent gh or missing
authentication must not block any other onboarding work.

## When something is wrong

Use `st doctor`, `st agents show`, `st conversations status` and `st missions show`
to separate daemon availability, provider login, channel admission, message delivery
and work failure. Read the exact reason and preserve relevant evidence. A restart
is a recovery action, not proof that a problem was fixed: check the new incarnation
and its read receipt. Never assume a machine name, a published release, a paired
device, a successful installation, or a completed mission from a command attempt.

Message agent/st/expert for later questions, and use `st --help` and nested command
help for the installed build's current syntax. This curated guide draws on the
repository's getting-started, talking-to-agents, seat-lifecycle, mission graph runtime
and troubleshooting guides; it does not require a checkout to read them.

## The expert's onboarding workflow

Read the claimed step and its current `person_answers` with `st work show STEP
--json`. Use the run's actual requester, generation, attempt and workspace; never
substitute the example person/ada for a configured person. The person answers on
Home, and `work ask` yields the worker lease. Ask one clear question at a time, give
its purpose, and stop doing dependent work until that answer arrives. Do not answer
as the person, infer consent from silence, or hide a request in the harness terminal.

For choices, write a private JSON request file with this shape:

```json
{"version":1,"type":"choice","question":"Would you like to pair a phone now?","why_person":"Only you can choose whether to connect your phone.","answers":[{"id":"enable","label":"Pair now","consequence":"We will pair your phone and check its connection."},{"id":"skip","label":"Skip","consequence":"Continue without a phone; this run will not ask again."}]}
```

Then use `st work ask --for PERSON --step STEP --request FILE --as agent/st/expert
--idempotency-key KEY`. Read `work ask --help` for the installed version. Use a key
containing the current run, generation, step, attempt and question name. Reusing it
recovers the same request; use a new question key only when a help or decline answer
requires another question. After the ask, release the turn and wait for its graph
wake. On resumption, read the step before interpreting an answer.

The tour choice uses IDs `tour-seen` and `help`. The phone, second-machine and GitHub
choices each use `enable` and `skip`. A skip belongs to its own step and this run;
it does not skip another step. Even without hardware or gh, offer the skip answer
on Home instead of manufacturing it. Optional enable may be changed to skip through
a new person question if setup cannot proceed. On macOS, the permissions choice
uses `permissions-seen` and `help` after showing `st service permissions`.

For a decision, use `type: "decision"` and two answers:
`{"id":"accept","label":"Start","outcome":"accept","consequence":"Start the work shown above."}`
and `{"id":"decline","label":"Revise","outcome":"decline","consequence":"Revise the plan before anything starts."}`.
Include the proposed work in the question, and a `why_person` explaining the
decision. Only `accept` authorizes the shown sample or project seat. For the project
directory and harness, first ask a `feedback` request; show the resulting absolute
directory, harness and proposed seat name together in the subsequent decision.
The provider login and model choice belong to the person's provider account.

### A garden note with a real review

Create `garden-note` beneath this onboarding run's workspace. Write a short invented
garden note there and publish its text with `st documents put FILE --as doc/NAME`.
Substitute that returned immutable reference and the actual run requester into this
private sample KDL. Use a distinct mission name containing the current generation
so old sample runs cannot satisfy a new lesson:

```kdl
version 2
mission "st/onboarding/garden-example" state="ready" {
  goal "Review a private note about the garden."
  step "review-note" {
    agentless
    goal "Read the invented garden note and decide whether to accept it."
    gate "the person approves the note" type="human" {
      reviewer "person/ada"
      question "Is this private garden note ready?"
      review "doc/garden/note@REPLACE_WITH_RETURNED_HASH"
    }
  }
}
```

Show the draft and KDL to the person. Preview with `st apply FILE --dry-run --as
agent/st/expert --workspace SCRATCH` before asking the decision on Home. Do not use
`--check` on arbitrary commands before inspecting them. After accept, publish with
`st apply FILE --as agent/st/expert --workspace SCRATCH` and start the exact published
revision with `st missions start MISSION --revision REVISION --workspace SCRATCH
--as agent/st/expert`. Record the returned run ID. The sample needs no extra provider
seat: its one agentless step waits for the human review, which appears on Home.
Explain the mission, step, gate, evidence document and review while the person
answers. Read `st missions show RUN --json`; do not claim success until its current
run is completed. Do not revise its mission after it completes and before checking
the lesson, because the gate compares the revision that ran with the sample spec.

### Evidence checked by the bundled gates

The `first-mission`, `your-project`, macOS `keep-running`, and `wrap-up` steps put a
small JSON document at `doc/st/onboarding/RUN/STEP`. RUN is the bare full run ID,
without `mission-run/`; retain its slashes. The evidence always includes:

```json
{"version":1,"run":"st/onboarding/example","generation":"example-generation","attempt":1,"sample_run":"st/onboarding/garden-example/one"}
```

Replace every example value with the current graph value. For `first-mission`,
include `sample_run` as above. For `your-project`, replace it with `agent`, `message`
and `workspace` containing the new seat subject, its first message subject and the
approved absolute directory. After showing macOS permissions, use
`"permissions_shown":true`. For `wrap-up`, use `"summary":"..."` listing the checked
objects, how to message the expert, and any optional skips. Keep evidence under
8192 bytes. Do not put unrelated fields into this checked JSON; store longer notes
as a separate evidence document. Publish via `st documents put FILE --as doc/NAME`
and cite the returned immutable reference in `work complete`.

Each gate runs `st gate onboarding STEP --run RUN --generation GENERATION --attempt
ATTEMPT`. It exits 0 for checked facts, 1 for facts not yet present and 3 for broken
checks or malformed evidence. A document alone is not completion: the garden run
must have completed its actual person review; a project seat must have a healthy
incarnation and a recipient read claim. Stale generation or attempt evidence cannot
pass. Run a gate directly to see its diagnosis before submitting a step. A preview
with `apply --dry-run --check` uses a synthetic run and normally reports not yet;
that is valid and writes no onboarding progress.

For keeping work running, an installed active service is required. On Linux, read
`loginctl show-user --property=Linger --value`; when it is not `yes`, give the person
the exact `loginctl enable-linger USER` command for their own terminal. They may
need system authorization. The expert must neither run sudo nor ask for a password.
A normal shell terminal can help; do not assume a setup tab can preload commands.
On macOS, show the permissions guidance and wait for the person's acknowledgement;
this acknowledges the guidance, not an automatically verified OS permission grant.

The final gate checks that the eight preceding steps completed and the built-in
expert remains a persistent seat. Complete `wrap-up` normally; completion of the
run follows that step, so never wait for the run to complete before submitting it.
Keep the expert available for later questions. Do not automatically stop or suspend
it, create another onboarding run, repeat skipped questions or enable observers.
