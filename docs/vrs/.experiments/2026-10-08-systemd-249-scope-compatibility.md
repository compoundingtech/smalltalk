# Systemd 249 scope compatibility

Run on 2026-10-08 in Ubuntu 22.04 with systemd
`249 (249.11-0ubuntu3.22)`, user `ada`, machine `studio`.

The container had two CPUs, 3 GiB memory, a private cgroup namespace and no host
mounts. A real systemd user manager created the scopes. This is a container
source proof, not a real VM or release installation proof.

## Source and runtime boundary

Isolation source: `c73ee1ef53891662322034bb0df0ee933424787c`.
The probe compiled the production isolation modules from both `st-runtime` and
`st-drivers`; executable resolution and bounded subprocess helpers came from
production runtime code. It called the public mode and scope wrappers, then
executed real `systemd-run`, rather than a command stand-in.

The service/seat proof used composed source
`2959b4d518c11db95f638dea780577e5f9aaa616`, including the isolation source above,
setup, terminal UI and doctor changes. Its unstripped binary SHA256 was
`186948009bd2749037aed70249cbe488cbd48bd374d83c3eb006555f03e038ad`.
PTY was `0.13.0-rust+15ca74f`.

The host-linked binary failed before main under the stock Ubuntu 22.04 loader:
`GLIBC_2.39` was unavailable. Weak symbol bindings did not make the version
requirement optional. For the source proof only, the container received a
separate glibc 2.42 loader/runtime. The private binary copy's interpreter and
RUNPATH selected that runtime; Ubuntu's systemd and Python remained native.
This does **not** establish native stock Ubuntu 22.04 artifact compatibility.

## Controls

Both production wrappers passed these real-manager controls:

| Control | Result |
| --- | --- |
| Literal `%n:$HOME:${UNSET}:$$:%%` followed by a non-UTF-8 `ff` byte, empty argument, and `two words` | Exact argument bytes preserved. |
| Working directory `~/ws%$literal` | Inherited exactly. |
| Environment value `%n:$HOME:${UNSET}:$$:%%` | Inherited exactly. |
| Standard input containing a NUL, percent and dollar bytes | Preserved; stdout captured and stderr marker received. |
| Child exit code 37 | Propagated through the scope wrapper. |
| Scope creation from a transient user service | Scope and service were siblings under `app.slice`. |
| Stop the launching service with its ordinary cgroup kill policy | Scoped child survived with the same PID and cgroup. |
| `systemctl --user show-environment` fails | Selected `DegradedDetached`. |
| Missing `XDG_RUNTIME_DIR` | Selected `DegradedDetached`. |

Two negative controls distinguished the legacy behavior. Passing
`--expand-environment=no` to real systemd 249 failed with an unrecognized option.
Doubling percent and dollar bytes changed the child's arguments; raw bytes
were already preserved, so an escaping transform would be incorrect.

An initial production-wrapper run with a non-UTF-8 argument failed with
`Failed to start transient scope unit: Bad message`. The generated description
copied argument bytes into a D-Bus string. The fixed `--description=st seat`
removed that interpretation boundary; the same raw argument then passed.

## Service and seat

`st service install` installed the daemon's ordinary user service.
`st agents new scope-recheck --harness codex --workspace "$HOME"/'ws%$literal'
--timeout 90s` returned ready. The real native Codex driver and PTY launched a
provider stand-in speaking the app-server protocol. The stand-in received a
message and replied through the real `st conversations reply` CLI. It did not
call a model or use credentials.

The seat's processes occupied scopes outside `st3.service`. Stopping the daemon
service preserved every recorded seat PID. After starting the daemon service
again, the same seat replied to another message. The fixture waited for API
readiness during replay before asserting doctor health.

The 13 isolation unit tests also passed: five driver tests and eight runtime
tests. No paid provider, macOS, real VM or mixed fleet result is claimed here.
