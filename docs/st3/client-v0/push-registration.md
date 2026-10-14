# Paired push target registration (Draft, #2783)

This first slice is gated by the daemon's `push-registration` Cargo feature. It adds no sender or APNs request. A paired phone uses its existing Bearer credential on its pairing issuer:

| Command | Body | Result |
| --- | --- | --- |
| `PUT /v1/client/push-registration` | `token`, `environment` (`development` or `production`), `bundle_id`, `app_version` | `{ "registered": true }` |
| `DELETE /v1/client/push-registration` | Empty | `{ "revoked": true }` |

The token is lowercase hexadecimal APNs token bytes, 64 to 512 characters. The body limit is 2048 bytes. The response never returns the token. Registration and unregistering require the existing `read.projections` scope; empty-scope pairings are denied. The `push-registration` capability is granted only when that scope and the exact pairing grant are active on its issuer. A failed optional capability probe reports `ungranted` without failing the capabilities response.

The issuer stores one row per `(person, device)` in `push-registrations.sqlite3` under its state directory. Registration atomically replaces that row. The file is mode `0600` on Unix. SQLite `secure_delete` is best effort for deleted pages; unlinked DELETE-journal copies and filesystem snapshots or host backups can retain bytes. It is separate from the graph store: the registration module makes no graph write, the request middleware records route and headers rather than the body, and `/v1/backup` exports the graph store plus message bodies, not this file. Graph claims, replication, API responses, and graph backup exports do not carry the token. Unregistering removes the exact grant's row. The existing `pairing.revoke` action revokes the device key and signals the graph change before attempting local row deletion. A local deletion failure is logged without aborting the graph revocation; startup or expiry cleanup removes any stale row. On daemon start, cleanup removes rows with expired or revoked grants; a one-shot timer removes the next row at its pairing expiry. Registration wakes that scheduler. There is no periodic scan.

## Source-stage controls

All execution controls below are **UNRUN** at this Draft source stage. Source review must precede execution or qualification.

| Control | Required evidence |
| --- | --- |
| Registration and replacement | One `(person, device)` row; second PUT replaces token and metadata; no token in response or claim body. |
| Unregister and pairing revocation | DELETE removes only the authenticated grant's row; `pairing.revoke` revokes the key even when local push cleanup fails, and blocks a later PUT. |
| Expiry and restart | Expiry timer removes the row; startup cleanup removes expired and revoked rows before use. |
| Authority and isolation | Unpaired, empty-scope, expired, revoked, and nonissuer requests are denied; pairing person equals session authority; another member has no token row. |
| Privacy | File permissions are `0600`; no token appears in graph, replication, response, logs, or graph backup. |
| Contract | Schema and operations manifest parse, generated clients and contract digest match, and the feature-on build passes. |

The source tests in `client_push_registration.rs` and `client_v0.rs` cover replacement, unregister, direct revocation cleanup, expiry cleanup, malformed token, issuer binding, empty-scope denial, optional capability failure, local file permissions, and key revocation when local push storage fails. They remain unrun under this step's source-only constraint. The Rust contract digest was updated from the two raw schema files; client code generation remains unrun and generated client surfaces require an update on an authorized build host before Ready.
