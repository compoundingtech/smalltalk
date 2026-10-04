# iOS compatibility patch

Vendored crates.io netwatch 0.19.3 with its MIT/Apache-2.0 licenses intact.
Only build.rs and src/interfaces/posix_minimal.rs differ: iOS uses the existing
minimal route-monitor implementation and netdev-backed interface enumeration
instead of BSD routing sockets that require macOS-only libc structs/constants.
Default-route/home-router metadata is absent. Native NWPathMonitor callbacks
explicitly call iroh Endpoint::network_change; background stops the carrier.
There is no new polling loop. Validate relay/direct paths and network changes
on a device before treating the spike as a supported transport.
