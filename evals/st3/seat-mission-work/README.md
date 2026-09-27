# Seat mission work

This paid black-box eval declares a slash-qualified agent at the publication root, outside the
mission. The seat keeps the exact `agent/eval/seat-mission-work/worker` identity and uses a real
Codex TUI.

The finite mission assigns one work item to that existing seat. The eval passes only after the
seat claims the work with its live incarnation, writes the requested artifact, submits completion,
and the held-out mechanical gate accepts the artifact. Eval isolation temporarily owns the seat so
normal terminal cleanup cannot leak it into the daemon after the run.
