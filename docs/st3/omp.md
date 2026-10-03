# Running st with omp

An omp seat is an ordinary st agent. Declare its workspace, harness, model, and effort in KDL:

```kdl
agent "worker" {
  workspace "${ST_WORKSPACE}"
  harness "omp" {
    model "openai-codex/MODEL"
    effort "medium"
  }
}
```

st validates the installed omp version before launch and supplies the channel extension, session
directory, and boot prompt. The launcher runs under `bun`, which must be available in the daemon's
login-shell environment. The seat uses the provider login of the user running the daemon. Confirm
the selected model in the session transcript; omp may resolve an unavailable model name to another
one.

The session JSONL is stored in st's driver state for the seat. The `model_change` entry and each
assistant turn identify the selected provider and model. omp's Python `eval` tool has a filtered
environment; run st commands from its shell tool so `ST_AGENT`, `ST3_BIN`, and `ST3_ENDPOINT` are
available.

Messages delivered during a running turn are held until the current tool batch returns. A tool call
that runs longer than the hold limit can still be backgrounded. Read the exact graph message with
`st conversations read` before acting on it, and archive it after the related action completes.

The channel reports idle only after `ctx.isIdle()` proves that the native turn has settled.
It keeps sampling through slow final unwind rather than abandoning the idle edge after a
timeout. New activity, session replacement, or channel replacement retires the old sampler;
an outstanding human ask or approval keeps its blocking observation, including on reconnect.

## Channel telemetry

The managed channel forwards `timeline` and `context` frames to the shared pi-family
normalizers. Response usage comes from `timeline` / `message_end`, including the provider's
token buckets, model, and reported cost; context occupancy is a separate measurement.
A `turn` frame carries a terminal result, not usage: provider errors become timeline errors.
`session` and `ready` frames report the native session ID and transcript for resume.

Unsupported frames, including `pre_compact`, produce a structured debug event with
`frame_type` and `observer_enabled`; frame payloads are not logged. When the managed observation
outbox is unavailable, unforwarded `timeline`, `context`, and `turn` frames produce the same
diagnostic. The managed route does not perform the standalone channel's pre-compaction context
recovery stub.
