//! A person's name and a profile word: one definition, used by the registry to check a tile's
//! person and by the sekrets gateway for profile owners and names.

/// `person/ada` → `ada`.
pub fn person_name(person: &str) -> Option<&str> {
    person
        .strip_prefix("person/")
        .filter(|name| valid_word(name))
}

/// A lowercase word of at most 64 bytes: letters, digits, `-`, `_` and `.`, not starting with
/// `.` or `-`.
pub fn valid_word(word: &str) -> bool {
    !word.is_empty()
        && word.len() <= 64
        && !word.starts_with(['.', '-'])
        && word.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
}
