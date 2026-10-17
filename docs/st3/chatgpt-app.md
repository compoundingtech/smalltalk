# ChatGPT app: composition and install/use

Design for [#2172](https://github.com/compoundingtech/smalltalk/issues/2172), building on the assistant
principal and grants in [#2290](https://github.com/compoundingtech/smalltalk/pull/2290) and the assistant
routes in [#2541](https://github.com/compoundingtech/smalltalk/pull/2541). This is a standalone st app.

## Composition

```text
ChatGPT web (developer-mode app)        ChatGPT desktop (local plugin)
              │                                     │
    Secure MCP Tunnel                         MCP over stdio
              │                                     │
    MCP Streamable HTTP                             │
              └───────────────┬─────────────────────┘
                       st-chatgpt adapter
                              │ typed client-v0 assistant operations
                       paired st gateway
                              │
                         st daemon
```

| Piece | Owns | Must not |
| --- | --- | --- |
| st daemon (`crates/st3`) | Principal, grants, scopes, owner approvals, fences and action receipts | Speak MCP or expose the control socket to ChatGPT |
| Typed client (`crates/st3-client`) | Client-v0 assistant operations | Widen authority |
| Adapter (`crates/st-chatgpt`) | Both MCP transports, closed tool list, annotations and pluggable authentication | Run arbitrary commands, expose `st3.sock` or return credentials |
| ChatGPT app or plugin record | Connection to the adapter | Hold st credentials |
| Downstream deployment | Adapter and tunnel services for one installation | Fork the authority model or tool list |

The adapter is a separate client process, like the [iOS app](../ios-app.md). It reaches the daemon through
the paired gateway, not the privileged control socket. Each tool maps to a typed assistant operation with
an explicit scope and effect. There is no command runner or generic passthrough.

## Authority and content

- **Separate principal.** ChatGPT acts as `assistant/chatgpt`, never as the owning person. Only the owner
  can issue, renew or revoke its grants. Grants expire after at most 30 days.
- **Reads: consent at grant.** The owner selects scopes and consents to disclosure to OpenAI when issuing
  the grant. The read policy is `caller-bound-allow`; there is no per-read approval or external attestor.
  Consent covers all content within the selected scope, including conversation transcripts, question
  text and answer options. Reads remain bounded and return snapshot and freshness information.
- **Low-risk writes.** Sending a message to an owner-allowlisted agent requires the grant's write scope,
  recipient allowlist and ChatGPT confirmation card. It does not require a separate st owner approval.
- **Consequential writes.** Alert answers, reviews and mission starts require a per-action owner approval
  in st. For a person-ask answer, the approval binds the exact alert, episode, selected option and fence.
  Execution checks those bindings and records a receipt: retries return the result without executing
  again. ChatGPT cannot approve its own request. The confirmation card does not replace this approval.

Read tools are annotated read-only; write tools are annotated as writes. Scope checks, expiry, revocation,
budgets and stale fences are enforced by st, not by ChatGPT's tool annotations.

## Transports and authentication

One adapter serves both surfaces:

- **HTTP:** ChatGPT web reaches MCP Streamable HTTP through OpenAI Secure MCP Tunnel. The tunnel is
  outbound-only and needs no public MCP URL.
- **Stdio:** ChatGPT desktop launches the same adapter as a local plugin. This surface is desktop-only
  and available in Work/Codex threads, not general ChatGPT conversations or mobile/voice.

Authentication is pluggable at the adapter: Secure MCP Tunnel is the dogfood transport; OAuth is the
next authentication path. The No-auth tunnel has a known caller-binding gap: other keys in the same
OpenAI Platform organization can call the adapter by `tunnel_id`. A tunnel ID is not authenticated proof
of a particular ChatGPT caller. This is an accepted, recorded dogfood exception, not a GA security claim.
OAuth must bind the authenticated caller to the st grant and remove this gap.

## Install and use

For an example owner, `person/ada`:

1. Run st and the adapter on the owner's host. Issue an `assistant/chatgpt` grant through st's trusted-local
   owner interface, selecting content scopes, recipient allowlist, disclosure consent and a lifetime of
   at most 30 days. Complete pairing through the paired gateway; keep the credential on the adapter host.
2. For web, create a Secure MCP Tunnel in the owner's OpenAI Platform organization, associate it with the
   ChatGPT workspace, and run its tunnel client against the adapter's HTTP endpoint. The owner creates
   the developer-mode ChatGPT app record and selects the tunnel.
3. For desktop, configure the local plugin to launch `st-chatgpt` with the stdio transport in a Work/Codex
   thread. It uses the same authority model and tool list as HTTP.
4. Ask what needs attention, read the bounded alert or transcript, or send a message to an allowlisted
   agent. For a person-ask answer, request the exact option, approve the action in st, then execute it once.

Dogfood uses a real-data owner deployment; scratch scenarios are separate evidence. Neither daemon
startup nor an app record grants authority. Revocation or expiry denies subsequent requests. An offline
daemon returns unavailable; a stale fence or exceeded budget returns an explicit error, not a silent retry.

Public directory publication is separate from the tunnel dogfood path. It requires a supported hosted
HTTPS endpoint, OAuth and the applicable OpenAI review. Mobile and voice are outside these surfaces.

## Prototype evidence

Prototype commit `777eba9e` passed 24/24 scratch scenarios over HTTP and stdio: MCP initialization, tool
listing and annotations, tunnel HTTP routing, alert/question/options reads, mission and agent reads,
bounded transcript content, native owner approval and durable answers, idempotent answers and messages,
recipient and scope denial, budget exhaustion, stale episode/fence rejection, grant expiry and revocation,
and daemon-unavailable reporting. This proves the scratch adapter/daemon path, not ChatGPT UI behavior
or authenticated tunnel caller binding.

## Open questions

1. Where does the OAuth authorization server live, and how does it map authenticated callers to grants?
2. Does GA use a hosted relay or a self-hosted endpoint?
3. Which transcript redaction policy applies before scoped content is disclosed?
4. Which consequential actions receive tools beyond the person-ask answer, including reviews and mission starts?
