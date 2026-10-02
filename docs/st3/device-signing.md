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
  - CLI or stui: in a key file.
- A device's public key is written `p256:` followed by the base64url (no padding) of the
  uncompressed SEC1 point (65 bytes). CryptoKit's `x963Representation` gives those bytes.
- A signature is ECDSA over SHA-256, as the fixed 64-byte `r || s`, in base64url without padding.
  CryptoKit's `rawRepresentation` and WebCrypto's raw signatures give those bytes.
- Ed25519 device keys (bare base64url of 32 bytes) are accepted too.

## Enrollment

1. On a trusted machine, the person runs `st devices pair` (or the equivalent client-API call).
   It returns a pairing code.
2. The device sends `POST /v1/client/pairings/{id}/complete` with three fields:
   - `device_public_key`: the device's public key.
   - `key_storage`: `secure-enclave` or `software`. Optional.
   - `code`: the pairing code.
3. The daemon pairs the device as before. When `device_public_key` is a signing key, the daemon
   also enrolls it:
   - The person's root key, which that node holds, writes a `principal.key-granted` claim with
     role `device`.
   - The answer gains `device_key_chain`: the claim IDs of the device's grant and then of the
     person's root grant. The device keeps the chain with its key.
   - A `device_public_key` that is not a signing key pairs the device as before. That device has
     no grant and cannot sign.
4. Revoking the pairing (`pairing.revoke`) also writes `principal.key-revoked` for the key.
   Anything that key signs after the revocation is invalid on every member. What it signed
   before stays verified.

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

Only paired clients sign. A local stui or CLI on the Unix socket acts as the configured person,
and the daemon signs for it with that person's key for the machine.
