use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

#[test]
fn root_requirement_ids_are_unique() {
    let requirements = include_str!("../docs/vrs/requirements.md");
    let mut seen = BTreeSet::new();

    for line in requirements.lines() {
        let Some(rest) = line.strip_prefix("- **R") else {
            continue;
        };
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || !rest[digits..].starts_with(' ') {
            continue;
        }
        let number = rest[..digits]
            .parse::<u32>()
            .expect("requirement ID digits must parse");
        assert!(
            seen.insert(number),
            "duplicate root requirement ID R{number}"
        );
    }
}

/// Four decision numbers were each recorded twice before the collision was noticed. The numbers
/// are historical IDs — 26 inbound references across `docs/`, `src/` and `tests/` cite them — so
/// they are not renumbered; this test ratchets the set instead. A new number falling into a
/// collision fails, a third file joining an existing collision fails, and repairing one fails
/// loudly so the allow-list is updated deliberately. Titles are deliberately not pinned: a
/// retitled decision is not a ledger property.
#[test]
fn root_decision_number_duplicates_match_recorded_history() {
    let decision_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/vrs/.decisions");
    let mut by_number = BTreeMap::<String, BTreeSet<String>>::new();

    for entry in fs::read_dir(decision_dir).expect("root decision directory must be readable") {
        let path = entry.expect("decision entry must be readable").path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some((number, _)) = stem.split_once('-') else {
            continue;
        };
        if number.len() == 4 && number.bytes().all(|byte| byte.is_ascii_digit()) {
            by_number
                .entry(number.to_owned())
                .or_default()
                .insert(stem.to_owned());
        }
    }

    let duplicates = by_number
        .into_iter()
        .filter(|(_, stems)| stems.len() > 1)
        .map(|(number, stems)| (number, stems.len()))
        .collect::<BTreeMap<_, _>>();
    let expected = ["0005", "0007", "0014", "0015"]
        .into_iter()
        .map(|number| (number.to_owned(), 2usize))
        .collect::<BTreeMap<_, _>>();

    assert_eq!(
        duplicates, expected,
        "decision-number collisions changed; each is recorded history, so update this \
         allow-list deliberately rather than renumbering a decision"
    );
}

#[test]
fn root_delta_ids_are_unique() {
    let delta_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/vrs/.delta");
    let mut seen = BTreeSet::new();

    for entry in fs::read_dir(delta_dir).expect("root delta directory must be readable") {
        let path = entry.expect("delta entry must be readable").path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some(rest) = stem.strip_prefix("DELTA-") else {
            continue;
        };
        let Some((number, _)) = rest.split_once('-') else {
            continue;
        };
        if number.len() == 3 && number.bytes().all(|byte| byte.is_ascii_digit()) {
            assert!(
                seen.insert(number.to_owned()),
                "duplicate root delta ID DELTA-{number}"
            );
        }
    }
}

/// The four collided decision numbers are recorded history, so `0015` alone names two different
/// files and a reader cannot tell which. Every citation of a collided number must therefore carry
/// its stem — `0015-immutable-agent-id-and-mutable-address`, not `0015` — and this ratchets that:
/// a new bare citation fails here rather than being discovered by a confused reader.
///
/// Two files legitimately name the bare numbers, because their subject IS the collision.
#[test]
fn collided_decision_numbers_are_never_cited_bare() {
    const COLLIDED: [&str; 4] = ["0005", "0007", "0014", "0015"];
    const DOCUMENTS_THE_COLLISION: [&str; 2] = [
        "docs/vrs/spec.md",
        "docs/vrs/.decisions/0006-observed-harness-state-is-a-driver-written-catalog-record.md",
    ];

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut bare = Vec::new();
    let mut checked = 0usize;

    let mut pending = vec![
        root.join("docs/vrs"),
        root.join("src"),
        root.join("tests"),
        root.join("crates"),
    ];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("citation source directory must be readable") {
            let path = entry.expect("citation source entry must be readable").path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                pending.push(path);
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .expect("walked path is under the manifest directory")
                .to_string_lossy()
                .into_owned();
            let is_source = matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("md" | "rs")
            );
            if !is_source
                || relative == "tests/vrs_ledger.rs"
                || DOCUMENTS_THE_COLLISION.contains(&relative.as_str())
            {
                continue;
            }
            let source = fs::read_to_string(&path).expect("citation source must be readable");
            for (number, line) in source.lines().enumerate() {
                for collided in COLLIDED {
                    // A line that names the stem anywhere — including as a Markdown link target
                    // behind a bare label — has already said which file it means.
                    let stem_named = line.contains(&format!("{collided}-"));
                    let mut rest = line;
                    while let Some(at) = rest.find(collided) {
                        let after = &rest[at + collided.len()..];
                        let inside_longer_token = after
                            .chars()
                            .next()
                            .is_some_and(|character| character.is_numeric())
                            || rest[..at]
                                .chars()
                                .next_back()
                                .is_some_and(|character| character.is_alphanumeric());
                        if !stem_named && !after.starts_with('-') && !inside_longer_token {
                            bare.push(format!("{relative}:{}: {}", number + 1, line.trim()));
                        }
                        if !inside_longer_token {
                            checked += 1;
                        }
                        rest = after;
                    }
                }
            }
        }
    }

    assert!(
        checked >= 20,
        "expected a substantial citation set, found only {checked}"
    );
    assert!(
        bare.is_empty(),
        "a collided decision number is cited without its stem, so it names two files:\n{}",
        bare.join("\n")
    );
}
