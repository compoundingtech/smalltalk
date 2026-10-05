# Smalltalk with Home Manager

Import `inputs.smalltalk.homeManagerModules.default` and configure the user service:

```nix
services.smalltalk = {
  enable = true;
  person = "person/ada";
  declarations = {
    seats = [ ./seat.kdl ];
    missions = [ ./mission.kdl ];
  };
  # ptyPackage = inputs.pty.packages.${pkgs.system}.default;
};
```

The module installs `st3`, writes `st3/config.toml`, and starts a systemd user
service on Linux or a launchd agent on macOS. When declarations are provided,
an additional oneshot service applies seat files and publishes mission files
as `person`, retrying while the daemon starts. `declarationsApply.enable = false`
disables this step. Reapplying is safe, but removing a file does **not** delete
its previously published declaration; this module does not yet use managed-set apply (#646).
The optional `ptyPackage` replaces the bundled `pty` for both the daemon and
seats, working around the executable-directory PATH precedence in #633.

Activation atomically installs a real `st3` executable at `stateDir/bin/st3` (by default
`~/.local/state/st3/bin/st3`) before restarting the daemon, with `st` as an alias in that directory.
The daemon, declaration-apply service, and service PATH use this stable location so running seats
can follow new builds without ending their harness sessions. Unchanged contents are not rewritten.
Package references in the systemd unit or launchd plist still trigger daemon restarts on upgrades.
Seats started from store paths before this change remain stale until restarted; see
[seats across deploys](st3/seat-deploys.md).

Linux user-manager lingering and macOS `st service permissions` remain host
setup prerequisites. Fleet/replication setup is not managed by this module.

See [getting started](getting-started.md) for harness login and your first mission, and
[upgrading st](upgrading-st.md) for checking a deployment.
