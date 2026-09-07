# Child process output capture is bounded and tail-preserving

Status: accepted

Decision made by Johannes on 2026-08-25 (issue #339, independently reproduced
measurements in the session record; option A of three, cap size 256 KiB).

## Context

The supervisor shell-out helpers (`src/run.rs`
`output_with_input_timeout_observed`, `src/ding/mod.rs` `output_with_timeout`)
redirect child stdout/stderr to tempfiles — sound, because files keep an
escaped descendant from blocking cleanup and make bounded read-back
deadlock-free — then unconditionally rewind both streams and `read_to_end`
them into fresh heap buffers. Measured peak RSS scales 1:1 with child output
(16 MiB child → +16.2 MiB RSS; 8 concurrent × 16 MiB → +103 MiB) despite doc
comments claiming "bounded output capture". No spec or invariant defined a
bound; the comment described aspiration, not behavior.

Consumer inventory: only `pty list --json` (parsed JSON, naturally bounded by
catalog size) and `pty peek` (terminal screen text, consumed for composer
matching where the tail is what matters) need complete stdout. Every other
call site uses stderr only trimmed inside error strings. The same audit found
two sibling hazards: detached reaper threads accumulate without bound under
timeout storms, and `src/eval_run.rs` holds whole step output plus full
scrollback in memory before writing log files, with an undrained pipe pair in
the bash judge that can deadlock.

## Decision

1. **Diagnostics capture is capped at 256 KiB per stream and tail-preserving.**
   When a stream exceeds the cap, the last `CAPTURE_CAP_BYTES` bytes are kept;
   the head is dropped. Truncation emits one diagnostic line naming the
   command, stream, kept/total bytes, and cap.
2. **Payload capture stays complete and explicit.** Callers that parse
   structured data (`pty list --json`) use a distinctly named
   full-stdout variant whose doc comment states that stdout is intentionally
   uncapped and why. Bounded-tail remains the default so an uncapped read is
   always a visible, deliberate choice at the call site.
3. **One shared reaper thread** drains killed children over a channel,
   replacing per-timeout detached threads.
4. **Eval run steps stream to their log files** instead of buffering, and the
   bash judge uses null stdio (only its exit status is consumed).

## Options

| Option | Result | Reason |
| --- | --- | --- |
| Cap diagnostics; keep named payload capture complete | Selected | Bounds memory while preserving structured payload parsing. |
| Spill oversized diagnostics to disk | Rejected | Adds spill-file lifecycle without a demonstrated consumer. |
| Stream every shell-out to log files | Rejected | Changes every error path; revisit only if a consumer needs complete oversized diagnostics. |

## Evidence and Argument

The reproduced 16 MiB and eight-way concurrent probes showed peak RSS scaling
with child output. The selected cap removes that scaling while retaining the
tail that carries failure diagnostics.

## Consequences

- Peak supervisor RSS no longer scales with child output volume; worst case
  is bounded by calls × 512 KiB regardless of child behavior.
- Diagnostics for oversized children lose their head. Error messages built
  from stderr keep their tail, which is where failure text lives.
- The tempfile redirection is now load-bearing for more than cleanup: it is
  what makes the bounded read-back deadlock-free. Any future move back to
  pipes must preserve a bound on buffered bytes.
