# Friend trial handoff

**Release hold.** Do not invite a friend to rely on this candidate until the
[current release record](friend-ready-release-status-2026-09-25.md) shows both
the default 24-hour idle and 72-hour bidirectional delivery reports passing.
Those reports support a small connected trial, not a 99.999999% availability
claim. Keep the delivery monitor and fault attention enabled after the trial
starts.

## Install on the friend's machine

From the reviewed `st3` checkout root, use the Nix package. It includes the
daemon CLI, the `st` alias, the `stui` terminal app, `st3-migrate`, and the
pinned `pty` runtime:

```sh
nix profile install .#st3
st3 service install
st3 service status
st3 doctor --strict
st3 now
```

The machine also needs whichever native harness its agent declaration selects.
On macOS, read `st3 service permissions` before expecting
service-owned work to access protected files. Keep the state directory and
service configuration private; do not copy the maintainer fleet's secrets or
state database into the friend's install. The normal local API is a privileged
Unix socket, not a remote endpoint.

With a declared full person identity, run `ST3_PERSON=person/<name> stui` in an
interactive terminal. Without that identity, the local TUI is read-only. Its
Now tab shows actionable person attention; Chat, Control, and Fleet show
sessions, mission work, and machine state. Actions require a live connection
and a current fence; there is no offline mutation queue. The
[TUI guide](../../crates/stui/README.md) lists keys and the PTY smoke test.

For a first mission, use a reviewed example from
[`examples/st3`](../../examples/st3/README.md) and its declared agent rather
than inventing a gate from memory. `st3 missions publish` previews and admits
the exact KDL, while `st3 missions start` pins a run revision. Inspect the
result with `st3 missions show`, `st3 work ls`, `st3 now`, and
`st3 attention ls --as person/<name>`. A sent wake message is not completion:
the step's claim, product, and gate verdict must be visible in the run.

## iOS connection

The current iOS app is a locally built Expo/Xcode app, not an App Store or
TestFlight download. Follow the [iOS build guide](../../apps/ios/README.md),
which distinguishes Debug simulator proof from an offline Release device
build. A physical iPhone installation is optional for this candidate's
verification, so do not imply one has been proven.

On a trusted host, expose **only** the paired client gateway through
tailnet-only HTTPS, following the
[client boundary guide](client-v0/README.md#tailnet-https-carrier). Never expose
the privileged `st3.sock`. Begin a single-use pairing for the intended person
with `st3 devices --as person/<name> pair --full-control "iPhone"` only if this
trusted device should have full person-delegated control, then enter the gateway URL,
pairing ID, and code on the app. The credential belongs in iOS Keychain. Test
Now, Chat (including fenced terminal input on a permitted running terminal),
Control, Fleet, and revocation while connected; revoked credentials must lose
access. Do not place a populated pairing link, private URL, device
ID, or signing identity in a commit or support message.

## If something goes wrong

Keep the graph and append-only soak evidence. Check `st3 doctor --strict`,
`st3 replication status`, the exact mission run, and the relevant attention or
conversation thread before changing state. A failed gate is a hold requiring
diagnosis and a new complete window after remediation; do not edit thresholds
or delete failed probes to obtain a pass. `st3 service reset` erases local state
and is not a routine recovery command. The current client is online-only, and
the live gateway must stay paired-only throughout the trial.
