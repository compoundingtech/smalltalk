#![cfg(unix)]
//! `st2 agent address` — the mutable agent address (R24/R25) as one atomic address-book cutover.
//!
//! Address is the third authoring sibling of `st2 rename` and `st2 describe`: it holds the same
//! catalog-authoring lock, resolves exactly one declaration, applies the same `ST_AGENT`
//! self/descendant guardrail, refuses Nix-owned declarations and non-KDL formats, and never
//! touches the subject's immutable `id`. What it adds is host-local address uniqueness, decided
//! against the complete prospective catalog rather than the one declaration being edited.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// One canonical declaration. `extra` carries the address/supervisor/id lines under test.
fn declaration(identity: &str, host: &str, managed_by: &str, extra: &str) -> String {
    format!(
        "// unrelated comment\nagent {identity:?} {{\n  host {host:?}\n  meta {{ managed-by {managed_by:?}; keep \"unchanged\" }}\n{extra}  command \"sleep 300\"\n}}\n"
    )
}

fn run(root: &Path, args: &[&str], actor: Option<&str>) -> Output {
    let mut process = Command::new(env!("CARGO_BIN_EXE_st2"));
    process
        .args(["--catalog", root.to_str().unwrap()])
        .args(args)
        .env_remove("ST_AGENT")
        .env_remove("CATALOG");
    if let Some(actor) = actor {
        process.env("ST_AGENT", actor);
    }
    process.output().unwrap()
}

fn address(root: &Path, args: &[&str], actor: Option<&str>) -> Output {
    let mut full = vec!["agent", "address"];
    full.extend_from_slice(args);
    run(root, &full, actor)
}

fn receipt(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}):\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn address_is_set_changed_and_cleared_with_a_classified_receipt() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(root, "h/alpha/agent.kdl", &declaration("alpha", "h", "catalog", ""));

    let set = address(
        root,
        &["h.alpha", "ops.alpha", "--host", "h", "--json"],
        None,
    );
    assert!(
        set.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&set.stderr)
    );
    let set = receipt(&set);
    assert_eq!(set["result"], "changed");
    assert_eq!(set["id"], "h.alpha", "the immutable ID is what did not change");
    assert_eq!(set["identity"], "h.alpha");
    assert_eq!(set["address"], "ops.alpha");
    assert_eq!(set["busAddress"], "h.ops.alpha");
    assert_eq!(set["retired"], false);

    // Restating the same address is a proven no-op, not a rewrite.
    let same = receipt(&address(
        root,
        &["h.alpha", "ops.alpha", "--host", "h", "--json"],
        None,
    ));
    assert_eq!(same["result"], "unchanged");
    assert_eq!(same["address"], "ops.alpha");

    let changed = receipt(&address(
        root,
        &["h.alpha", "ops.beta", "--host", "h", "--json"],
        None,
    ));
    assert_eq!(changed["result"], "changed");
    assert_eq!(changed["busAddress"], "h.ops.beta");

    // Clearing restores the positional identity fallback as the effective address.
    let cleared = receipt(&address(
        root,
        &["h.alpha", "--clear", "--host", "h", "--json"],
        None,
    ));
    assert_eq!(cleared["result"], "changed");
    assert!(cleared["address"].is_null());
    assert_eq!(cleared["busAddress"], "h.alpha");

    let again = receipt(&address(
        root,
        &["h.alpha", "--clear", "--host", "h", "--json"],
        None,
    ));
    assert_eq!(again["result"], "unchanged");
    assert!(again["address"].is_null());
}

#[test]
fn a_cutover_rewrites_only_the_address_and_preserves_every_other_byte() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let original = declaration("alpha", "h", "catalog", "  id \"0199b8f4-8d3a-7c21-9a44-6f85b7320ea1\"\n");
    write(root, "h/alpha/agent.kdl", &original);

    assert!(
        address(root, &["h.alpha", "ops.alpha", "--host", "h"], None)
            .status
            .success()
    );

    let after = fs::read_to_string(root.join("h/alpha/agent.kdl")).unwrap();
    assert!(
        after.contains("address \"ops.alpha\""),
        "the cutover landed:\n{after}"
    );
    assert!(
        after.contains("id \"0199b8f4-8d3a-7c21-9a44-6f85b7320ea1\""),
        "the immutable id survives an address change:\n{after}"
    );
    // Removing exactly the inserted node must reproduce the original bytes: the comment, the
    // `meta` block, the host, and the command all survive untouched.
    let restored = after
        .lines()
        .filter(|line| line.trim() != "address \"ops.alpha\"")
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    assert_eq!(restored, original);
}

#[test]
fn a_colliding_address_refuses_on_the_same_host_and_is_admitted_on_another() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(root, "h/alpha/agent.kdl", &declaration("alpha", "h", "catalog", ""));
    write(root, "h/beta/agent.kdl", &declaration("beta", "h", "catalog", ""));
    write(root, "g/gamma/agent.kdl", &declaration("gamma", "g", "catalog", ""));

    // `alpha` has no explicit address, so its positional identity is its effective address — an
    // explicit-vs-fallback collision is the same collision.
    let refused = address(
        root,
        &["h.beta", "alpha", "--host", "h", "--json"],
        None,
    );
    assert!(!refused.status.success());
    let refused = receipt(&refused);
    assert_eq!(refused["result"], "error");
    assert_eq!(refused["code"], "address-conflict");
    let message = refused["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("h.alpha") && message.contains("h/alpha/agent.kdl"),
        "the refusal must name the incumbent claimant, not the edited file: {message}"
    );
    assert!(
        !fs::read_to_string(root.join("h/beta/agent.kdl"))
            .unwrap()
            .contains("address"),
        "a refused cutover writes nothing"
    );

    // The same address on another logical host is legal: addresses are unique per host.
    let admitted = receipt(&address(
        root,
        &["g.gamma", "alpha", "--host", "h", "--json"],
        None,
    ));
    assert_eq!(admitted["result"], "changed");
    assert_eq!(admitted["busAddress"], "g.alpha");
}

#[test]
fn clearing_refuses_when_the_identity_fallback_would_collide() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    // `beta` claimed the bytes that are `alpha`'s identity fallback, which is legal while alpha
    // carries an explicit address. Clearing alpha's address would put two subjects on one route.
    write(
        root,
        "h/alpha/agent.kdl",
        &declaration("alpha", "h", "catalog", "  address \"ops\"\n"),
    );
    write(
        root,
        "h/beta/agent.kdl",
        &declaration("beta", "h", "catalog", "  address \"alpha\"\n"),
    );

    let refused = address(root, &["h.alpha", "--clear", "--host", "h", "--json"], None);
    assert!(!refused.status.success());
    let refused = receipt(&refused);
    assert_eq!(refused["code"], "address-conflict");
    assert!(
        fs::read_to_string(root.join("h/alpha/agent.kdl"))
            .unwrap()
            .contains("address \"ops\""),
        "a refused clear leaves the explicit address in place"
    );
}

#[test]
fn address_refuses_invalid_grammar_nix_ownership_non_kdl_and_ambiguity() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(root, "h/alpha/agent.kdl", &declaration("alpha", "h", "catalog", ""));
    write(root, "h/nix/agent.kdl", &declaration("nix", "h", "nix", ""));
    write(
        root,
        "h/legacy/agent.toml",
        "identity = \"legacy\"\nhost = \"h\"\ncommand = \"sleep 300\"\n",
    );
    // One bare identity declared on two hosts: an ordinary reference cannot name one subject.
    write(root, "h/twin/agent.kdl", &declaration("twin", "h", "catalog", ""));
    write(root, "g/twin/agent.kdl", &declaration("twin", "g", "catalog", ""));

    for (target, value, code) in [
        ("h.alpha", "Ops Alpha", "invalid-address"),
        ("h.alpha", "ops..alpha", "invalid-address"),
        ("h.nix", "ops.nix", "nix-managed-declaration"),
        ("h.legacy", "ops.legacy", "unsupported-declaration-format"),
        ("twin", "ops.twin", "target-ambiguous"),
    ] {
        let refused = address(root, &[target, value, "--host", "h", "--json"], None);
        assert!(
            !refused.status.success(),
            "{target} {value} was admitted: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
        assert_eq!(receipt(&refused)["code"], code, "for {target} {value}");
    }
}

#[test]
fn the_actor_guardrail_admits_a_descendant_and_refuses_a_stranger() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(root, "h/root/agent.kdl", &declaration("root", "h", "catalog", ""));
    write(
        root,
        "h/child/agent.kdl",
        &declaration("child", "h", "catalog", "  supervisor \"h.root\"\n"),
    );
    write(root, "h/stranger/agent.kdl", &declaration("stranger", "h", "catalog", ""));

    let refused = address(
        root,
        &["h.stranger", "ops.stranger", "--host", "h", "--json"],
        Some("h.root"),
    );
    assert!(!refused.status.success());
    assert_eq!(receipt(&refused)["code"], "address-not-authorized");

    let admitted = address(
        root,
        &["h.child", "ops.child", "--host", "h", "--json"],
        Some("h.root"),
    );
    assert!(
        admitted.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&admitted.stderr)
    );
    assert_eq!(receipt(&admitted)["address"], "ops.child");
}

#[test]
fn the_exact_id_form_selects_by_id_and_never_falls_through_to_address_lookup() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    // The two subjects cross their bytes: `one`'s address is `two`, and `two`'s address is `one`.
    // An exact-ID selector is catalog-global ID lookup only, so `--id h.two` must name the
    // declaration whose positional identity is `two` — never the subject that owns the address
    // bytes `two`.
    write(
        root,
        "h/one/agent.kdl",
        &declaration("one", "h", "catalog", "  address \"two\"\n"),
    );
    write(
        root,
        "h/two/agent.kdl",
        &declaration("two", "h", "catalog", "  address \"one\"\n"),
    );

    let renamed = run(
        root,
        &["rename", "--id", "h.two", "Renamed", "--host", "h", "--json"],
        None,
    );
    assert!(
        renamed.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&renamed.stderr)
    );
    assert_eq!(receipt(&renamed)["identity"], "h.two");
    assert!(
        fs::read_to_string(root.join("h/two/agent.kdl"))
            .unwrap()
            .contains("name \"Renamed\""),
        "the ID named its own subject"
    );
    assert!(
        !fs::read_to_string(root.join("h/one/agent.kdl"))
            .unwrap()
            .contains("name \"Renamed\""),
        "the subject holding the address bytes was not touched"
    );

    // The same form drives the address cutover.
    let cutover = receipt(&address(
        root,
        &["--id", "h.one", "ops.one", "--host", "h", "--json"],
        None,
    ));
    assert_eq!(cutover["identity"], "h.one");
    assert_eq!(cutover["busAddress"], "h.ops.one");

    // `two` is a live effective address, but it is not an agent ID, and an exact-ID selector
    // never retries its input as an address — so this refuses instead of naming `h/one`.
    let unknown = run(
        root,
        &["rename", "--id", "two", "Nope", "--host", "h", "--json"],
        None,
    );
    assert!(!unknown.status.success());
    let stderr = String::from_utf8_lossy(&unknown.stderr);
    assert!(stderr.contains("'two'"), "stderr:\n{stderr}");
    for identity in ["one", "two"] {
        assert!(
            !fs::read_to_string(root.join(format!("h/{identity}/agent.kdl")))
                .unwrap()
                .contains("Nope"),
            "an exact-ID miss must not fall through to address lookup"
        );
    }

    // Supplying both forms is a clap conflict, not a precedence rule: with `--id`, the reference
    // is off the positional list, so a second positional is exactly what the exclusion catches.
    let both = run(
        root,
        &["rename", "h.one", "Nope", "--id", "h.one", "--host", "h"],
        None,
    );
    assert!(!both.status.success());
    assert!(
        String::from_utf8_lossy(&both.stderr).contains("cannot be used with"),
        "stderr:\n{}",
        String::from_utf8_lossy(&both.stderr)
    );
}

/// One exact-ID selector feeds two different resolvers: authoring matches the positional
/// declaration key, while inbox/status resolution answers on the current address. Handing either
/// resolver the other's string is the ID-through-a-mutable-address hop decision 0015-immutable-agent-id-and-mutable-address forbids, so
/// the same `--id` must work on both sides once a subject's address diverges from its identity.
#[test]
fn the_exact_id_form_serves_declaration_and_route_resolution_alike() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(
        root,
        "h/one/agent.kdl",
        &declaration("one", "h", "catalog", "  address \"chat\"\n"),
    );
    fs::create_dir_all(root.join("h/one/resources/inbox")).unwrap();

    // Declaration side: the authoring receipt names the declaration, not the address.
    let described = run(
        root,
        &["describe", "--id", "h.one", "Owns chat", "--host", "h", "--json"],
        None,
    );
    assert!(
        described.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&described.stderr)
    );
    assert_eq!(receipt(&described)["identity"], "h.one");

    // Route side: the same ID resolves the subject's inbox through its current address.
    let listed = run(
        root,
        &["message", "ls", "--id", "h.one", "--count", "--host", "h"],
        None,
    );
    assert!(
        listed.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&listed.stdout).trim(), "0");

    let status = run(
        root,
        &["status", "--id", "h.one", "--host", "h"],
        None,
    );
    assert!(
        status.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&status.stderr)
    );
}

/// A **parent's** address cutover must leave the org chart and everything the org chart carries
/// intact: `supervisor` is the positional declaration key, so no child declaration changes, and
/// crash-loop notices still reach the renamed parent — even when another subject has since taken
/// the address bytes the parent's children spell.
#[test]
fn a_parents_address_cutover_keeps_the_org_chart_and_its_notifications() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(
        root,
        "h/root/agent.kdl",
        &declaration("root", "h", "catalog", ""),
    );
    write(
        root,
        "h/child/agent.kdl",
        &declaration("child", "h", "catalog", "  supervisor \"h.root\"\n"),
    );
    // The bare reading of the same edge, and a stranger that claims the bytes it spells.
    write(
        root,
        "h/bare/agent.kdl",
        &declaration("bare", "h", "catalog", "  supervisor \"root\"\n"),
    );
    write(
        root,
        "h/impostor/agent.kdl",
        &declaration(
            "impostor",
            "h",
            "catalog",
            "  address \"root\"\n  supervisor \"h.root\"\n",
        ),
    );

    let cutover = receipt(&address(
        root,
        &["h.root", "ops.root", "--host", "h", "--json"],
        None,
    ));
    assert_eq!(cutover["address"], "ops.root");
    assert_eq!(cutover["busAddress"], "h.ops.root");

    // 1. The org chart still validates with both children's `supervisor` values unedited.
    let validated = run(root, &["validate", "--host", "h"], None);
    assert!(
        validated.status.success(),
        "stdout:\n{}stderr:\n{}",
        String::from_utf8_lossy(&validated.stdout),
        String::from_utf8_lossy(&validated.stderr)
    );
    let report = String::from_utf8_lossy(&validated.stdout);
    assert!(report.contains("0 errors"), "{report}");
    for child in ["h/child/agent.kdl", "h/bare/agent.kdl"] {
        let declaration = fs::read_to_string(root.join(child)).unwrap();
        assert!(
            declaration.contains("supervisor"),
            "{child} lost its supervisor edge"
        );
    }

    // 2. Messages route on the new address.
    let sent = run(
        root,
        &[
            "message",
            "send",
            "ops.root",
            "--host",
            "h",
            "--as",
            "h.child",
            "-m",
            "the parent is still reachable",
        ],
        None,
    );
    assert!(
        sent.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&sent.stderr)
    );

    // 3. Crash-loop notices reach the renamed parent through the positional edge — both the
    //    qualified and the bare spelling — and never the subject holding the released bytes.
    for (task, identity, supervisor) in [
        ("h.child-agent", "child", "h.root"),
        ("h.bare-agent", "bare", "root"),
    ] {
        st2::run::surface_crash_loop(
            root,
            "h",
            &st2::run::CrashLoop {
                pty_id: task.to_owned(),
                identity: identity.to_owned(),
                host: Some("h".to_owned()),
                supervisor: Some(supervisor.to_owned()),
            },
        );
    }

    let parent = st2::message::list_dir(&st2::message::inbox_dir(&root.join("h/root"))).unwrap();
    assert_eq!(
        parent.len(),
        3,
        "one message plus two crash-loop notices: {parent:?}"
    );
    assert_eq!(
        parent
            .iter()
            .filter(|message| message.tags.contains(&"crash-loop".to_owned()))
            .count(),
        2,
        "both supervisor spellings notified the renamed parent: {parent:?}"
    );
    let impostor =
        st2::message::list_dir(&st2::message::inbox_dir(&root.join("h/impostor"))).unwrap();
    assert!(
        impostor.is_empty(),
        "the address book must not answer a supervisor edge: {impostor:?}"
    );
}

/// Authoring selects the declaration, never the address — which is what the positional's help
/// text says. After a cutover the subject's own new address does not name it; its declaration key
/// and the exact `--id` form still do.
#[test]
fn authoring_selects_the_declaration_key_and_not_the_current_address() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(
        root,
        "h/alpha/agent.kdl",
        &declaration("alpha", "h", "catalog", ""),
    );

    let cutover = receipt(&address(
        root,
        &["h.alpha", "ops.alpha", "--host", "h", "--json"],
        None,
    ));
    assert_eq!(cutover["address"], "ops.alpha");

    for selector in ["ops.alpha", "h.ops.alpha"] {
        let refused = address(
            root,
            &[selector, "back.alpha", "--host", "h", "--json"],
            None,
        );
        assert!(
            !refused.status.success(),
            "{selector} selected a declaration: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
        assert_eq!(
            receipt(&refused)["code"],
            "target-not-found",
            "for {selector}"
        );

        let renamed = run(
            root,
            &["rename", selector, "Renamed", "--host", "h", "--json"],
            None,
        );
        assert_eq!(
            receipt(&renamed)["code"],
            "target-not-found",
            "for {selector}"
        );
    }

    // Both declaration-key spellings still select, before and after the cutover.
    for selector in ["alpha", "h.alpha"] {
        let admitted = receipt(&address(
            root,
            &[selector, "back.alpha", "--host", "h", "--json"],
            None,
        ));
        assert_eq!(admitted["address"], "back.alpha", "for {selector}");
    }
}

/// Message resolution deduplicates candidates by agent ID, so two declarations sharing one
/// effective ID must refuse rather than deliver into whichever file came first in path order.
#[test]
fn two_declarations_sharing_one_effective_id_refuse_a_reference() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(
        root,
        "h/alpha/agent.kdl",
        &declaration(
            "alpha",
            "h",
            "catalog",
            "  id \"dup\"\n  address \"chat\"\n",
        ),
    );
    write(
        root,
        "h/beta/agent.kdl",
        &declaration("beta", "h", "catalog", "  id \"dup\"\n"),
    );

    let refused = run(
        root,
        &[
            "message", "send", "chat", "--host", "h", "--as", "h.beta", "-m", "who am I",
        ],
        None,
    );
    assert!(
        !refused.status.success(),
        "one id naming two declarations must not deliver: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("ambiguous") && stderr.contains("2 subjects"),
        "{stderr}"
    );
}

/// Both routing planes pin the local host first, so one reference decides identically whether it
/// arrives through `message send` or through stream ingress.
#[test]
fn a_bare_address_declared_on_two_hosts_resolves_to_the_local_subject() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    write(
        root,
        "h/local/agent.kdl",
        &declaration("local", "h", "catalog", "  address \"chat\"\n"),
    );
    write(
        root,
        "g/remote/agent.kdl",
        &declaration("remote", "g", "catalog", "  address \"chat\"\n"),
    );
    write(
        root,
        "h/sender/agent.kdl",
        &declaration("sender", "h", "catalog", ""),
    );

    let sent = run(
        root,
        &[
            "message",
            "send",
            "chat",
            "--host",
            "h",
            "--as",
            "h.sender",
            "-m",
            "local wins",
        ],
        None,
    );
    assert!(
        sent.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&sent.stderr)
    );
    assert_eq!(
        st2::message::list_dir(&st2::message::inbox_dir(&root.join("h/local")))
            .unwrap()
            .len(),
        1
    );
    assert!(
        st2::message::list_dir(&st2::message::inbox_dir(&root.join("g/remote")))
            .unwrap()
            .is_empty(),
        "the foreign host's subject must not receive a locally pinned reference"
    );
}
