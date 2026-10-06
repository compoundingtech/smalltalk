# Device pairing fingerprint design

Agreed design for [#1410](https://github.com/compoundingtech/smalltalk/issues/1410), following [#1353](https://github.com/compoundingtech/smalltalk/pull/1353). Approved on 2026-10-06: the phone uses a separate fingerprint field, and existing devices receive a warning on re-pair.

The current verifier checks grant hashes, signatures, issuer links, and the submitted device key. Its trust anchor comes from the same response: an attacker can replace the person and node keys and return a completely forged, internally valid chain. Pin the person's root public key using a fingerprint obtained from the trusted machine before completing enrollment. Keep the existing grant checks and additionally compare the verified root grant's key with that fingerprint before saving credentials.

## Fingerprint and trusted input

The trusted Unix-only pairing begin operation returns an optional `person_root_fingerprint`, derived from the exact local person-root key used for enrollment. `st devices pair` displays it separately from the one-use code, including in JSON output. Use a versioned, full SHA-256 fingerprint of the canonical public-key representation, encoded as base64url without padding (`sha256:` followed by 43 characters); do not truncate the security comparison. The fingerprint is public. It must come from the trusted machine, never from the completing gateway's anonymous capability response or its returned proofs.

CLI and stui accept `--fingerprint`. Missing or malformed input fails before submitting the code. An explicit, mutually exclusive `--unpinned` override prints a prominent warning that an active attacker can forge the enrollment chain. It still checks grant hashes, signatures, links and the device key. The transport restrictions and encrypted-path notice remain applicable; a fingerprint does not encrypt pairing codes or bearer credentials or prevent an attacker from relaying traffic and stealing a bearer on an unencrypted path.

## Phone fingerprint input

Add a separate fingerprint field beside the phone's pairing ID and code. Paste it from the trusted machine; never populate it from the gateway. Show the same explicit unpinned warning and require a deliberate override when bypassing it.

Debug pairing links open the pairing form with the code prefilled. Completion still requires separate fingerprint input or a deliberate unpinned override; no automatic bypass.

The phone verifies grant hashes, signatures, issuer links, the submitted device key and the root fingerprint before saving credentials. One Keychain profile entry commits its bearer, verified pin and native signing-key handle together. Each candidate key is stored separately, so a failed re-pair does not destroy the previous signing key.

## Existing pairings

Existing saved profiles keep working without invented pins or forced revocation. On re-pair, explain that the old pairing was unpinned and require a trusted fingerprint or the explicit override. Save the verified fingerprint atomically with each new device profile, alongside its bearer and signing key. A different root requires a fresh pairing and explicit input; never replace a stored pin from a remote response.

## Contract and read only pairing

This needs additive optional client-response schema fields: `person_root_fingerprint` on `PairingChallenge`, and `person_root_key_proof` on `PairedSession` for pairings without a device signing grant. Generate the Rust, TypeScript and Swift models from the contract. No new claim kind, database schema, required completion-request field, API version or client rejection policy is proposed. Anonymous capabilities remain static compatibility information with no keys or fingerprints.

Messaging pairings pin the root in the existing two-grant proof chain. Read-only pairings return and verify the existing signed root grant separately, so they can compare the same trusted fingerprint without enrolling a signing device or retaining a private signing key. This root proof does not bind the bearer or response scopes; encrypted transport remains required. Prepare and validate any required root proof before consuming the code. Old clients may ignore the new optional fields. New pinned clients fail explicitly if a member cannot provide the necessary proof; an upgrade is required for pinned read-only pairing against an older member.

## Validation and delivery

Use isolated daemons and invented test identities. Test honest P-256 and Ed25519 enrollment, a fully forged self-consistent chain that passes the old verifier but fails against the trusted fingerprint, wrong and missing fingerprints, the explicit unpinned path, preserved profiles on failure, saved pin reload, read-only pairing without enrollment, and the selected phone and migration flows. Check an older client against the changed daemon and a new client against the pre-change daemon, including the explicit read-only limitation. Keep anonymous discovery free of key material and presence updates.

Put the agreed design in the PR description. Run required checks on the PR's own head, enter the merge queue with `gh pr merge --auto` only when green, and do not rerun cancelled CI jobs. After merge, request deployment from the designated fleet agent and close #1410 with the cause.
