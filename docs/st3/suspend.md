# Suspending a seat

`st agents suspend` stops a quiet seat and keeps its harness's own session. `st agents resume`
relaunches the harness on that same session and confirms it is the same one. This works on one
host for Claude, Codex, pi, omp, and OpenCode. Moving a suspended seat to another host is planned
in #901 and is not built yet.

```sh
st agents suspend agent/example/worker --as person/ada --reason "idle overnight"
st agents show agent/example/worker
st agents resume agent/example/worker --as person/ada
```

Both commands wait until the operation finishes or fails, and `--timeout` sets how long they
wait. A client uses the fenced client-v0 actions `agent.suspend` and `agent.resume`. Suspend is
fenced on the running incarnation and the selected declaration; resume is fenced on the
declaration.

## When a seat can suspend

A seat suspends only at a clean boundary. Core does not read harness state. Each driver adds
`quiescent` and `blocking` to the `harness.observed` claims it already publishes, and core reads
those two fields. Core adds its own blockers, so st refuses a suspend with a typed list of
reasons:

| reason | meaning |
| --- | --- |
| `turn-in-flight` | the harness is working |
| `pending-ask` | the harness waits on a person: a question, permission, or review |
| `unsent-input` | text sits in the harness's composer |
| `starting`, `harness-indeterminate`, `harness-unobserved` | the driver cannot yet show the harness is idle |
| `claimed-work` | the seat holds a claimed step, whose lease would lapse |
| `subagent-running` | st still records a subagent for the seat |
| `native-session-unbound` | the driver has reported no native session it can resume; Claude and pi write a transcript only with their first turn |

The owner checks the same reasons again before it stops anything. A refused suspend leaves the
seat running, and `st agents show` prints the refusal.

## What suspend and resume do

The seat's driver reports the native session its harness is running as a `harness.session-file`
claim for that incarnation, through `POST /v1/agents/native-session`, once the harness has
written what a resume reads. That session is the snapshot. Suspend records it, stops the runtime, and leaves the seat `suspended`. A suspended seat
stays declared. Its restart policy does not restart it, `st agents restart` refuses it, and mail
sent to it waits until it resumes.

Resume launches the driver with the session in `ST3_NATIVE_RESUME_SESSION`. Each driver
relaunches its harness on exactly that session:

| harness | how it resumes | what it checks first |
| --- | --- | --- |
| Claude | `claude --resume ID` | the transcript is in the workspace's Claude project directory |
| Codex | `codex --remote … resume THREAD`, then the control connection's `thread/resume` | the app-server accepts the thread |
| pi | `pi --session TRANSCRIPT` | the seat's transcript has the session's header |
| omp | `omp --resume ID` | the seat's transcript has the session's header |
| OpenCode | `opencode --session ID`, with delivery held to that session | the session is in `opencode.db`, and the server returns it |

A driver that cannot resume ends the launch before the harness picks another session. It records
a `native-resume-unavailable` diagnostic whose status is the reason: `transcript-missing`,
`authored-session-selection` (the declaration already selects a session), `harness-refused`, or
`invalid-session-id`. The resume completes only when the relaunched driver reports the same
native session. A different session (`native-session-mismatch`), no session within three minutes
(`native-session-unbound`), or an exit before binding fails the resume. After a failed resume the
seat stays suspended on the same snapshot, with the code and reason.

## Phases

The agent resource's `suspension` shows the latest operation. Each phase is a claim by the
requester, so every phase change is a change of the agent.

```text
suspend: quiescing -> snapshotting -> suspended     (failed: refused, the seat keeps running)
resume:  restoring -> verifying   -> resumed        (failed: back to suspended, with a code)
```

A suspension belongs to the launch it was taken under. Stopping the seat, or applying a
declaration that changes how it launches, ends the suspension, and the next start follows the
usual rules. A label change does not end it.

## Limits

- A seat resumes only on the host where it suspended. Moving it is planned in #901.
- Outside suspend and resume, a relaunch still starts a new native session for every harness
  except omp, which resumes on relaunch once #1048 merges.
- Mail does not wake a suspended seat. Only `st agents resume` does.
- OpenCode delivery follows the session the server reports most recently. A seat whose OpenCode
  session ran a subagent session last may record that session.

The boot canaries cover suspend and resume for every harness. See
[scripts/st3-boot-canaries](../../scripts/st3-boot-canaries/README.md).
