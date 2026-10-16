//! The server's removed legacy handlers stay out of api.rs and out of every route-family file.

use std::collections::BTreeSet;

#[test]
fn removed_legacy_handlers_are_absent_from_api_and_its_route_families() {
    // api.rs declares each route family right after its "Route families" comment; the list must
    // name every one, so a removed handler cannot come back in a family file unseen.
    let families = [
        ("agents", include_str!("agents.rs")),
        ("client_reads", include_str!("client_reads.rs")),
        ("fleet", include_str!("fleet.rs")),
        ("launches", include_str!("launches.rs")),
        ("messages", include_str!("messages.rs")),
        ("operations", include_str!("operations.rs")),
        ("records", include_str!("records.rs")),
        ("replication", include_str!("replication.rs")),
        ("runs", include_str!("runs.rs")),
        ("sessions", include_str!("sessions.rs")),
        ("work", include_str!("work.rs")),
    ];
    let api = include_str!("../api.rs");
    let declared: BTreeSet<&str> = api
        .split("// Route families:")
        .nth(1)
        .unwrap()
        .lines()
        .skip(2)
        .map_while(|line| line.strip_prefix("mod ")?.strip_suffix(';'))
        .collect();
    assert_eq!(
        declared,
        families.iter().map(|(name, _)| *name).collect::<BTreeSet<_>>(),
        "every route family api.rs declares is scanned"
    );
    let server_source = std::iter::once(api)
        .chain(families.iter().map(|(_, source)| *source))
        .collect::<String>();
    for handler in [
        "watch_resource",
        "unwatch_resource",
        "refresh_resource",
        "reset_runtime",
        "quick_claude",
        "quick_codex",
        "start_mission_run",
    ] {
        assert!(
            !server_source.contains(&format!("fn {handler}(")),
            "legacy server handler {handler} remains compiled"
        );
    }
}
