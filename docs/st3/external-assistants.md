# External assistants

Status: reviewed requirements for a staged integration, not authorization to issue a live grant or deploy an adapter. Slice A adds assistant pairing and grant management. Slice B adds proof-gated read projections. Slices C and D below are future requirements and remain dormant: no assistant message capability, recipient-policy mutation, kill/enable API, or downstream mutation authority is implied by A/B.

The normative wire contract is [`client-v0.schema.json`](client-v0/schemas/client-v0.schema.json), [`operations.json`](client-v0/schemas/operations.json), and the [fixtures](client-v0/fixtures). Regenerate Rust, Swift, and TypeScript clients with `cargo run -p st3-client-codegen`; do not edit generated clients. Existing person pairing retains its wire shape and historical person claims. Assistant capability discovery must fail closed on older servers, never fall back to a person credential or broad limited person grant.

## Trust boundaries and residual risk

The intended composition is external reasoning service → MCP tunnel/shim → SCG approval/execution → st assistant grant → receiving agent. SCG verifies the caller's signing identity; st separately verifies its assistant grant and authorization proofs. A request-body actor, bare approval identifier, ordinary tunnel signature, or person-device signature is not an assistant authorization proof.

The SCG host owns the st bearer and assistant key. A separate pinned approval-attestor key authorizes read/send proofs. Neither st bearers nor person keys, attestor secrets, raw socket access, or private profiles may enter external model context, tool responses, plugin configuration, or process arguments. The shim owns only its request-signing and tunnel credentials. Pairing is an explicit operator operation, not adapter startup behavior.

Approved reads disclose data to the external model provider. A compromised executor host can steal local secrets; an approved message can influence its recipient. Revocation cannot recall bytes already disclosed, committed messages, or started effects. Transport/client/plan support is a separate feasibility gate: unsupported writes remain unsupported, without a read-hinted workaround or a pending send created through a read tool. Voice approval must use an actually supported explicit approval interaction, not inferred spoken consent.

## A. Principal, pairing, and revocable grant

### Authority and lifecycle

- The only supported principal is `assistant/chatgpt`, with trusted class `external-assistant`. It never has person authority. Its grant-bound request audit actor is `assistant/chatgpt/session/<grant-id>`; the authoritative message actor and sender are `assistant/chatgpt`, not the owning person. Message evidence retains `assistant_owner`, `assistant_session_actor`, and `assistant_grant_id` as on-behalf-of provenance, separate from person-delegation authority.
- Only the daemon's configured owning person through the trusted-local CLI over the Unix API may begin, inspect, renew, revoke, or change a grant. Missing owner configuration fails closed; remote clients, agents, assistants, and other people cannot manage grants. Deployment owner identity is external configuration, not a hardcoded public identity. Public examples and tests use the fixture owner `person/operator`.
- "Owner" authority here means same-UID trusted-local socket access: the daemon's configured owning person acts through the local Unix client, and any process already running as the daemon's UID is trusted the same way. The `/proc` ancestry check distinguishes local from remote callers; it is not a security boundary against a hostile same-UID process, and A does not claim it to be one. The host's UID isolation is the boundary.
- Administrative reads validate server-established harness and native-peer provenance, not just `x-st3-person`. Bound agents and harness peers cannot inspect grants or enumerate owner assistant devices with a forged owner header; ordinary agent projection reads remain available. Paired person devices are not trusted-local grant administrators, even over Unix.
- Begin uses the existing five-minute, single-use challenge flow. Scopes, identity, and the configured owner as issuer are sealed at begin; remote completion supplies only the challenge code and device key, never scopes, issuer, or actor selection. Assistant completion must not enroll the key in a person signing chain.
- `st devices pair --assistant` explicitly seals `minimal_read_mode: "caller-bound-allow"` with the supplied scopes, expiry, and disclosure consent in the challenge. Completion preserves that mode atomically; fresh CLI grants can read without a policy-update call. Wire callers that omit the mode retain `approval-once`. Grant management accepts either the printed `assistant-grant/<hex>` identifier or its bare hexadecimal suffix.
- `expires_in_seconds` defaults to 3600, must be positive, and cannot exceed 2592000 (30 days), the ceiling advertised by `operations.json` as `assistant_grant_max_ttl_seconds` and enforced for issuance and policy updates. Challenge expiry and grant expiry are independent. There is no automatic renewal. Policy update or renewal increments the policy revision; creation starts at revision 1.
- Policy updates check the effective expiry from the latest policy: an expired or revoked grant cannot be revived, while a grant renewed before expiry remains updatable after its original expiry.
- Each request rechecks the authoritative grant, revocation, expiry, and current policy. Revocation targets the exact grant, not a shared device actor. Restart must not revive a revoked or expired grant.
- Completion, authenticated assistant requests, renewal/policy changes, and revocation must reach the authoritative grant-issuer member. Replicated grants cannot establish current revocation/policy authority or enable cross-member proof replay.
- Versioned assistant grant/revocation claims are separate from historical person claims. Credentials are stored authoritatively as digests, never bearers. Public projections must not expose credential digests. Rebuildable projections are not sole authorization authority.
- Creation, completion, renewal, policy change, and revocation outcomes must retain issuer, actor, transport, timestamps, and claim references for audit. Logs and public CLI output must not expose bearer credentials or private keys.

### Wire shape

`POST /v1/client/pairings` accepts disjoint untagged `PersonPairingBegin` and `AssistantPairingBegin` variants. The person variant retains its existing `person_id`, optional `full_control`, and optional limited scopes. The assistant variant has `api_version`, `device_name`, `assistant_id: "assistant/chatgpt"`, explicit `scopes`, optional `expires_in_seconds`, `disclosure_consent: true`, and `consent_evidence`: a non-empty free-form string of at most 512 bytes naming the recorded owner consent (for example an `axe decision ask` reference). It has no `person_id` or `full_control`. Consent must explicitly cover disclosure of reads to OpenAI; missing or empty consent evidence denies issuance. The consent evidence is not a read approval, send approval, or permission to disclose to unrelated callers.

An assistant challenge retains `kind: "pairing-challenge"`, pairing ID, code, and challenge `expires_at`; it also returns principal, `grant_kind: "assistant"`, grant ID, grant expiry, policy revision, and `minimal_read_mode: "approval-once"`. Existing person challenges remain unchanged, apart from optional `grant_kind: "person"`.

Completion returns the normal snapshot envelope. `PairedSession` is a disjoint untagged union: existing `PersonPairedSession` retains `kind: "paired-session"`; `AssistantPairedSession` uses `kind: "assistant-paired-session"` and returns `api_version`, principal, sender class, device ID, session actor, credential, scopes, expiry, and `grant`. The credential belongs only in protected client storage. Person profiles reject assistant sessions; assistant profiles are stored separately with owner-only permissions and never install a person signing device.

Assistant clients validate that the issuer is a concrete `PersonId` returned by the selected trusted gateway completion, not that it equals a named deployment owner. They do not accept caller-selected issuers or use person grant/signing-chain proofs as assistant authority. The daemon enforces configured-owner issuance and preserves the sealed issuer.

`AssistantGrant` returns `kind: "assistant-grant"`, ID, principal, sender class, session actor, device ID, key ID, issuer as the daemon-configured owning person's concrete `PersonId`, exact scopes, RFC 3339 expiry, integer policy revision, minimal read mode, recipient allowlist, disclosure consent, consent evidence, and revoked status. The key ID is the SHA-256 hexadecimal digest of the canonical public-key string, not a credential digest. Pairing starts with an empty recipient allowlist. Read mode is either `approval-once` or explicitly sealed `caller-bound-allow`; the CLI selects the latter.

Trusted-local management routes are:

| Method | Route | Response value |
| --- | --- | --- |
| GET | `/v1/client/assistant/grants/{id}` | `AssistantGrant` |
| POST | `/v1/client/assistant/grants/{id}/revoke` | `AssistantGrant` |
| POST | `/v1/client/assistant/grants/{id}/policy` | `AssistantGrant` |

Policy requests contain `api_version`, explicit scopes, optional duration with the same default/maximum, disclosure consent, consent evidence, and `minimal_read_mode` (`approval-once` or `caller-bound-allow`). These operations use the existing envelope/error conventions and advertise `assistant.grants.v1`. Device and capabilities projections distinguish assistant metadata from a person delegator; assistant device rows must not synthesize `person_id`.

### Acceptance

Reject missing owner configuration, remote, agent, non-owner, absent-consent, unsupported-scope, mixed person-and-assistant, zero-duration, and over-maximum issuance. Remote completion cannot escalate scopes, select an issuer, or select a person actor. Use `person/operator` as the configured fixture owner and prove another concrete person is denied. Exercise expiry, exact-grant revocation, policy revision, and restart. Verify sealed issuer preservation, no person chain enrollment, and no credentials/digests in grant projections or safe CLI output. Merging A is not permission for live issuance.

## B. Explicit proof-gated reads

Use native snapshot/page envelope semantics and typed generated clients, never CLI output, KDL, arbitrary claim parsing, or prose scraping. The six scopes are closed and unrelated person scopes are not accepted:
The attention, missions, and agents pages retain the optional native `sync` notice during replication catch-up or divergence. Its closed `SyncNotice` contract is shared with ordinary pages and decoded by the generated SDKs.

| GET route under `/v1/client/assistant/` | Scope | Allowed fields, in addition to snapshot/page metadata |
| --- | --- | --- |
| `summary` | `read.assistant.summary` | Counts of the configured owner's attention, agent states, mission states; `as_of` |
| `attention` | `read.assistant.attention` | ID, attention-kind enum, person fixed to the configured owner, agent/mission references, revision/episode, waiting-since; no title, body, options |
| `missions` | `read.assistant.missions` | Mission/run/agent IDs, phase, step counts, blocked count, revision, updated-at; no authored goal or status prose |
| `agents` | `read.assistant.agents` | Agent ID, lifecycle/runtime-state enums, current mission/run IDs, updated-at; no paths, environment, conversation previews |
| `conversation-content` | `read.assistant.conversation` | Exact agent/session/reference/range, bounded text, revision, returned bytes, truncation/next-range |
| `terminal-content` | `read.assistant.terminal` | Exact agent/terminal/incarnation/screen revision, bounded text, returned bytes, truncation |

The baseline grant contains only the first four minimal scopes: `read.assistant.summary`, `read.assistant.attention`, `read.assistant.missions`, and `read.assistant.agents`. The configured owning person may explicitly add content scopes. `approval-once` grants require a proof for each read; explicitly sealed `caller-bound-allow` grants admit reads within the exact grant scopes, expiry, consent, caller binding, and response budget. The adapter's rich projections remain bounded and do not grant unrelated person authority.

`ScgReadAuthorization` must bind verified issuer/key and identity `chatgpt`, assistant principal, grant and policy revision, approval/execution IDs, exact projection/filter/cursor or content resource/revision/range, response-byte maximum, request digest, expiry, and a one-use nonce. st verifies a separately pinned attestor key and consumes the authorization atomically. A new page or range needs a new approval. Exact retries may recover the same bounded receipt inside the protected executor, but cached/status/private-result re-delivery to the external model requires fresh authorization.

Pagination binds the exact projection class as well as its snapshot and cache: metadata pages use `assistant.<route>`, while rich pages use `assistant.<route>-rich`. A rich cursor cannot continue a metadata request, including one authorized by another grant or after a read-mode change. Fresh metadata authorization never authorizes release of cached rich rows.

Content responses default to 4 KiB and have a hard maximum of 16 KiB UTF-8 including envelope overhead; configuration may lower these limits. Content reads do not stream, follow attachments, or silently fetch entire bodies. Over-budget requests fail rather than automatically paginate.

Assistant principals are denied every other route and stream, including generic messages, timelines, search, declarations, documents, history, events, and terminal WebSockets. Every mutation is denied in A/B, including `observations.report`, `prompt.respond`, and ordinary message lifecycle operations. Native/legacy alternate routes cannot bypass scope or proof enforcement.

B adds typed projection/content/proof schemas, the six operations with exact scope/budget metadata, and advertised assistant response ceilings. Acceptance must cover unrelated-route/mutation denial, field-only projection despite secrets/injected text in source bodies, absent/wrong/expired/reused authorization, stale revisions, changed ranges or grants, Unicode/envelope ceilings, and no private bytes in unapproved, denied, expired, foreign-handle, cached, or replayed status results.

### Read wire contract

Query parameters are scalar strings; duplicates, unknown names, and any `person` selector are rejected with `validation-failed`. The person is always the grant issuer, the daemon-configured owner.

| Route | Query | Response value |
| --- | --- | --- |
| `summary` | `max_response_bytes` | `AssistantSummary`: `kind`, `as_of`, `counts` only |
| `attention`, `missions`, `agents` | `cursor`, `limit`, `max_response_bytes` | `AssistantAttentionPage`, `AssistantMissionPage`, `AssistantAgentPage`: the native page shape with collection `assistant.<route>` and empty `filters` |
| `conversation-content` | `agent_id`, `session_id`, `reference`, `revision`, `start`, `end`, `max_response_bytes` | `AssistantContentPage` with `kind: "assistant-conversation-content"` |
| `terminal-content` | `agent_id`, `terminal_id`, `incarnation_id`, `screen_revision`, `max_response_bytes` | `AssistantContentPage` with `kind: "assistant-terminal-content"` |

Every read carries `x-st3-scg-read-authorization`: unpadded base64url JSON `ScgReadAuthorization { payload, signature }`. The closed payload contains `issuer`, `identity: "chatgpt"`, `assistant_id: "assistant/chatgpt"`, `grant_id`, `policy_revision`, `key_id`, `approval_id`, `execution_id`, `projection` (the route suffix), `parameters` (every decoded query parameter, cursor and range included), `max_response_bytes`, `request_digest`, `expires_at_unix_ms`, and `nonce`. `signature` is unpadded base64url Ed25519 over the UTF-8 bytes `st3.scg-read.v1\n` followed by the canonical JSON payload: recursively sorted object keys, compact, integers only. `request_digest` is lowercase hexadecimal SHA-256 of canonical `{"method":"GET","path":<exact path>,"parameters":<decoded map>}`. st consumes the nonce atomically before returning private bytes; a replayed, expired, mismatched, or attestor-unverifiable proof is denied and discloses nothing. Generated clients never sign, retry, or widen the budget: they send exactly one request and reject a proof whose projection, parameters, or budget differ from the call.

The response budget covers the entire JSON envelope. Without a query budget, the proof must name the pinned default (4096 unless lowered); an explicit `max_response_bytes` must equal the proof value. Neither may exceed the pinned maximum (16384 unless lowered). A response or native source record over budget fails with 413 `response-budget-exceeded`; non-UTF-8 content fails with 422 `content-not-text`. A forged, expired, or changed conversation reference returns 410 `conversation-content-invalidated`; a conversation revision mismatch, a session that is not the agent's current session, or a terminal incarnation/screen mismatch returns 409 `stale-fence`.

### Read trust

For `approval-once` grants, every read is denied until the configured owning person pins trust for the exact grant at its authoritative origin. Explicitly sealed `caller-bound-allow` grants instead use their consent, scopes, expiry, and caller binding without a separate read-trust or policy call:

```sh
st devices --as person/operator grant read-trust GRANT --issuer person/operator \
  --key-id <key-id> --attestor-public-key <base64url-ed25519> --shim-public-key <base64url-ed25519> \
  [--default-response-bytes N] [--max-response-bytes N]
```

This calls `POST /v1/client/assistant/grants/{id}/read-trust` with `AssistantReadTrustRequest` (`api_version`, `issuer`, `key_id`, `attestor_public_key`, `shim_public_key`, optional `default_response_bytes` ≤ 4096 and `max_response_bytes` ≤ 16384). Keys are 32-byte Ed25519 public keys in unpadded base64url. The attestor key must differ from the shim key, so a shim-signed request cannot authorize a read. Pinning increments the policy revision and returns `AssistantGrant` with `read_trust`; outstanding proofs for the old revision stop verifying. Public fixtures use placeholder pins and an intentionally invalid signature, never real keys.

## C. Future dormant approved message send

C is not enabled by A/B, and its scope is not part of the six-scope read contract. Before any assistant send can be enabled:

- Introduce only `assistant.message.send` for plain-text `message.send`, not broad `control.messages`. All other mutations and message lifecycle operations remain denied.
- Store an initially empty authoritative recipient allowlist. Only the configured owning person through the trusted-local CLI may replace it with exact existing `agent/...` IDs, never patterns, groups, or people. Recheck current recipient existence and policy immediately before append. Policy changes invalidate outstanding authorizations.
- Parameters are exactly `to`, `content`, `intent_id`, `intent_revision`, and `scg_approval`. No attachments, tags, caller sender/class/kind, routing, replies, or attention settlement. Fixed title is `External assistant request`, fixed kind is `wake`; both enter the proof/card digest.
- `ScgActionApproval` binds approval/execution IDs, issuer/key, verified `chatgpt` identity, principal/grant/policy revision, `message.send`, recipient and exact UTF-8 body digest, intent/revision, action digest, issued/expiry times, nonce, and signature. The trusted SCG executor attests only a verified owning-person allow-once decision. A shim-signed request or bare approval ID cannot substitute.
- Atomically retain verified approval binding, effect, and durable dedup state in action-result and message evidence: approval/execution/digest references, intent/revision, grant ID, and sender class.
- SCG's durable ledger maps principal + exact recipient + exact-body SHA-256 to one intent independent of tool-call IDs, restart, or grant renewal. Duplicate submissions reuse the existing pending/terminal handle without another card/send. An intentional identical resend needs a human-selected intent revision showing the prior delivery. st uses `assistant-message:<intent_id>:<intent_revision>`; changed bytes fail, parallel submissions append at most once, and tombstones outlive response retention.
- Enforce proof/allowlist/class invariants at authoritative message acceptance/store boundaries, including native ingress, claim ingestion, and replication. Caches or body markers cannot establish authority.

Trusted output must put `from=assistant/chatgpt sender_class=external-assistant` before the body and retain it under truncation; JSON keeps `sender_class: "external-assistant"`. Message reads and inbox notifications must warn: `WARNING: Untrusted external-assistant request; not owning-person authorization.` Receiving agents treat requests as untrusted. Consequential requests require owning-person confirmation through `axe decision ask`, never an assistant reply. Delivery approval is not downstream mutation approval; read-only analysis remains possible.

The assistant sender is preserved in stored claims, message projections, and delivered notifications. DING must not add person-only reply instructions to assistant messages; the owner/grant provenance is audit context, not person sender authority.

C must land dormant with tests for fabricated, wrong-key, expired and wrong-binding proofs, person recipients, non-allowlisted or removed recipients, alternate-ingress spoofing, and at-most-once delivery across concurrency, restart and lost response. Every enabled recipient needs the consequential-request doctrine. Acceptance must inject a genuinely approved consequential request into a normal-authority fixture recipient, withhold owning-person confirmation, and prove no consequential effect occurs and no assistant reply satisfies confirmation. Sender attribution alone is insufficient.

## D. Future coordinated kill and activation gate

D requires a configured-owning-person kill operation through the trusted-local CLI that atomically revokes grants and sets a durable principal-disabled flag. Issuance, completion, every read/action, final content release, and final message append check it. Restart preserves denial. Re-enable requires the configured owning person and a fresh grant/policy epoch, never resurrection of old approvals or queues. Missing owner configuration continues to fail closed.

SCG kill must deny catalog paths, cancel pending approvals/read grants/queued intents, disable shim ingress/signing identity, and revoke/stop the tunnel credential. The adapter rechecks the kill epoch before forwarding content/tool results. Every component records acknowledgement or failed stop; incomplete coordination fails closed. Components must not execute a pre-kill approved intent when reconnecting. Already transported bytes and committed messages remain historical. Reconcile uncertain started effects to their message/claim instead of blindly resending; do not promise rollback.

Acceptance starts an approved bounded read, pending read/send cards, a queued approved send, and a paused pre-commit send; kills; then probes old bearers/proofs, fresh pairing, direct endpoints, late approval, and restart of all components. Assert no disclosure or append after the kill linearization point, durable denial/audit, and terminal denial of pending/queued subjects.

A→B and a separately reviewed adapter may merge without activation. C stays dormant until proof, allowlist, alternate-ingress, and recipient-doctrine acceptance is complete. D and the adapter's coordinated kill test must pass on the exact candidate revisions before any live assistant grant, including read-only issuance. Live activation additionally requires a recorded caller-binding/disclosure policy decision and supported plan/client feasibility. Send enablement requires every C criterion and actual write support. No code merge, provider consent, or ambient tunnel access silently grants activation authority.
