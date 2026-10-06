# Provider adapter contract, first draft

The Discord pilot is an adapter: it imports provider messages into st and delivers
ordinary st replies back to the provider. This document separates the author,
authenticated transport actor and conversation route. It applies to Discord first;
other providers can implement the same responsibilities.

The native import and bounded delivery-wait endpoints below are implemented in this
worktree and await final verification and deployment. The graph-owned delivery queue
and receipt API remain a proposal. These endpoints are absent from the currently
installed st binary.

## Identity and route

An adapter authenticates as its own program seat, such as `agent/example/bridge`.
A Discord account is `external/discord/user/404`, never the adapter or an existing
native person. A webhook can have `external/discord/webhook/505`. Provider metadata
can distinguish bot and human accounts. Future verified person-to-account edges
associate accounts without rewriting historical senders or granting authority
because a message body claims a person's name.

An external account does not identify a destination channel. The conversation route
also records provider server/workspace, channel or DM, thread and provider message
ID. Replies preserve both native `in_reply_to` and provider reply/thread references.
The pilot retains this information in private adapter state. It does not route an
arbitrary st message solely because its recipient has an external namespace.

The first native import permission is deliberately narrow: the selected program
seat declares its enrolled external sources and target st agent:

```kdl
version 2
agent "example/bridge" {
  workspace "/srv/example/bridge"
  argv "/usr/bin/python3" "/srv/example/bridge/program.py"
  tags st3.adapter.sources="external/discord/user/404,external/discord/user/606" st3.adapter.target="agent/example/test"
}
```

The authenticated local actor must be that program seat. The import handler checks
the selected declaration rather than accepting permissions asserted in the request.
An unenrolled source, native person, target or undeclared adapter is refused. This
first permission binds up to sixteen distinct accounts to one agent. Sources are
a comma-separated list without spaces or duplicates; the singular
`st3.adapter.source` remains supported for a single account. Multi-channel routing
needs a separate explicit contract.

## Inbound import and images

The adapter uploads image bytes using `POST /v1/client/blobs` as itself. It then sends
`POST /v1/client/adapter/import`, authenticated as itself, with the usual message
fields: stable `idempotency_key`, external `from`, agent `to`, text, `in_reply_to`,
title, tags and attachment inputs. Attachment inputs are `{blob, media_type, name}`.
The upload owner remains the adapter; the recorded message sender remains external.
The import appends `adapter:agent/example/bridge` to the message tags as provenance.
Ordinary message sends retain their existing sender/upload ownership checks.

The import returns the canonical st message and is idempotent. The provider message
ID and route determine the stable import key. A retry must use the identical body,
sender and attachments. A retained old pilot import is not renamed or executed again
during migration.

Images use st's existing content-addressed file store. Bytes remain outside the
replicated claim log; messages carry hash, type, size and origin. The pilot accepts
PNG, JPEG, GIF and WebP, up to four images and 10 MiB total per provider message.
The adapter downloads only HTTPS attachments for the configured channel from the
Discord CDN, without a bot authorization header or redirects. It checks bytes/type,
size and hash before delivery. Non-image files, stickers and embedded URLs are not
downloaded. Expired attachment URLs can be refreshed by fetching the original
provider message, without changing retained attachment identity.

## Interim outbound wait contract

The live text pilot still pages the mailbox every five seconds. This update replaces
that scan with `GET /v1/client/adapter/deliveries?after=INDEX&wait_ms=MILLISECONDS`,
authenticated as the enrolled adapter. The server subscribes before reading, waits
for graph change notifications for at most ten seconds, and returns a bounded page
of at most 100 replies, a durable local frontier, host identity and `more` flag under
`st3.adapter.delivery.v0`. It reads only enrolled external accounts and their
legacy `person/discord-404` mailboxes during migration. Only replies from
the configured agent, addressed to one of those exact accounts and linked to a
retained route, are exported. Native replies are sufficient; agents need no Discord
client or token.

The adapter queues retained reply parts before advancing its cursor. Reconnect and
restart replay by native message ID; the existing outbox still proves provider
delivery. A backwards frontier or changed host requires explicit reconciliation.
The bridge runs one wait request until the next Discord intake deadline; it wakes
for replies without scanning history on a fixed timer. Discord intake remains
five-second REST polling. This interim notification source observes global graph
changes internally, but unrelated writes do not end the wait or force another HTTP
request. Selected resource notifications should
replace that source when the graph-watch engine arrives.

SQLite holds the provider/native message ID map, reply references, accepted intake
cursor, and an outbox with stable per-part delivery nonce and states `pending`,
`sending`, `uncertain`, `sent`. A confirmed receipt records the provider ID. The
credential is never stored there. The database is a delivery checkpoint, not proof
that an external account belongs to a person.

This separate SQLite database is transitional. Durable routes, account links,
provider/native correlations and delivery outcomes belong in the st resource graph,
so authorization, replication and watches see the same facts. Provider-local state
should retain only recoverable transport checkpoints and caches. Moving these facts
requires importing existing maps and receipts before retiring the pilot database.

For images, the outbox retains content hashes and attachment metadata. An outgoing
image receipt must match author, channel, source reference, nonce when required,
text, attachment names/type/size and downloaded content hashes. A lost receipt is
held and reconciled without blind reposting. Images appear only on the first part
of a split text reply. Original maps and receipts survive additive schema migration.

## Proposed outbound interface

Replace mailbox scans with a provider-neutral durable delivery queue owned by the
adapter. One resumable stream or long-poll connection can carry its enrolled routes.
Each item needs a stable delivery ID, route ID, native message ID, external recipient,
conversation/reply reference, text and attachment references. The adapter records a
provider receipt, a retryable refusal or an unknown outcome for that exact delivery.
Only proven delivery completes it; receipt submission must also be idempotent.

The server retains unacknowledged items across reconnects, daemon restarts and
offline adapters. A persisted cursor permits replay; a cursor gap requires a
bounded queue reload, not silently skipping messages. Explicit route ownership
prevents two adapters from both delivering the same route. Rate limits and provider
retry delays remain adapter responsibilities. Secrets remain in the provider process.

The installed binary has no `st watch` command. Conversation WebSocket streams and
long-poll changes reads exist, but follow an agent session/timeline rather than this
provider delivery queue. They can supply change notifications while a queue is built;
they must not be mistaken for delivery receipts. Discord intake can later use its
Gateway rather than REST polling.

The active graph-watch mission `fleet/smalltalk/graph-watch/2026-10-06` plans an
indexed resource watch engine with durable cursors, bounded replay and reactive
subscriptions. Its contract-and-engine step is still pending. Represent the delivery
queue as graph resources and reuse that engine's frontier, reconnect and resync
semantics when available; do not build a second general graph-watch engine inside
each adapter. A notification means the queue changed, not that a provider accepted
a delivery. Intake from Discord's Gateway and outbound graph notifications are
separate streams.

## Shared plugin scope

A bundled provider plugin can declare program seats, enrolled routes and supported
operations while sharing identity, authorization, blob references, durable cursors,
delivery receipts and retry rules. Discord and Slack can implement message import
and delivery. Provider credentials remain outside declarations and graph claims.

Notion can reuse this lifecycle for resource synchronization: page/file revisions
arrive as graph resources and outbound operations update provider content. It needs
revision comparisons, conflict policy, deletion handling and suppression of its own
write notifications. Page edits should not be forced into chat-message semantics.
This is a design direction, not an implemented Slack or Notion connector. Notion's
[webhooks](https://developers.notion.com/reference/webhooks) notify changes and its
[page update API](https://developers.notion.com/reference/patch-page) supplies one
outbound operation.

## Verification required before live cutover

Registering `external` changes the native schema registry and its compatibility
digest. A previous registry rejects this sender as an unknown subject family; a
single-host rollout is insufficient evidence that peers can validate and project
these messages. Coordinate the native schema release across the relevant fleet
members, or prove an explicit mixed-version compatibility path before importing
new external senders. Restarting only the Discord program cannot install this API
or update peer registries.

Native tests must prove route refusal, actor separation, image upload ownership,
import idempotency and unchanged ordinary-send protections. Adapter tests must
prove images both ways, anonymous bounded CDN reads, reply correlation, migration
of old checkpoints, restart recovery and no duplicate uncertain image send. Live
cutover needs the native import endpoint first, then the route declaration and
bridge restart, followed by an actual inbound/outbound image exchange. The tested
text bridge must remain available until that sequence is ready.

Provider references: [Discord message and attachment API](https://docs.discord.com/developers/resources/message),
including multipart file upload, reply references and nonce behavior.
