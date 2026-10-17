# Push registration

A paired phone stores its APNs device token on st so a later sender can reach it. This slice
only stores and removes the token. Nothing sends a push yet: there is no APNs sender, no alert
trigger and no APNs key on any member.

## Commands

`push.register` posts `PushRegistration` to `POST /v1/client/push-registration`.
`push.unregister` sends `DELETE /v1/client/push-registration` with no body.

The generated methods are Rust `push_register` / `push_unregister`, Swift `pushRegister` /
`pushUnregister` and TypeScript `pushRegister` / `pushUnregister`. Both return
`PushRegistrationResponse`:

```json
{"api_version": "st3.client.v0", "request_id": "request/…", "value": {"registered": true, "environment": "development"}}
```

`value` is `PushRegistrationStatus`. `environment` is present exactly when `registered` is true.
The response has no snapshot, fence, action receipt or replicated ACK.

A registration body has exactly four fields:

```json
{
  "token": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "environment": "development",
  "bundle_id": "com.example.st",
  "app_version": "1.4.0 (212)"
}
```

- `token`: the APNs device token as hex, 2–512 characters in whole bytes.
- `environment`: `development` (APNs sandbox) or `production`. The app reports which
  environment its build's entitlement gave it; st does not guess.
- `bundle_id`: 1–255 ASCII letters, digits, `.` or `-`. A sender uses it as the APNs topic.
- `app_version`: 1–64 printable ASCII characters.

Any other field is refused, including a person, device or session identity. The request body
is at most 4096 bytes.

## Who may register

Only a current paired session on the member that issued its pairing may register.
The person and device come from the session, never from the body. Read-only pairings may
register: a registration grants nothing. Anonymous callers, local Unix sessions, pairings
issued by another member, and expired or revoked pairings cannot register.

Unregistration also accepts a cleanup-only proof from the saved bearer of an expired or
revoked locally issued pairing. This exception applies only to exact
`DELETE /v1/client/push-registration`, not reads, registration, or any other route. It does not
create a normal paired session or restore any revoked/expired authority. The issuer checks the
bearer's credential hash against its immutable pairing claim, including the local issuing actor
and exact grant, claim index, person and device, and rechecks that identity in the deletion
transaction. Only a registration belonging to that exact pairing incarnation can be deleted;
an old bearer cannot delete a replacement pairing's registration. A valid proof returns the
existing `{"registered": false}` response even when no matching registration remains.

`GET /v1/client/capabilities` advertises `{"id": "push-registration", "version": 1}`. It is
`granted` for a paired session on its issuer and `unavailable` for local sessions and on other
members. Older servers omit it; a client that does not see it granted must not register.
Cleanup with a saved issuer credential does not require a live capability read or grant.

## Storage

There is one registration per (person, device). Registering again replaces it in place, so the
phone registers on every launch, as Apple asks, and a rotated token wins. `push.unregister`
deletes it and returns `{"registered": false}`, also when nothing was registered.

The registration lives in a local table on the member that issued the pairing. It is not a
claim kind, so it never enters the graph, replication, graph exports or client snapshots.
It survives a daemon restart; full local database backups must be treated as secret-bearing.

## Removal

Registration eligibility never outlives its pairing:

- `pairing.revoke` deletes the pairing's registration in the same step.
- A revocation that reaches the member later by replication deletes it too.
- Expiry immediately prevents registration and hides the registered summary. Independent
  cleanup deletes expired rows on daemon startup and within 30 seconds while running, without
  needing an attention reader or a device window.
  Mutation authorization samples the clock after admission to the writer transaction, so
  waiting in the writer queue never extends a pairing's authority.
- The phone retains durable cleanup ownership for every issuer it may have registered with,
  even after switching members or restarting. This journal contains only pairing-profile keys.
- "Forget this device", independent-member removal and re-pairing clean affected registrations
  before deleting or replacing pairing credentials. The retired bearer can confirm cleanup even
  after expiry/revocation already removed the row. Offline/failed cleanup stays explicit and
  retains credentials for retry; a generic 401/403 is not success. Ownership is released only
  after an explicit `registered: false` confirmation and durable journal removal. Turning the
  switch off never claims successful remote opt-out while it is pending.

## The token is a secret

The token addresses one phone. It never appears in claims, replication, resources, events,
logs, traces or error messages. Validation errors name the field and the rule, never the value.
The device resource exposes only a summary:

```json
"push": {"registered": true, "environment": "production"}
```

`Device.push` is present only on the member that issued the pairing, and is
`{"registered": false}` there when the phone has not registered. Other members omit it.

## Not yet

A sender (APNs signing and delivery), the alert trigger and its deduplication, and the
notification payload and tap handling come in later slices. Until then, a registration has no
effect beyond the device summary above.
