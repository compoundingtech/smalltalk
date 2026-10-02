# Attachments

A message can carry up to four images, so a screenshot pasted into stui on one machine reaches a
seat on another. This page defines what is stored, where, and how it moves.

## What travels, and what does not

An attachment's bytes are **not** claims and **not** replicated. They are files under
`<state_dir>/blobs/<sha256>` on the member that took the upload, which this page calls the origin.

The `message.sent` claim carries only a reference, in its `attachments` field:

```json
{"sha256": "…64 hex…", "media_type": "image/png", "name": "Screenshot.png", "size": 184211, "origin": "host/bluey"}
```

Nothing in the claim log or in replication names the bytes except that reference, so a pasted
image adds a few hundred bytes to sync, not megabytes. The reference field is `sha256`, never
`blob_hash`: the graph replicates and requires every blob a claim's `blob_hash`, `hash` or
`bundle_hash` field names, which is the opposite of what an attachment wants.

## Limits

| Limit | Value |
|---|---|
| Types | `image/png`, `image/jpeg`, `image/gif`, `image/webp`; the bytes must match the type named |
| Size | 10 MiB per image |
| Per message | 4 |
| Per actor | 128 MiB of uploads inside the retention window (`blob-quota-exceeded`) |
| Retention | 7 days from the last write or fetch, on every member (`ST3_BLOB_RETENTION_HOURS` changes it) |

The daemon sweeps files past retention at most every ten minutes, when it takes an upload or keeps
a fetched copy. After that a read answers `blob-expired` (HTTP 410) and the message keeps its text.

## Upload

`POST /v1/client/blobs` with the image as the raw body and its type as `Content-Type`. The session
needs the `control.messages` scope. The answer is `{blob, sha256, size, media_type}`, where `blob`
is `blob/<sha256>`. The same bytes answer the same reference. `st blobs put FILE` does it from the
CLI, and `st conversations send --attach FILE` does it as part of a send.

Errors: `blob-too-large` (413), `unsupported-media-type` (415), `blob-content-mismatch` (422),
`blob-quota-exceeded` (429).

## Send

`message.send` takes `attachments: [{blob, media_type, name?}]` in its parameters, and the
internal `POST /v1/messages` takes the same list. Each `blob` must be an upload by the sender that
this member still holds. The daemon completes each entry with `size` and `origin` before it writes
the claim. A message with an attachment may have empty text.

## Read

- `GET /v1/client/blobs/{sha256}?message=message/ID` answers the raw bytes with their type.
- `GET /v1/client/blobs/{sha256}/chunk?message=message/ID&offset=N` answers up to 512 KiB as
  base64 in JSON with the total `size`. Clients whose response size is negotiated read this way;
  `Client::blob` in Rust, TypeScript and Swift assemble the whole file.

A person's session may read an attachment it uploaded, and one carried by a message it may read:
a message it sent or received, one in a thread it took part in, or one to or from an agent, which
free mode lets any person in the fleet read. Any other read is `forbidden`. A local agent session
reads what it can name, also under free mode.

A member that does not hold the file asks the origin. The request is a client read over the same
signed peer route that relays terminal and conversation reads (`ClientReadOperation::Blob`), so it
passes through intermediate members the same way, never through replication. The file moves in
512 KiB chunks, its hash is checked, and the member keeps a copy under its own retention window.
An origin that cannot be reached answers `remote-unavailable`.

## Delivery to a seat

A driver writes each attachment under its own state directory, `attachments/<sha256>.<ext>`, and
names the path in the notification it hands the harness:

```
<smalltalk-message id="…" …>
look at this
<attachment path="/…/attachments/<sha256>.png" media_type="image/png" bytes="184211" name="Screenshot.png"/>
</smalltalk-message>
```

A message is delivered once its files are on this machine: an origin that is unreachable delays
delivery like any other transient fault. A file past its retention window is named with
`unavailable="expired"` and no path. The Claude, pi, omp, Codex and OpenCode drivers all take this
envelope, and a harness opens the file with its own file tool.

Seat copies are removed after the same seven days.
