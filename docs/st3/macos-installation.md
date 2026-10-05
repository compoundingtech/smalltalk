# macOS installation and signing

Source installs (`scripts/install`) and extracted release installs (`install.sh`) put
`st3` and `stui` inside `~/Applications/SmallTalk.app`. The selected bin directory
contains links to these tools, with `st` linking to `st3`. Other tools remain regular
executables in that directory. Python 3 is required on macOS.

The app path and bundle identifier stay stable across updates. The installer registers
the app with Launch Services and points existing daemon and replication LaunchAgent
plists at its executable. It preserves their arguments and does not restart services
or agent seats. For a first daemon install, run `st service install` from the installed
app executable; for an update, restart the daemon and replication services when ready.

When the `--from` directory also holds `StListen.app` (stui's speech helper, built by
`apps/macos/listen/build.sh DIR` with Xcode's Swift and the macOS 26 SDK), the installer
nests it at `SmallTalk.app/Contents/Helpers/StListen.app`. It signs the helper before the app,
and gives the app the microphone and speech usage strings. macOS asks for the microphone on
behalf of the outermost app, so the prompt names Smalltalk and the answer lasts across updates
signed with the same identity. `scripts/install` and release archives build the helper when they
can; without it, stui's voice mode says that it is missing.

Configure one persistent signing identity to retain its code identity across builds:

```sh
export ST_MACOS_SIGNING_IDENTITY='Developer ID Application: Example Developer (EXAMPLE123)'
export ST_MACOS_SIGNING_TEAM='EXAMPLE123'
scripts/install --from target/release
# An extracted release uses the same settings:
./install.sh
```

The identity can be an exact keychain identity name or its full SHA-1 certificate
fingerprint. It must be explicitly configured: the installer never chooses an identity
from the keychain. When a team is configured, the selected certificate's team is checked
before signing and the installed signature is checked again. A missing, ambiguous or
wrong-team identity fails clearly; it never falls back quietly.

With no signing configuration, installation uses ad-hoc signing. This preserves the
existing installation option, including macOS approval prompts after code changes.
A team without an explicit identity is an error. Persistent signing preserves a stable
designated requirement, but existing macOS permission grants may still need one initial
approval when moving from a prior ad-hoc installation.

Optional settings are `ST_MACOS_APP_PATH`, `ST_MACOS_BIN_DIR`, and
`ST_MACOS_BUNDLE_ID`. Choose a fixed path and identifier and keep them unchanged.
`--bin-dir` on the install wrapper overrides the bin-directory setting. The shared
Python helper also accepts `--identity`, `--team-id`, `--app` and `--identifier`.

Before replacing anything, the installer signs and verifies a complete candidate app.
An unchanged payload with the same signing settings keeps its existing signed bundle.
A changed app replaces the whole bundle through an atomic directory swap on the same
filesystem. The installer keeps the prior app, CLI links/files and service plists under
`~/.local/state/st3/macos-installs/`; if installation fails, it restores them.

Automation can use the helper's explicit transaction modes (`--prepare-only`,
`--backup-only`, `--install-app APP`, `--verify-app APP`, `--restore-app`) with a
persistent `--job DIRECTORY`. Isolated staging/tests can set `--home DIRECTORY`
(or `ST_MACOS_INSTALL_HOME`) so app metadata, service plists and locks stay outside
the real user installation. Back up before `--install-app`, and keep the directory
until deployment health checks pass. Artifact verification/install with a configured full
certificate fingerprint checks the embedded signature without requiring its private key
on the receiving machine. Building/signing a candidate still requires the explicitly
configured keychain identity. The helper restores installation files, not
application databases; schema compatibility must be checked by the deployment caller.
