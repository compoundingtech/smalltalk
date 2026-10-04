# iOS compatibility patch

Vendored from crates.io netdev 0.46.3, with its MIT license and source intact.
The only changes are in src/os/darwin/mod.rs and
src/interface/ipv6_addr_flags.rs: iOS uses the existing unknown IPv6 address-flags
fallback instead of compiling a macOS ioctl that needs libc::in6_ifreq, which
libc does not export on iOS. Interface addresses and routing still use upstream
code. This loses optional tentative/deprecated address metadata on iOS; device
network migration remains part of the proof. The device and simulator builds
verify the platform guard. Remove this patch when upstream supports that target.

Trailing whitespace in the upstream README and workflow was normalized for repository checks.
