# omp provider-credential rejection: which field names it

## Question

st2 classifies a rejected provider credential for OpenCode, Claude, and Codex
(#452). omp is the fleet's default managed harness and had no auth-class signal
at all: a seat whose credential was refused looked exactly like a quiet one.

Does omp expose a STRUCTURED signal that separates a refused credential from an
exhausted allowance, or only the prose the channel already forwards? The live
counter-example that motivates the question is a dev3 seat wedged with
`"reason":"403 You have run out of credits…"` and 120 harness-state transitions:
a 403 that no re-login fixes. Any classifier that keys on `401|403` — in the
status code or in the prose — reports that seat as a rejected credential.

Date: 2026-09-05. Binary under test: `readlink -f $(which omp)` →
`/nix/store/7q6b5pb5pxcjhpmbqy0lv79vb116vbhx-omp-18.1.7/bin/omp`,
`--version` prints `omp/18.1.7`. Linux x86_64.

## Method

Two parts.

**1. Source read.** The candidate binary is a Bun single-file executable that
embeds its own sources; `strings -n 6` recovers them. The relevant module is
`packages/ai/src/error/`:

- `qe` is a flag bitfield: `Class: 4096`, `AccountPolicy: 16384`,
  `ContentBlocked: 32768`, `UsageLimit: 524288`, `Transient: 131072`,
  `AuthFailed: 16777216`, `PayloadRejected: 2147483648`, and others.
- `jr(...flags)` ORs the flags AND SETS `qe.Class`, so `Class` is what
  distinguishes a CLASSIFICATION from a bare HTTP status: when no flag fires,
  the same field carries the raw status (or `0`).
- An assistant message whose turn failed carries three fields, assigned
  together at the provider-stream boundary: `i.stopReason = d.stopReason;
  i.errorStatus = d.status; i.errorId = d.id; i.errorMessage = …`. `errorId`
  is that bitfield.
- omp's OWN credential-invalidating rule, in its `agent_end` maintenance
  routing, is `Kt(ld(f), qe.AuthFailed) && !Kt(_, qe.UsageLimit) && !O` where
  `O` is a `CONCURRENT_LIMIT` message — i.e. `AuthFailed` minus capacity. It
  reaches for the credential store only under that conjunction.
- `AuthFailed` is set from the prose regex
  `/\b(?:401|403|unauthorized|forbidden|authentication|…)\b/i` as well as from
  a typed status, which is exactly why it needs the negative flags: the words
  co-occur.
- `stripInternalDetailsFields` removes only `__queueChipText`, and `agent_end`
  emits `messages: e` unsanitized, so an extension sees all three fields.

**2. Measurement.** A Bun server on `127.0.0.1` impersonating the Anthropic
Messages API, selected through `ANTHROPIC_BASE_URL`, returns one chosen status
and body per case. omp ran in print mode
(`-p --no-session --no-tools --no-lsp --no-skills --no-rules --no-extensions`)
against `anthropic/claude-sonnet-4-5` with `ANTHROPIC_API_KEY=sk-ant-lab-bogus`
and one throwaway extension that appended `role`, `stopReason`, `errorMessage`,
`errorStatus`, `errorId` and the message's own `error*` keys for every
`agent_end` message. All state (`HOME`, `OMP_PROFILE=authlab`, workspace,
output) lived under one `/tmp/st2-omp-auth-lab` root.

## Result

Every failing turn carried all three fields as own enumerable properties
(`ownKeys` reported `["errorStatus","errorId","errorMessage"]`). The successful
turn carried `errorId: 0` and no `errorStatus`/`errorMessage` at all.

| case | HTTP | `errorStatus` | `errorId` | hex | flags | `willContinue` |
| --- | --- | --- | --- | --- | --- | --- |
| `invalid x-api-key` | 401 | 401 | 16781312 | `0x1001000` | AuthFailed \| Class | — |
| `OAuth token has expired: invalid_grant` | 401 | 401 | 16781312 | `0x1001000` | AuthFailed \| Class | — |
| `API key does not have permission` | 403 | 403 | 16781312 | `0x1001000` | AuthFailed \| Class | — |
| **`You have run out of credits`** | 403 | 403 | 17305600 | `0x1081000` | AuthFailed \| **UsageLimit** \| Class | — |
| `cyber_policy: trusted access…` | 403 | 403 | 16830464 | `0x100d000` | AuthFailed \| **AccountPolicy** \| ContentBlocked \| Class | — |
| `CONCURRENT_LIMIT: too many concurrent requests` | 403 | 403 | 16912384 | `0x1021000` | AuthFailed \| **Transient** \| Class | `true` |
| `Payment required: insufficient balance` | 402 | 402 | 528384 | `0x81000` | UsageLimit \| Class | — |
| `rate limit` | 429 | 429 | 135168 | `0x21000` | Transient \| Class | `true` |
| ordinary turn (streamed 200) | 200 | *absent* | 0 | `0x0` | none — `stopReason: "stop"` | — |

Four facts fall out of that table.

1. **The status code is not the signal.** Three of the four 403s are not
   credential rejections, and the one that is shares its status with all of
   them. `401|403` — in the code or in the prose — misclassifies the exact live
   seat that motivated this work.
2. **`errorId` is the signal**, and the discriminator is `AuthFailed` minus
   `UsageLimit`, `AccountPolicy`, and `Transient`. Each negative flag is
   measured, not assumed: credits set `UsageLimit`, a policy refusal sets
   `AccountPolicy`, and `CONCURRENT_LIMIT` — the case omp's own rule spells out
   in prose — sets `Transient`.
3. **`Class` must be checked.** Without a flag, the same field carries the raw
   status, so the bit test has to know it is reading a classification.
4. **A retried error never ends a turn.** Both `Transient` cases arrived with
   `willContinue: true` and repeated; the shipped extension already returns
   early on that, so no credential edge is ever claimed mid-turn. The ordinary
   end is unambiguous in the other direction: `stopReason: "stop"` with no
   error fields is the recovery edge.

## Conclusion

The omp extension forwards omp's own words — the bounded prose plus `errorId` —
on a `type: "turn"` frame at every turn that actually ended, and st2 owns the
verdict, as it does for every other producer: `Source::TurnResult` +
`Reason::ProviderAuthRejected` under `Driver::Omp`, with reason `providerAuth`
on the observed-state record. `errorStatus` is deliberately NOT on the wire: it
classifies nothing this record needs, and it is already inside the prose omp
puts in `errorMessage` (`"403 {…}"`).

## VRS Impact

The result grounds omp provider-auth classification in the `errorId` bitfield,
including the negative capacity and policy flags, rather than HTTP status or
error prose.
