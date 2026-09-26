use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

fn raw_string_start(line: &str) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    for index in 0..bytes.len() {
        if index > 0
            && (bytes[index - 1].is_ascii_alphanumeric() || matches!(bytes[index - 1], b'_' | b'"'))
        {
            continue;
        }
        let mut cursor = match bytes[index..] {
            [b'r', ..] => index + 1,
            [b'b', b'r', ..] => index + 2,
            _ => continue,
        };
        let mut hashes = 0;
        while bytes.get(cursor) == Some(&b'#') {
            hashes += 1;
            cursor += 1;
        }
        if bytes.get(cursor) == Some(&b'"') {
            return Some((cursor + 1, hashes));
        }
    }
    None
}

fn raw_string_closes(line: &str, hashes: usize) -> bool {
    line.as_bytes()
        .windows(hashes + 1)
        .any(|window| window.first() == Some(&b'"') && window[1..].iter().all(|byte| *byte == b'#'))
}

fn declared_tests(source: &str) -> BTreeSet<String> {
    let mut tests = BTreeSet::new();
    let mut modules = Vec::<(usize, String)>::new();
    let mut saw_test_attribute = false;
    let mut raw_string_hashes = None;

    for line in source.lines() {
        if let Some(hashes) = raw_string_hashes {
            if raw_string_closes(line, hashes) {
                raw_string_hashes = None;
            }
            continue;
        }
        if line.trim_start().starts_with("//") {
            continue;
        }
        let code = line;
        if let Some((content_start, hashes)) = raw_string_start(code) {
            if !raw_string_closes(&code[content_start..], hashes) {
                raw_string_hashes = Some(hashes);
            }
            continue;
        }
        let trimmed = code.trim();
        let indentation = code.len() - code.trim_start().len();
        if trimmed == "}" {
            while modules
                .last()
                .is_some_and(|(module_indentation, _)| *module_indentation >= indentation)
            {
                modules.pop();
            }
        }

        let declaration = trimmed.strip_prefix("pub ").unwrap_or(trimmed);
        if let Some(rest) = declaration.strip_prefix("mod ")
            && let Some((name, _)) = rest.split_once('{')
        {
            modules.push((indentation, name.trim().to_owned()));
            saw_test_attribute = false;
            continue;
        }
        if trimmed.starts_with("#[") {
            if trimmed.contains("test") {
                saw_test_attribute = true;
            }
            continue;
        }
        if !saw_test_attribute || trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }

        let declaration = declaration.strip_prefix("async ").unwrap_or(declaration);
        if let Some(rest) = declaration.strip_prefix("fn ")
            && let Some((name, _)) = rest.split_once('(')
        {
            let name = name.trim();
            tests.insert(name.to_owned());
            if !modules.is_empty() {
                let mut qualified = modules
                    .iter()
                    .map(|(_, module)| module.as_str())
                    .collect::<Vec<_>>()
                    .join("::");
                qualified.push_str("::");
                qualified.push_str(name);
                tests.insert(qualified);
            }
        }
        saw_test_attribute = false;
    }

    tests
}

#[test]
fn declared_tests_preserve_module_qualification() {
    let source = r##"
#[test]
fn root_proof() {}

#[cfg(test)]
mod tests {
    fn fixture() -> &'static str {
        r#"
}
"#
    }

    #[test]
    fn module_proof() {}

    mod nested {
        #[test]
        fn nested_proof() {}
    }
}
"##;
    let tests = declared_tests(source);

    assert!(tests.contains("root_proof"));
    assert!(tests.contains("module_proof"));
    assert!(tests.contains("tests::module_proof"));
    assert!(tests.contains("tests::nested::nested_proof"));
    assert!(!tests.contains("wrong::module_proof"));
}

#[test]
fn qualified_proof_references_resolve() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let invariants = include_str!("../INVARIANTS.md");
    let mut checked = 0;

    for (index, span) in invariants.split('`').enumerate() {
        if index % 2 == 0 {
            continue;
        }
        let Some((source_path, test_name)) = span.split_once(".rs::") else {
            if span.ends_with(".rs") {
                assert!(
                    root.join(span).is_file(),
                    "invariant proof source `{span}` does not exist"
                );
            }
            continue;
        };
        let relative_path = format!("{source_path}.rs");

        let source = fs::read_to_string(root.join(&relative_path))
            .unwrap_or_else(|error| panic!("proof source {relative_path} is unreadable: {error}"));
        let tests = declared_tests(&source);
        assert!(
            tests.contains(test_name),
            "invariant proof `{span}` does not name a declared test"
        );
        checked += 1;
    }

    assert!(
        checked >= 20,
        "expected a substantial qualified proof set, found only {checked}"
    );
}
