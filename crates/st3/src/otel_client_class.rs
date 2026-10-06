//! Classifying the `x-st3-client` header into the closed `st3.client.class` label vocabulary
//! (docs/vrs/06-observability/spec.md: `cli`, `stui`, `fractal`, `web`, `replication-worker`,
//! `omp-channel`, `hook`, plus `app`, `driver`, `peer`; `other` for unrecognized values).
//!
//! The header is a client's own name for itself ("stui 0.1.0+a0c135e3"); st lists it as
//! reported and never treats it as identity or authority (api/client_presence.rs). This
//! classification is likewise observational only: it feeds telemetry labels, never policy.
//!
//! Mapping table, with public producer locations or the reserved shape each rule handles.
//! First match wins; matching is case-insensitive on the trimmed header, bounded to
//! the same 120 chars presence keeps, and allocation-free.
//!
//! | Header shape (lowercased token matched) | Class | Source |
//! | --- | --- | --- |
//! | `stui <version>` | `Stui` | crates/stui/src/version.rs:23-25 (`client_name()`), set at crates/stui/src/main.rs:663 |
//! | `st <machine_version>`, `st agents ls`, `st attention ls`, `st conversations read` | `Cli` | crates/st3/src/main.rs (`st {machine_version}` and CLI subcommands) |
//! | `st3 doctor` | `Cli` | crates/st3/src/main.rs (CLI command family) |
//! | `fractal` | `Fractal` | reserved external client name; docs/vrs/06-observability/spec.md vocabulary |
//! | `smalltalk-ios <version> (<build>)` | `App` | clients/swift/St3Client/Sources/St3Client/Client.swift:11-13, apps/ios/store.tsx:112, clients/typescript/st3-views/src/clientName.ts:2-4 |
//! | `smalltalk-ide <version>` | `App` | clients/typescript/st3-views/src/clientName.ts:7 (test clientName.test.mjs:7), docs/clients/build-your-own.md:98 |
//! | `smalltalk-example-tui <version>` | `App` | examples/client-tui via docs/clients/build-your-own.md:79 — the `smalltalk-` product family prefix |
//! | `st3 replication-worker` | `ReplicationWorker` | crates/st3/src/service.rs (worker command), crates/st3/src/peer.rs (`run_worker`); reserved header shape |
//! | `st3 driver omp-channel`, `agent/<seat> · st3 driver omp-channel` | `OmpChannel` | reserved channel header shape; `omp-channel` wins over the `driver` rule below |
//! | `st3 driver omp`, `st3 driver codex`, `agent/<seat> · st3 driver <driver>` | `Driver` | crates/st3/src/external_sessions.rs (driver commands); synthetic seat prefixes exercise trailing-token matching |
//! | `st3 driver-hook`, `claude-observe`, `claude-statusline`, any `hook` token | `Hook` | crates/st3/src/driver_hook.rs:22-25 (`HOOKS`, `SUBCOMMAND`), crates/st3/src/telemetry.rs:80 (`hook`); reserved — hooks reach the daemon through claims, not this header |
//! | `peer`, `fabric`, `relay` tokens | `Peer` | crates/st3/src/peer.rs:51-68 (peer/client-read routes); reserved — peer traffic authenticates with fleet headers, not `x-st3-client` |
//! | `web`, `browser` tokens | `Web` | reserved for browser clients only (docs/vrs/06-observability/spec.md:377 vocabulary); no producer today |
//! | `curl` | `Other` | ad-hoc client, deliberately not the CLI command family |
//! | anything else present | `Other` | closed-vocabulary fallback (docs/vrs/06-observability/spec.md:372-373) |
//! | header absent, or blank after trim | `Unknown` | `client_name()` treats a blank header as absent (api/client_presence.rs:58-66) |

/// The client classes telemetry counts, as the closed `st3.client.class` vocabulary.
/// `as_str` values are the label strings registered in docs/vrs/06-observability/spec.md
/// (hyphenated there, so kept hyphenated here rather than re-spelled).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientClass {
    Cli,
    Stui,
    Fractal,
    Web,
    App,
    Driver,
    ReplicationWorker,
    OmpChannel,
    Hook,
    Peer,
    Other,
    Unknown,
}

impl ClientClass {
    /// Every variant, in vocabulary order, so enumeration tests can walk the closed set.
    pub const ALL: [ClientClass; 12] = [
        ClientClass::Cli,
        ClientClass::Stui,
        ClientClass::Fractal,
        ClientClass::Web,
        ClientClass::App,
        ClientClass::Driver,
        ClientClass::ReplicationWorker,
        ClientClass::OmpChannel,
        ClientClass::Hook,
        ClientClass::Peer,
        ClientClass::Other,
        ClientClass::Unknown,
    ];

    /// The `st3.client.class` label value.
    pub fn as_str(self) -> &'static str {
        match self {
            ClientClass::Cli => "cli",
            ClientClass::Stui => "stui",
            ClientClass::Fractal => "fractal",
            ClientClass::Web => "web",
            ClientClass::App => "app",
            ClientClass::Driver => "driver",
            ClientClass::ReplicationWorker => "replication-worker",
            ClientClass::OmpChannel => "omp-channel",
            ClientClass::Hook => "hook",
            ClientClass::Peer => "peer",
            ClientClass::Other => "other",
            ClientClass::Unknown => "unknown",
        }
    }

    /// Classify the `x-st3-client` header value: `None` (absent, or blank where presence
    /// already discards it) is [`ClientClass::Unknown`], a present but unrecognized value is
    /// [`ClientClass::Other`], everything else matches the table above. Case-insensitive,
    /// bounded to presence's 120-char name bound, and allocation-free.
    pub fn from_header(value: Option<&str>) -> ClientClass {
        let Some(value) = value else {
            return ClientClass::Unknown;
        };
        let name = value.trim();
        if name.is_empty() {
            return ClientClass::Unknown;
        }
        let name = bounded(name).as_bytes();
        // Order matters: omp-channel and the hook/worker names must win over the driver
        // token they contain, and driver over the `st3 ` CLI prefix it starts with.
        if contains_ascii_ci(name, b"omp-channel") {
            ClientClass::OmpChannel
        } else if contains_ascii_ci(name, b"replication-worker") {
            ClientClass::ReplicationWorker
        } else if contains_ascii_ci(name, b"hook") {
            ClientClass::Hook
        } else if contains_ascii_ci(name, b"driver") {
            ClientClass::Driver
        } else if contains_ascii_ci(name, b"peer")
            || contains_ascii_ci(name, b"fabric")
            || contains_ascii_ci(name, b"relay")
        {
            ClientClass::Peer
        } else if starts_with_ascii_ci(name, b"stui") {
            ClientClass::Stui
        } else if starts_with_ascii_ci(name, b"smalltalk-") {
            ClientClass::App
        } else if starts_with_ascii_ci(name, b"fractal") {
            ClientClass::Fractal
        } else if contains_ascii_ci(name, b"web") || contains_ascii_ci(name, b"browser") {
            ClientClass::Web
        } else if starts_with_ascii_ci(name, b"st ")
            || starts_with_ascii_ci(name, b"st3 ")
            || name.eq_ignore_ascii_case(b"st")
        {
            ClientClass::Cli
        } else {
            ClientClass::Other
        }
    }
}

/// Presence keeps at most this many characters of a client name (api/client_presence.rs).
const MAX_NAME_CHARS: usize = 120;

/// The prefix of `name` presence would have kept, so classification and the stored name
/// always agree on what was seen.
fn bounded(name: &str) -> &str {
    for (index, (offset, _)) in name.char_indices().enumerate() {
        if index == MAX_NAME_CHARS {
            return &name[..offset];
        }
    }
    name
}

/// Case-insensitive ASCII prefix test. Splitting a multi-byte character cannot fake a match:
/// needle bytes are ASCII and UTF-8 continuation bytes never are.
fn starts_with_ascii_ci(name: &[u8], prefix: &[u8]) -> bool {
    name.len() >= prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Case-insensitive ASCII substring test, without lowercasing into a new allocation.
fn contains_ascii_ci(name: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && name
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic client names exercise the supported header shapes and closed vocabulary.
    /// `curl` is deliberately classified `Other`: ad-hoc clients are not our CLI.
    /// Seat-prefixed rows classify by their trailing client token.
    #[test]
    fn synthetic_client_names_classify() {
        let observed = [
            (
                "agent/example/seat-01 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-02 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-03 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-04 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/host-a.example-seat · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-06 · st3 driver omp",
                ClientClass::Driver,
            ),
            ("agent/example/seat-07 · st3 driver omp", ClientClass::Driver),
            (
                "agent/example/seat-08 · st3 driver omp",
                ClientClass::Driver,
            ),
            ("agent/example/seat-09 · st3 driver omp", ClientClass::Driver),
            (
                "agent/example/seat-10 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-11 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-12 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-13 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-14 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-15 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-16 · st3 driver omp",
                ClientClass::Driver,
            ),
            ("agent/example/seat-17 · st3 driver omp", ClientClass::Driver),
            (
                "agent/example/seat-18 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-19 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-20 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-21 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-22 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-23 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-24 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-25 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-25 · st3 driver omp-channel",
                ClientClass::OmpChannel,
            ),
            (
                "agent/example/seat-26 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-27 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-28 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-29 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-30 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-31 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-32 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-33 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-34 · st3 driver omp",
                ClientClass::Driver,
            ),
            (
                "agent/example/seat-35 · st3 driver omp",
                ClientClass::Driver,
            ),
            ("curl", ClientClass::Other),
            ("fractal", ClientClass::Fractal),
            ("st agents ls", ClientClass::Cli),
            ("st attention ls", ClientClass::Cli),
            ("st conversations read", ClientClass::Cli),
            ("st3 doctor", ClientClass::Cli),
            ("st3 driver codex", ClientClass::Driver),
            ("st3 driver omp", ClientClass::Driver),
            ("st3 driver omp-channel", ClientClass::OmpChannel),
            ("st3 replication-worker", ClientClass::ReplicationWorker),
            ("stui", ClientClass::Stui),
        ];
        for (name, class) in observed {
            assert_eq!(ClientClass::from_header(Some(name)), class, "{name:?}");
        }
        // Synthetic recognized names land in a named class; `curl` remains Other.
        assert!(
            observed
                .iter()
                .filter(|(_, class)| !matches!(class, ClientClass::Other | ClientClass::Unknown))
                .count()
                >= 46
        );
    }

    /// Names produced by public sources, plus reserved client shapes.
    #[test]
    fn current_producers_classify() {
        // stui/src/version.rs:23, st3-client/src/lib.rs:106.
        assert_eq!(
            ClientClass::from_header(Some("stui 0.1.0+a0c135e3")),
            ClientClass::Stui
        );
        // st3/src/main.rs:4482.
        assert_eq!(
            ClientClass::from_header(Some("st 25.11.0+ab12cd3")),
            ClientClass::Cli
        );
        assert_eq!(ClientClass::from_header(Some("st")), ClientClass::Cli);
        // clientName.ts and Client.swift:11.
        assert_eq!(
            ClientClass::from_header(Some("smalltalk-ios 1.2.0 (a0c135e3)")),
            ClientClass::App
        );
        assert_eq!(
            ClientClass::from_header(Some("smalltalk-ide 2.0.0")),
            ClientClass::App
        );
        // Reserved buckets: hooks (driver_hook.rs:22-25), peers (peer.rs), browsers.
        assert_eq!(
            ClientClass::from_header(Some("st3 driver-hook claude-observe")),
            ClientClass::Hook
        );
        assert_eq!(
            ClientClass::from_header(Some("st3 peer relay")),
            ClientClass::Peer
        );
        assert_eq!(
            ClientClass::from_header(Some("acme-browser 0.3")),
            ClientClass::Web
        );
        // Case and surrounding whitespace do not matter.
        assert_eq!(
            ClientClass::from_header(Some("  ST3 Driver OMP-Channel  ")),
            ClientClass::OmpChannel
        );
    }

    /// Absent is Unknown, blank is Unknown (presence discards it), junk is Other.
    #[test]
    fn absent_blank_and_junk_classify() {
        assert_eq!(ClientClass::from_header(None), ClientClass::Unknown);
        assert_eq!(ClientClass::from_header(Some("")), ClientClass::Unknown);
        assert_eq!(ClientClass::from_header(Some("   ")), ClientClass::Unknown);
        assert_eq!(
            ClientClass::from_header(Some("anonymous-pairing-probe")),
            ClientClass::Other
        );
        assert_eq!(
            ClientClass::from_header(Some("somebody else's client 9")),
            ClientClass::Other
        );
        // A name that only differs after the 120-char bound presence keeps.
        let mut long = "x".repeat(119);
        long.push_str("st agents ls");
        assert_eq!(ClientClass::from_header(Some(&long)), ClientClass::Other);
        // The same name inside the bound classifies, proving the bound is what binds.
        assert_eq!(
            ClientClass::from_header(Some("st agents ls")),
            ClientClass::Cli
        );
    }

    /// ALL is the closed vocabulary, and its label values are unique, lowercase identifiers
    /// with no spaces — the registered `st3.client.class` axis (hyphenated where the spec
    /// registers hyphens, e.g. `replication-worker`).
    #[test]
    fn all_covers_every_variant_with_unique_labels() {
        assert_eq!(
            ClientClass::ALL,
            [
                ClientClass::Cli,
                ClientClass::Stui,
                ClientClass::Fractal,
                ClientClass::Web,
                ClientClass::App,
                ClientClass::Driver,
                ClientClass::ReplicationWorker,
                ClientClass::OmpChannel,
                ClientClass::Hook,
                ClientClass::Peer,
                ClientClass::Other,
                ClientClass::Unknown,
            ]
        );
        let mut labels: Vec<_> = ClientClass::ALL
            .iter()
            .map(|class| class.as_str())
            .collect();
        labels.sort_unstable();
        let unique = labels.len() == 12;
        assert!(unique, "duplicate label values");
        for label in labels {
            assert!(
                label.chars().all(|character| character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || character == '-'),
                "{label} is not a lowercase label"
            );
            assert!(
                !label.starts_with('-') && !label.ends_with('-'),
                "{label} has a stray hyphen"
            );
        }
    }
}
