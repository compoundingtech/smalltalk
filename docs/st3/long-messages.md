# Long messages

A message can carry up to 256 KiB of UTF-8 text. Up to 4 KiB the text is in the `message.sent`
claim, as it always was. Past that the claim holds a preview, and the whole text is a file on the
machine that took the message.

## What is stored, and where

| Part | Where |
|---|---|
| the first 1 KiB, cut at a character, then `…` | `content` of the `message.sent` claim |
| a reference: hash, size, and the member that has the file | one entry in the claim's `attachments`, with the media type `text/plain` (no image has it) |
| the whole text | `<state_dir>/message-bodies/<sha256>` on that member, its **owner** |

Nothing of the text but the preview enters the database or replication, and the claim has no new
field. Conversation content stays with its owner and is read from there per request, the way an
image attachment is: no other member keeps a copy, in a table or a file. The claim row stays about
1.5 KiB however long the message is, and no `blobs` row, document or `doc.bound` claim is written.

A build that predates this keeps only the `attachments` entries that are images, so it reads the
message as its preview and accepts the claim. A message without a `text/plain` entry, which is
every message written before this, reads exactly as it did.

## Reading

A member that reads or delivers the message asks the owner for the text each time, over the signed
peer route a relayed client read takes (`ClientReadOperation::Blob`, in 512 KiB chunks), and checks
the hash. The ask is authorized as an attachment's is: a person reads it in a conversation they are
in, and an agent seat reads what it can name.

- `GET /v1/client/message-bodies/{id}` (scope `read.projections`) answers
  `{message, bytes, text, complete}`. If the owner cannot be reached it answers
  `remote-unavailable` naming the host; if the file is gone, `blob-not-found`. A message with no
  body answers its content, so any message may be asked.
- Message resources and timeline message bodies carry optional `body_ref` (`blob/<sha256>`) and
  `body_bytes` when `content` is a preview.
- `st conversations read` prints the whole text, and `--json` adds it as `text`. A seat's
  delivery resolves the body before it is shown; a prompt shows the first 2,048 characters with
  the instruction to read the rest. When the owner cannot give the text, the seat is handed the
  preview and a note saying which machine has the rest (`complete` is false), instead of waiting.
- Search indexes the preview.

## Limits and refusals

`message.send` and `POST /v1/messages` take the whole text in `content`. Over 256 KiB the daemon
refuses with `message-too-large`, saying the limit and the size sent, and writes nothing, so a
composer keeps the person's text. `GET /v1/client/capabilities` states `limits.max_message_bytes`
(262144); a daemon that predates this omits it and takes 4096.

A device signs the whole text it sends, which no other member holds. The member that accepts the
message checks that signature against the whole text, then writes the claim without it, as it
writes a local send: a signature over text the preview does not contain would read as invalid on
every other member.

## What it costs

Measured by `what_a_message_costs_per_size` and the peer test
`a_long_body_is_read_from_its_owner_per_request_and_kept_nowhere_else` (debug build, loopback):

| Body | Claim row (every member) | `blobs` rows | Owner file | Replication exchange | Read on the owner | Read from another member |
|---:|---:|---:|---:|---:|---:|---:|
| 4 KiB | 4,556 B (inline, as before) | 0 | none | 5,089 B | not needed | not needed |
| 64 KiB | 1,534 B | 0 | 65,536 B | 2,223 B | 2.4 ms | 21 ms |
| 256 KiB | 1,535 B | 0 | 262,144 B | 2,223 B | 6.5 ms | 85 ms |

## Retention

The owner keeps the file as long as it keeps its state directory. Nothing sweeps
`message-bodies`, unlike an attachment's seven days, and there is no per-message delete yet. If the
owner is gone, offline or has lost its state, the other members show the preview and name that
machine. The files are not in the graph, so a [backup](backups.md) of the graph does not hold
them.

## Not covered

- A mission file's `message` node keeps its own 4 KiB limit.
- `st conversations export` writes the preview.
