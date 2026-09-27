# Smalltalk binary releases

Every pushed Git tag runs **Smalltalk tag release**. Both native builds must pass before a
GitHub release is published. Each release contains:

- `smalltalk-x86_64-unknown-linux-gnu.tar.gz`: Linux x86_64, built on Ubuntu 22.04 (glibc 2.35 or newer).
- `smalltalk-aarch64-apple-darwin.tar.gz`: Apple Silicon, macOS 15 or newer. The executables are
  ad-hoc signed, not Developer ID signed or notarized; macOS may require approval to open them.
- One `.sha256` checksum per archive, combined `SHA256SUMS`, and `RELEASE.json` with the exact
  source commit, target, Rust compiler version, and PTY runtime revision.

Each archive has `bin/st3`, `bin/st` (a relative symlink to `st3`), `bin/stui`, `bin/st3-migrate`,
and `bin/pty`, plus `install.sh`, `BUILD.json`, and this guide. PTY is built from the exact
`flake.lock` runtime revision; this is distinct from the `pty-core` library dependency. These
archives need neither Nix nor Rust installed. Harness CLIs and their logins remain separate.

## Install or update

Choose a tag from the repository's Releases page, then download the archive for your machine
and its `.sha256` file. For example, with GitHub CLI:

```sh
tag=v0.1.0 # replace with the release you want
archive=smalltalk-x86_64-unknown-linux-gnu.tar.gz
# On Apple Silicon: archive=smalltalk-aarch64-apple-darwin.tar.gz
gh release download "$tag" --repo compoundingtech/smalltalk \
  --pattern "$archive" --pattern "$archive.sha256"
shasum -a 256 -c "$archive.sha256"
tar -xzf "$archive"
"./${archive%.tar.gz}/install.sh" --bin-dir "$HOME/.local/bin"
```

Put the chosen bin directory on `PATH`. The installer stages all four tools and the `st` link,
then replaces each by rename; existing processes retain their old executable. It does not restart
anything, change configuration, or erase state. Keep the previous archive to roll back using the
same procedure. A first daemon install uses `st3 service install`. For an existing daemon, schedule
`st3 service restart` after installation and verify `st3 doctor --strict` and peer health. Restarting
invalidates an ongoing continuous soak window, so coordinate that separately from downloading or
installing files.

## Build and publication proof

PRs changing release files and manual **Run workflow** invocations run both native builds,
archive extraction, a temporary-directory installation, CLI help checks, the TUI PTY smoke suite,
and combined checksum/source validation. They upload `smalltalk-release` as an Actions artifact
and never publish a GitHub release. Tag pushes use the same jobs, then verify that the tag still
points at the built commit, upload a draft, and publish it only after every asset is present.
Existing releases are not overwritten. If publication fails leaving a draft, inspect and remove
that draft before rerunning the publish job; never move a published tag.

Tag names are labels, not embedded package versions: use `BUILD.json`/`RELEASE.json` for exact
source identity. Tag the tested commit (with this workflow in its tree); pushing a tag does not
wait for unrelated CI runs. Tags made with another workflow's default `GITHUB_TOKEN` do not
trigger another push workflow, and GitHub does not emit tag push events for a push of more than
three tags: push release tags individually from a person or app credential.

The runner and trigger choices follow GitHub's [runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
and [push event rules](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#push).
The prior `release-portable.yml` remains the separate manual st2 release path.
