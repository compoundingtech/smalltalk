# Fractal with Home Manager

Import `inputs.smalltalk.homeManagerModules.default` and configure the user service:

```nix
services.smalltalk = {
  accessMode = "auto";
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

On Linux, `memoryMax` (default `8G`) and the optional `memoryHigh` set the daemon's systemd memory
limits; `null` leaves a limit unset. Size `memoryMax` above the host's measured peak RSS: catch-up
after downtime raises memory use, and an OOM kill restarts the daemon into the same catch-up.

Activation atomically installs a real `st3` executable at `stateDir/bin/st3` (by default
`~/.local/state/st3/bin/st3`) before restarting the daemon, with `st` as an alias in that directory.
The daemon, declaration-apply service, and service PATH use this stable location so running seats
can follow new builds without ending their harness sessions. Unchanged contents are not rewritten.
Package references in the systemd unit or launchd plist still trigger daemon restarts on upgrades.
Seats started from store paths before this change remain stale until restarted; see
[seats across deploys](st3/seat-deploys.md).

## Optional Fractal command

Neither the default Nix package nor the default Home Manager configuration installs
`fractal`; updating a Smalltalk pin does not claim that command name. To opt in for
the Home Manager-owned stable executable:

```nix
programs.smalltalk.fractalBinary.enable = true;
```

With `services.smalltalk.enable = true` and `binary.path = null`, activation creates
`stateDir/bin/fractal -> st3`. Add that stable bin directory to your interactive PATH
to use it. `fractal` and `fractal tui` open the terminal UI, `fractal web` opens the web
UI, and other arguments behave like `st`. The existing `st` and `st3` commands do not
change.

Activation leaves foreign files, directories, and symlinks at `stateDir/bin/fractal`
untouched and prints guidance; `st` remains usable. A relative `fractal -> st3` link
is recognized as Smalltalk-owned. Disabling the option removes only that recognized
link, not a foreign command. With an externally deployed `binary.path`, this option
is ignored: the external deployer remains responsible for aliases.

For a standalone Nix-profile installation, explicitly select the flake's `#fractal`
package instead of the default package. It contains only `bin/fractal`, a symlink to
the same `st3` executable; install `#st3` alongside it if you also want the existing
commands in your profile. Unlike module activation, Nix profile collision handling
governs profile-installed commands.

## Externally deployed binary

If another deployer updates your Nix profile or installs the executable, set
`binary.path` to its stable absolute path:

```nix
services.smalltalk = {
  enable = true;
  person = "person/ada";
  binary.path = "/opt/smalltalk/bin/st3";
};
```

The default is `null`, which keeps the installation and restart behavior above.
When a path is set, Home Manager does not install `package`, copy an executable
to `stateDir/bin`, or create the `st` alias there. The daemon and declaration
commands execute `binary.path`, and their PATH starts with its containing
directory. The daemon's `--pty-binary` also selects `pty` in that directory.
The systemd units and launchd agents omit the Fractal and PTY package store
paths and the package restart trigger. Changing only `package` or `ptyPackage`
therefore does not restart the daemon on a Home Manager switch.

The external deployer must install the executable before the service starts,
provide any CLI aliases (such as `st`), install `pty` beside the configured
executable, and restart the daemon when an upgrade needs it. `ptyPackage` is
ignored in this mode, so a profile update selects both tools from the same
stable directory.
For running seats to follow replacements, use a real executable at a stable
path: a Nix-profile symlink alone resolves to the package's store executable
and does not provide the replaceable executable described in
[seats across deploys](st3/seat-deploys.md).

## Host setup

Linux user-manager lingering and macOS `st service permissions` remain host
setup prerequisites. Fleet/replication setup is not managed by this module.

See [getting started](getting-started.md) for harness login and your first mission, and
[upgrading st](upgrading-st.md) for checking a deployment.
