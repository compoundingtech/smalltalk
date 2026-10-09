# The Smalltalk guide

Smalltalk runs agents as durable seats and records missions, messages and decisions
in a graph. This guide is bundled into the executable so the Assistant can help on a
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
they refuse busy seats. The built-in Smalltalk Assistant stays available after onboarding, and
setup does not resurrect it after the person stops it. `st setup --onboarding`
explicitly starts another onboarding run.

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

Message agent/st/assistant for later questions, and use `st --help` and nested command
help for the installed build's current syntax. This curated guide draws on the
repository's getting-started, talking-to-agents, seat-lifecycle, mission graph runtime
and troubleshooting guides; it does not require a checkout to read them.
