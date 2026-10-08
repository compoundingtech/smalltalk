# Device signing

A device holds its own key, enrolls it when it pairs, and signs what it sends. Every member
verifies the signature, and attributes the message to the person on that device, whichever
gateway carried it. The protocol below is version 1. The shared test vectors are in
[`fixtures/clients/device-signing-v1.json`](../../fixtures/clients/device-signing-v1.json).
Every client's tests check against that file.

## Keys

- Each device makes a P-256 key pair and keeps the private half on the device:
  - iOS: in the Secure Enclave, with a software key in the Keychain on the simulator.
  - Browser: as a non-extractable WebCrypto key.
  - CLI or terminal UI: in a key file.
- A device's public key is written `p256:` followed by the base64url (no padding) of the
  uncompressed SEC1 point (65 bytes). CryptoKit's `x963Representation` gives those bytes.
- A signature is ECDSA over SHA-256, as the fixed 64-byte `r || s`, in base64url without padding.
  CryptoKit's `rawRepresentation` and WebCrypto's raw signatures give those bytes.
- Ed25519 device keys (bare base64url of 32 bytes) are accepted too.

## Enrollment

1. On a trusted machine, the person runs `st devices pair` (or the equivalent client-API call).
   It returns a pairing code and a separate person-root fingerprint. Copy the fingerprint from this trusted machine independently of the code.
2. The device sends `POST /v1/client/pairings/{id}/complete` with three fields:
   - `device_public_key`: the device's public key.
   - `key_storage`: `secure-enclave` or `software`. Optional.
   - `code`: the pairing code.
3. The daemon pairs the device as before. When `device_public_key` is a signing key and the
   pairing grants `control.messages` (`st devices pair --full-control`), the daemon also enrolls
   it. A device paired only to read, such as a display, gets no key that speaks for the person.
   Enrolment:
   - The person's root key, which that node holds, writes a `principal.key-granted` claim with
     role `device`.
   - The answer gains `device_key_chain`: the claim IDs of the device's grant and then of the
     person's root grant. The device keeps the chain with its key.
   - It also returns optional `device_key_proofs` in chain order: each grant's ID, batch ID,
     subject, kind, origin, actor, body, predecessors and existing claim signature. These public
     receipts let a completing client check content hashes, signatures and issuer/role links,
     including that the device grant binds the public key it submitted. They provide no new
     authority. The completing client compares the verified person-root key with the fingerprint
     supplied separately from the trusted machine. A completely forged, self-consistent chain
     fails that comparison. A read-only pairing returns `person_root_key_proof` without enrolling
     a signing device. That receipt verifies the root key but does not bind the bearer or scopes.
     Use an encrypted path for the code and bearer even when the fingerprint is pinned.
   - A `device_public_key` that is not a signing key pairs the device as before. That device has
     no grant and cannot sign.
4. Revoking the pairing (`pairing.revoke`) also writes `principal.key-revoked` for the key.
   Anything that key signs after the revocation is invalid on every member. What it signed
   before stays verified.

## CLI and terminal client pairing

On the trusted machine, begin a pairing as the person:

```sh
st devices pair cli-device --as person/avery --full-control
```

On the device, complete it using the member gateway's HTTP or HTTPS origin and the returned
pairing ID. The command prompts privately for the single-use code, or reads it from stdin:

```sh
st devices complete https://member.example pairing/PAIRING_ID --fingerprint sha256:FINGERPRINT
```

HTTP is allowed by default for loopback, private addresses and tailnet addresses, including
Tailscale's `100.64.0.0/10` range. Hostnames must resolve entirely to those addresses.
**An HTTP address must already be an encrypted path**, such as Tailscale/WireGuard or an SSH
tunnel: the pairing code and bearer credentials cross it. Both commands print this notice when
using HTTP. Other HTTP addresses are refused by default. On an already encrypted path, use
`--allow-public-http` with `st devices complete` to override that restriction.
The override is saved as `allow_public_http: true` on that device in the private profile;
existing profiles can set that field explicitly. The override also permits ambient HTTP proxies;
ensure that the proxy path is encrypted and trusted too. HTTPS requires no override.
The same address restriction applies to every plain WebSocket stream connection and reconnect.

The default generates a P-256 software key. Use `--algorithm ed25519` for Ed25519. To import
an existing private key, add `--key-file /absolute/path/device.der` and select its algorithm;
the file must contain DER PKCS#8, belong to your user, and have private permissions (0600).
The import file is read without following symlinks and is never changed.

The bearer credential, delegation chain and private key are stored together in one mode-0600
profile, `$XDG_CONFIG_HOME/st3/stui-devices.json` (or `~/.config/st3/stui-devices.json`).
`--profile /absolute/path/devices.json` selects another profile in a directory owned by you
and not writable by other users. New profile directories are created with mode 0700; an existing
mode-0755 config directory is accepted without changing its permissions.
One atomic rename replaces both credential and key together; concurrent updates are refused.
Wrong or reused codes, invalid keys and failures before that commit leave the existing profile
unchanged. If response validation or saving fails after the member consumes the code, the error
identifies the possible orphaned device and gives a revoke hint. On the trusted machine, inspect
`st devices ls --as person/avery`, then revoke that device with
`st devices revoke device/DEVICE_ID --as person/avery` before beginning another pairing.
The new device may have a live bearer even though the client retained its previous profile.
If a connection fails before an answer arrives, inspect the trusted device list because the
client cannot know whether the member consumed the code.
Neither ordinary output nor `--json` prints the bearer or private key.

Paired terminal UI messages are signed using the saved key and chain. Existing legacy
profiles remain readable. A pairing without `control.messages`, including `--read-only`,
enrolls no signing key and persists no private signing material.

Completion checks the private key with a local signing self-test before sending the existing
pairing request. It does not add a server proof-of-possession requirement or send a message.
Before sending the single-use code, it anonymously reads `GET /v1/client/capabilities` and
requires `device-key-proofs` version 1. This unauthenticated response contains only `api_version`
and that one capability. It is static: no machine version, snapshot, event cursor, limits, scopes,
actions or client-presence update. Authenticated capability reads retain their full response.
Older members are refused without submitting or consuming the code.
The member prepares and checks both proofs before recording completion or creating the bearer.
If a historical root has no usable signature or content hash, completion refuses without consuming
its code; inspect signing history on the trusted member. Every refusal after enrollment calls
`revoke_device_key` best-effort, recording `principal.key-revoked` for the prepared key and sealing
that revocation. This withdraws signing authority without rewriting the original grant. Retry with
a fresh signing key. A process crash or a storage failure can prevent cleanup and leave an unused
signing grant; inspect principal grant and revocation history on the trusted member in that case.
There is no completed device record for such a grant in `st devices ls`; pairing revocation only
covers completed devices. The trusted begin response carries optional `person_root_fingerprint`.
The anonymous advertisement never supplies a fingerprint or key.
New completions require a full person-root fingerprint (`sha256:` plus the 43-character
base64url SHA-256 digest of the canonical public-key string). Obtain it from the trusted machine
and pass `--fingerprint`; do not copy a key or fingerprint from the completing gateway. Missing
or malformed fingerprints fail before the code is submitted. `--unpinned` explicitly bypasses
this identity check and prints a loud warning that an active attacker can forge the chain;
hashes, signatures, issuer links and submitted-key binding are still checked. The two options
are mutually exclusive. The verified pin is saved atomically with the bearer and key.
Existing profiles keep working. Re-pairing an unpinned profile prints a migration warning;
ordinary connections do not. A new root requires fresh pairing with explicit trusted input.

The phone has a separate fingerprint field and a deliberate, warned unpinned override. Debug
pairing links prefill the pairing form and never submit the code automatically. The phone
verifies the same public grants before committing a single Keychain profile containing the
bearer, pin and reference to a separate native signing key; failure preserves its previous
profile and key. Older clients ignore the optional response fields and continue to work.
A new client can pin a messaging pairing against a member that already supplies v1 device
proofs. Pinned read-only pairing needs the added root-proof response; an older member's reply
is refused with the orphan/revoke guidance above, unless `--unpinned` was explicitly selected.

Before committing a messaging profile it verifies the returned device and person-root grant
proofs, their content hashes and signatures, the device-to-root-to-node issuer/role links, and
the first grant's binding to its submitted public key. A member that does not return those
proofs must be upgraded; the client refuses to save its response and reports the possible
orphaned device. Existing profiles remain readable. Use the prompt for the pairing code;
putting a literal code in an `echo` command or command argument can retain it in shell history.
The shared signer's integration tests complete both key types, reload their profiles, send real
signed messages, and check `Verified` verdicts on the accepting member and a receiving peer.

## Signing a message

The device adds `parameters.signature` to `message.send`. Servers that predate this ignore it.

```json
{
  "signer": "person/NAME",
  "key": "p256:…",
  "chain": ["<device grant ID>", "<person root grant ID>"],
  "nonce": "<16 random bytes, base64url>",
  "signed_at_unix_ms": 1791000000000,
  "signature": "<base64url r||s>",
  "format": "fields-v1",
  "signed_fields": ["content", "from", "in_reply_to", "session_id", "tags", "title", "to"]
}
```

The signed bytes are UTF-8 lines joined by `\n`, with no trailing newline:

```text
smallclaims-claim-fields-v1
message/<first 16 hex characters of sha256(idempotency_key)>
message.sent
person/NAME
content=<value>
from=<value>
in_reply_to=<value>
session_id=<value>
tags=<value>
title=<value>
to=<value>
person/NAME            (the signer)
                       (on whose behalf: empty)
p256:…                 (the key)
<chain IDs joined by ,>
<nonce>
<signed_at_unix_ms>
```

Each `<value>` is the field's JSON in RFC 8785 (JCS) form: compact, object keys sorted, `/` not
escaped, non-ASCII not escaped. The values are the ones the daemon stores:

- `from` is the device's person: `person_id` from the pairing answer.
- `to` is the canonical subject, such as `agent/alder`. A bare name is refused.
- A missing `title`, `in_reply_to` or `session_id` is `null`.
- `tags` is the array in the client's order, and `[]` when there are none.

## What the daemon checks before it writes the message

| Refusal | When |
| --- | --- |
| `device-signature-format` | The format is not `fields-v1`, or the signed fields differ from the list above. |
| `device-signature-signer` | The signer is not the paired person, or the signature names someone it acts on behalf of. |
| `device-signature-noncanonical` | `to` is not a canonical subject. |
| `device-signature-stale` | `signed_at_unix_ms` is more than 15 minutes from the accepting daemon's clock. |
| `device-signature-replayed` | Another claim already carries this key and nonce. A retry with the same idempotency key gets the first answer instead. |
| `device-key-not-enrolled` | The chain's first grant does not grant this key to this person. |
| `device-signature-invalid` | The signature does not match the message. |

Only the daemon that first accepts the send checks the time. Every member checks the signature
and its chain when the claim arrives, and records the verdict (`st doctor`, claim-signatures).
It never checks the time, so the verdict is the same everywhere, whatever order claims arrive
in.

## Scope of version 1

Only paired clients sign. A local terminal UI or CLI on the Unix socket acts as the configured person,
and the daemon signs for it with that person's key for the machine.
