# Claude batched tool calls under an open permission prompt

## Question

Date: 2026-08-23. Binary: Claude Code 2.1.237 (Fable 5 bundle), Linux, driven under `pty`.
Purpose: take the capture DQ-H1 (and #268 §C) said was missing — a batch where one call needs
permission — and settle whether the shipped blocked-exit rule
(`src/claude_session.rs::observe_hook_event`: clear on next `PreToolUse`/`PostToolUse`/`Stop`)
false-clears while a prompt is open.

## Method

A scratch project (isolated CWD; hooks are project-scoped) with `.claude/settings.local.json`
allowlisting `Bash(echo:*)` and registering logging hooks that append every payload as one JSON
line and always exit 0, for: `PreToolUse`, `PostToolUse`, `PermissionRequest`, `Stop`,
`SubagentStop` — later runs added `PermissionDenied`, `UserPromptSubmit`, `SessionEnd`.

- Runs 1–2: `claude -p` (non-interactive), 120–180 s timeouts.
- Runs 3–4: interactive `claude --permission-mode default` in a detached ephemeral `pty` session
  (`pty run -d -e --id <id> --cwd <proj> --env LOGFILE=… -- claude --permission-mode default`),
  prompts sent with `pty send --seq … --seq key:return`, screen sampled with `pty peek --plain`,
  the log dumped **while the prompt was open** and again after answering.

Four short turns total, ≈$0.61.

## Result

**Run 2, `-p`, batch allowlisted-first** (`echo a` + `touch scratch-file.txt`):

```
PreToolUse  Bash "echo a"                 tool_use_id=toolu_014yt…
PostToolUse Bash "echo a"                 (before the second call starts)
PreToolUse  Bash "touch scratch-file.txt" tool_use_id=toolu_01RrZ…
PreToolUse  Write …                       (fallback attempt, then waits)
Stop
```

No `PermissionRequest` fires in `-p` mode; the non-interactive path blocks/denies without one.
Execution is strictly serial: the first call's `Post` precedes the second call's `Pre`.

**Run 3, interactive, batch permission-first** (`touch scratch2.txt` + allowlisted `echo b`):

```
481.34  PreToolUse        Bash "touch scratch2.txt"  tool_use_id=toolu_01HK5…  prompt_id=0ea832de…
481.67  PermissionRequest Bash "touch scratch2.txt"  NO tool_use_id            prompt_id=0ea832de…
        — prompt open 33 s; log dumped during: NO further event of any kind,
          although the TUI already rendered the batched `echo b` —
514.10  PostToolUse       Bash "touch scratch2.txt"  tool_use_id=toolu_01HK5…  (the grant)
514.30  PreToolUse        Bash "echo b"              tool_use_id=toolu_01NhR…
514.59  PostToolUse       Bash "echo b"
516.17  Stop
519.44  SubagentStop      agent_id="a5c61ec4ef268c3cc" agent_type=""           (phantom, +3.3 s)
```

**Runs 3b/4, deny path** ("3. No" on the prompt, run 4 with `PermissionDenied` + `SessionEnd`
registered): after `PreToolUse` + `PermissionRequest`, denial produced **zero further events** —
no `PostToolUse`, no `Stop`, no `PermissionDenied`. The turn ends silently.

## Conclusion

1. **Execution serializes around an open permission prompt.** In both orderings no hook event
   fires while a prompt is up — a parallel-batched allowlisted call waits out the grant. The
   false-clear #268 §C predicted (first call's `Post` clearing `blocked` during the second call's
   prompt) is unobservable in this build: the next `Pre`/`Post`/`Stop` after `PermissionRequest`
   is always the blocked call's own resolution. The shipped exit rule is correct, not merely
   conservative.
2. **`PermissionRequest` still carries no call identity.** `tool_use_id` is present on
   `PreToolUse`/`PostToolUse` but absent on `PermissionRequest`; its `prompt_id` is turn-scoped
   (identical across every event of the turn) and cannot correlate a call.
3. **Denial is eventless.** Even with `PermissionDenied` registered, "No" ends the turn with no
   event, so `blockedOn: human` stands until the next `UserPromptSubmit`/`SessionStart`. Half-true
   semantically (a person's direction is still what the session waits on), but the state axis
   reads `active` while the model is stopped. Pinned as DQ-H1's residual limit.
4. **The phantom `SubagentStop` reproduces**: 3.3 s after `Stop`, `agent_id` non-empty,
   `agent_type` the empty string, no subagent ran — the guard's emergent-property basis holds in
   this build.
5. `-p` mode never fires `PermissionRequest`; permission evidence exists only interactively.

## VRS Impact

- DQ-H1 re-resolved: the batched-capture gate is met; the exit rule stands on measured ground and
  the open question narrows to the eventless deny window.
- The measured grant sequence is replayed verbatim by
  `src/claude_session.rs::measured_batched_grant_sequence_holds_blocked_until_the_granted_calls_own_post`.
- No requirement text changes: OHS-R05's producer rule is confirmed, not amended.
