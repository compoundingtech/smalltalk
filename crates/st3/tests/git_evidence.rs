//! Offline CLI contract: transporting/importing evidence needs no daemon or signing key.
use st3::evidence::{
    self, Check, Fact, Observation, Policy, Statement, Subject, TrustTier, Verdict,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};

/// The evidence API also invokes Git in this process. Run the entire test in a child with
/// explicit tools and private home/state instead of changing the parallel harness's environment.
fn isolated(test: &str) -> bool {
    const CHILD: &str = "ST3_GIT_EVIDENCE_ISOLATED_TEST";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        return false;
    }
    let root = tempfile::tempdir().unwrap();
    let tools = root.path().join("tools");
    let home = root.path().join("home");
    let state = root.path().join("state");
    let config = root.path().join("config");
    let tmp = root.path().join("tmp");
    for path in [&tools, &home, &state, &config, &tmp] {
        std::fs::create_dir(path).unwrap();
    }
    for program in ["git", "ssh-keygen", "python3", "timeout"] {
        let real = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .filter(|directory| !directory.join("st3-recorder.json").exists())
            .filter_map(|directory| std::fs::canonicalize(directory.join(program)).ok())
            .filter(|path| {
                !matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("st3" | "st" | "st3-fixture")
                )
            })
            .find(|path| {
                std::fs::metadata(path)
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
            .unwrap_or_else(|| {
                panic!("a real {program} must be available outside recorder directories")
            });
        std::os::unix::fs::symlink(&real, tools.join(program)).unwrap();
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env_clear()
        .env(CHILD, test)
        .env("PATH", &tools)
        .env(
            "ST3_GIT_EVIDENCE_REAL_GIT",
            std::fs::canonicalize(tools.join("git")).unwrap(),
        )
        .env("HOME", &home)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_CONFIG_HOME", &config)
        .env("TMPDIR", &tmp)
        .env("NEXTEST_BIN_EXE_st3", test_bin!("st3"))
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("running 1 test"));
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

fn git(repo: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn cli(repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new(test_bin!("st3"))
        .args([
            "--endpoint",
            "unix:/no-evidence-daemon.sock",
            "--daemon-wait",
            "0",
            "evidence",
        ])
        .args(args)
        .arg("--repo")
        .arg(repo)
        .output()
        .unwrap()
}

fn success(output: std::process::Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn put_show_verify_work_offline_for_tests_review_and_explicit_degraded_tier() {
    if isolated(
        "git_evidence::put_show_verify_work_offline_for_tests_review_and_explicit_degraded_tier",
    ) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let repo = root.path();
    git(repo, &["init", "-q"], b"");
    let policy = Policy {
        version: 1,
        risk_class: "daemon".into(),
        required_checks: BTreeSet::from(["unit-tests".into()]),
        allowed_tiers: BTreeSet::from([TrustTier::Full]),
        test_signers: BTreeSet::from(["fleet/fixture-observer".into()]),
        review_signers: BTreeSet::from(["fleet/fixture-observer".into()]),
    };
    let blob = git(
        repo,
        &["hash-object", "-w", "--stdin"],
        &serde_json::to_vec(&policy).unwrap(),
    );
    let config = git(
        repo,
        &["mktree"],
        format!("100644 blob {blob}\tevidence-policy.json\n").as_bytes(),
    );
    let tree = git(
        repo,
        &["mktree"],
        format!("040000 tree {config}\t.st\n").as_bytes(),
    );
    let base = git(repo, &["commit-tree", &tree], b"base\n");
    let head = git(repo, &["commit-tree", &tree, "-p", &base], b"head\n");
    let key = repo.join("throwaway-key");
    let generated = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .output()
        .unwrap();
    assert!(generated.status.success());
    let trusted = repo.join("allowed-signers");
    std::fs::write(
        &trusted,
        format!(
            "fleet/fixture-observer namespaces=\"git\" {}",
            std::fs::read_to_string(key.with_extension("pub")).unwrap()
        ),
    )
    .unwrap();
    let trusted = trusted.to_str().unwrap();
    let mut statement = Statement {
        statement_type: evidence::STATEMENT_TYPE.into(),
        predicate_type: evidence::PREDICATE_TYPE.into(),
        subject: vec![Subject {
            name: head.clone(),
            digest: BTreeMap::from([("gitTree".into(), tree.clone())]),
        }],
        predicate: Observation {
            schema_version: 1,
            change_id: "fixture-change".into(),
            signer: Some("fleet/fixture-observer".into()),
            tier: TrustTier::Full,
            re_attests: None,
            producer_sha256: "a".repeat(64),
            observed_at: chrono::Utc::now().to_rfc3339(),
            fact: Fact::Tests {
                inputs: BTreeMap::new(),
                runner: "fixture-runner".into(),
                checks: BTreeMap::from([(
                    "unit-tests".into(),
                    Check {
                        command: vec!["cargo".into(), "test".into()],
                        exit_code: 0,
                        output_sha256: "b".repeat(64),
                    },
                )]),
            },
        },
    };
    let tests = evidence::sign_observation(repo, &statement, &key).unwrap();
    assert_eq!(
        success(cli(
            repo,
            &["put", "--record", &tests, "--trusted-signers", trusted]
        ))["tier"],
        "full"
    );
    assert_eq!(
        success(cli(repo, &["show", "--record", &tests]))["predicate"]["observed"]["kind"],
        "tests"
    );
    assert_eq!(
        success(cli(
            repo,
            &["verify", "--record", &tests, "--trusted-signers", trusted]
        ))["signature_verified"],
        true
    );
    statement.predicate.fact = Fact::Review {
        reviewer: "agent/fixture-reviewer".into(),
        verdict: Verdict::GO,
        base_commit: base.clone(),
        files: evidence::changed_files(repo, &base, &head).unwrap(),
        tests: vec![tests.clone()],
    };
    let review = evidence::sign_observation(repo, &statement, &key).unwrap();
    success(cli(
        repo,
        &["put", "--record", &review, "--trusted-signers", trusted],
    ));
    assert_eq!(
        success(cli(repo, &["show", "--record", &review]))["predicate"]["observed"]["verdict"],
        "GO"
    );
    let gate = success(cli(
        repo,
        &[
            "verify",
            "--revision",
            &head,
            "--base",
            &base,
            "--change-id",
            "fixture-change",
            "--trusted-signers",
            trusted,
        ],
    ));
    assert_eq!(gate["review"], review);
    assert_eq!(gate["tier"], "full");
    // The target cannot lower requirements: policy comes from the accepted base.
    let mut stricter = policy;
    stricter.required_checks.insert("security-tests".into());
    let strict_blob = git(
        repo,
        &["hash-object", "-w", "--stdin"],
        &serde_json::to_vec(&stricter).unwrap(),
    );
    let strict_config = git(
        repo,
        &["mktree"],
        format!("100644 blob {strict_blob}\tevidence-policy.json\n").as_bytes(),
    );
    let strict_tree = git(
        repo,
        &["mktree"],
        format!("040000 tree {strict_config}\t.st\n").as_bytes(),
    );
    let strict_base = git(
        repo,
        &["commit-tree", &strict_tree, "-p", &head],
        b"accepted strict policy\n",
    );
    let weakened_head = git(
        repo,
        &["commit-tree", &tree, "-p", &strict_base],
        b"weaken policy\n",
    );
    let mut weakened = statement.clone();
    weakened.subject[0].name = weakened_head.clone();
    weakened.predicate.observed_at = chrono::Utc::now().to_rfc3339();
    weakened.predicate.fact = Fact::Review {
        reviewer: "agent/fixture-reviewer".into(),
        verdict: Verdict::GO,
        base_commit: strict_base.clone(),
        files: evidence::changed_files(repo, &strict_base, &weakened_head).unwrap(),
        tests: vec![tests],
    };
    let weakened_go = evidence::sign_observation(repo, &weakened, &key).unwrap();
    success(cli(
        repo,
        &[
            "put",
            "--record",
            &weakened_go,
            "--trusted-signers",
            trusted,
        ],
    ));
    let rejected = cli(
        repo,
        &[
            "verify",
            "--revision",
            &weakened_head,
            "--base",
            &strict_base,
            "--change-id",
            "fixture-change",
            "--trusted-signers",
            trusted,
        ],
    );
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("required checks missing"));
    statement.predicate.signer = None;
    statement.predicate.tier = TrustTier::Degraded;
    let degraded = evidence::record_degraded(repo, &statement).unwrap();
    success(cli(
        repo,
        &["put", "--record", &degraded, "--trusted-signers", trusted],
    ));
    let result = success(cli(
        repo,
        &[
            "verify",
            "--record",
            &degraded,
            "--trusted-signers",
            trusted,
        ],
    ));
    assert_eq!(result["tier"], "degraded");
    assert_eq!(result["signature_verified"], false);
    // An explicitly degraded review cannot supply a GO when trusted keys disappear.
    std::fs::write(trusted, "").unwrap();
    assert!(
        !cli(
            repo,
            &[
                "verify",
                "--revision",
                &head,
                "--base",
                &base,
                "--change-id",
                "fixture-change",
                "--trusted-signers",
                trusted
            ]
        )
        .status
        .success()
    );
}

#[cfg(unix)]
#[test]
fn protected_ci_cli_fetches_selectors_and_exports_verified_objects_without_a_key() {
    if isolated(
        "git_evidence::protected_ci_cli_fetches_selectors_and_exports_verified_objects_without_a_key",
    ) {
        return;
    }
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let caller = root.path().join("caller");
    let private = root.path().join("profile");
    let fixtures = root.path().join("api");
    let tools = root.path().join("installed-tools");
    for directory in [&caller, &private, &fixtures, &tools] {
        std::fs::create_dir(directory).unwrap();
    }
    git(&caller, &["init", "-q"], b"");
    let blob = git(
        &caller,
        &["hash-object", "-w", "--stdin"],
        b"checked source",
    );
    let recipe = b"fixture-ci-source-recipe";
    let recipe_digest = hex::encode(Sha256::digest(recipe));
    let recipe_blob = git(&caller, &["hash-object", "-w", "--stdin"], recipe);
    let scripts = git(
        &caller,
        &["mktree"],
        format!("100644 blob {recipe_blob}\tci-evidence-source\n").as_bytes(),
    );
    let workflow = b"fixture-workflow";
    let workflow_digest = hex::encode(Sha256::digest(workflow));
    let workflow_blob = git(&caller, &["hash-object", "-w", "--stdin"], workflow);
    let workflows = git(
        &caller,
        &["mktree"],
        format!("100644 blob {workflow_blob}\tfleet.yml\n").as_bytes(),
    );
    let github = git(
        &caller,
        &["mktree"],
        format!("040000 tree {workflows}\tworkflows\n").as_bytes(),
    );
    let tree = git(
        &caller,
        &["mktree"],
        format!("040000 tree {github}\t.github\n100644 blob {blob}\tcheck.rs\n040000 tree {scripts}\tscripts\n").as_bytes(),
    );
    let source = git(&caller, &["commit-tree", &tree], b"synthetic checkout");
    git(&caller, &["update-ref", "refs/heads/main", &source], b"");
    let scratch = private.join("observer.git");
    git(
        &private,
        &["init", "-q", "--bare", scratch.to_str().unwrap()],
        b"",
    );
    let key = private.join("throwaway-key");
    assert!(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .status()
            .unwrap()
            .success()
    );
    let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
    let allowed = root.path().join("allowed-signers");
    std::fs::write(
        &allowed,
        format!("fleet/fixture-observer namespaces=\"git\" {public}"),
    )
    .unwrap();
    let object_list = format!(
        "{source}\n{tree}\n{blob}\n{scripts}\n{recipe_blob}\n{github}\n{workflows}\n{workflow_blob}\n"
    );
    let mut child = Command::new("git")
        .arg("-C")
        .arg(&caller)
        .args(["pack-objects", "--stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(object_list.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let pack = output.stdout;
    std::fs::write(fixtures.join("source.pack"), &pack).unwrap();
    let receipt = serde_json::json!({
        "version":1, "repository":"example/project", "event":"merge_group",
        "run_id":"123", "run_attempt":"2", "source":source, "tree":tree,
        "object_format":"sha1", "change_id":"github:example/project#7",
        "producer_sha256":recipe_digest, "pack_sha256":hex::encode(Sha256::digest(&pack)),
        "pack_bytes":pack.len()
    });
    std::fs::write(
        fixtures.join("receipt.json"),
        serde_json::to_vec(&receipt).unwrap(),
    )
    .unwrap();
    assert!(Command::new("python3").args(["-c",
        "import pathlib,sys,zipfile; p=pathlib.Path(sys.argv[1]); z=zipfile.ZipFile(p/'artifact.zip','w'); [z.write(p/n,n) for n in ('receipt.json','source.pack')]; z.close()"])
        .arg(&fixtures).status().unwrap().success());
    let zip = std::fs::read(fixtures.join("artifact.zip")).unwrap();
    let metadata = serde_json::json!({
        "id":456,"expired":false,"name":"ci-evidence-source-2","workflow_run":{"id":123},
        "size_in_bytes":zip.len(),"digest":format!("sha256:{}",hex::encode(Sha256::digest(&zip)))
    });
    let run = serde_json::json!({
        "id":123,"run_attempt":2,"repository":{"full_name":"example/project"},
        "name":"Workspace CI","path":".github/workflows/fleet.yml","status":"completed",
        "event":"merge_group","head_sha":source,"pull_requests":[{"number":7}],
        "head_branch":format!("gh-readonly-queue/main/pr-7-{source}")
    });
    let jobs = serde_json::json!({"jobs":[
        {"id":10,"run_id":123,"run_attempt":2,"name":"evidence-source","status":"completed",
            "conclusion":"success","steps":[
                {"name":"Retain effective tested source","status":"completed","conclusion":"success"},
                {"name":"Upload effective tested source","status":"completed","conclusion":"success"}]},
        {"id":11,"run_id":123,"run_attempt":2,"name":"unit-tests","status":"completed",
            "conclusion":"success","completed_at":(chrono::Utc::now()-chrono::Duration::minutes(10)).to_rfc3339()}]});
    for (name, value) in [
        ("run.json", run),
        ("jobs.json", jobs),
        ("metadata.json", metadata),
    ] {
        std::fs::write(fixtures.join(name), serde_json::to_vec(&value).unwrap()).unwrap();
    }
    std::fs::create_dir_all(private.join(".config/st")).unwrap();
    std::fs::write(
        private.join(".config/st/evidence-ci.json"),
        serde_json::to_vec(&serde_json::json!({
            "repository":"example/project","scratch_repo":scratch,
            "recipe_sha256":recipe_digest,"required_checks":["unit-tests"],
            "accepted_base":source,"workflow_sha256":workflow_digest,
            "tier":"full",
            "signer":"fleet/fixture-observer","ssh_key":key
        }))
        .unwrap(),
    )
    .unwrap();
    let gh = tools.join("gh");
    // The interpreter is the python3 the isolated harness put on PATH, by absolute path: a `#!/usr/bin/env` line
    // needs /usr/bin/env, which the Nix build sandbox does not have (there the fake gh could not start).
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join("python3"))
        .find(|path| path.is_file())
        .expect("the isolated harness supplies python3 on PATH");
    std::fs::write(
        &gh,
        format!(
            "#!{}\n{}",
            python.display(),
            r#"import os, pathlib, sys
assert sys.argv[1:4] == ['api', '--hostname', 'github.com']
suffix = sys.argv[4].removeprefix('repos/example/project/')
files = {'actions/runs/123/attempts/2':'run.json',
         'actions/runs/123/attempts/2/jobs?per_page=100&page=1':'jobs.json',
         'actions/artifacts/456':'metadata.json',
         'actions/artifacts/456/zip':'artifact.zip',
         'actions/runs/123/attempts/3':'run3.json',
         'actions/runs/123/attempts/3/jobs?per_page=100&page=1':'jobs3.json',
         'actions/artifacts/789':'metadata3.json',
         'actions/artifacts/789/zip':'artifact3.zip'}
sys.stdout.buffer.write((pathlib.Path(os.environ['FIXTURE_API']) / files[suffix]).read_bytes())
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let real_git = std::env::var_os("ST3_GIT_EVIDENCE_REAL_GIT")
        .expect("the isolated harness supplies the genuine Git executable");
    let installed_git = tools.join("git");
    std::fs::write(
        &installed_git,
        format!(
            "#!{}\n{}",
            python.display(),
            r#"import os, sys
args = sys.argv[1:]
if 'fetch' in args:
    expected = 'https://github.com/example/project.git'
    assert expected in args and 'transfer.fsckObjects=true' in args
    args[args.index(expected)] = os.environ['FIXTURE_SOURCE']
os.execv(os.environ['FIXTURE_REAL_GIT'],[os.environ['FIXTURE_REAL_GIT'],*args])
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&installed_git, std::fs::Permissions::from_mode(0o755)).unwrap();
    let observe_request = |attempt: &str, artifact: &str, extra: &[&str]| {
        let mut paths = vec![tools.clone()];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        Command::new(test_bin!("st3"))
            .args([
                "--endpoint",
                "unix:/absent.sock",
                "--daemon-wait",
                "0",
                "evidence",
                "observe-ci",
                "--run",
                "123",
                "--attempt",
                attempt,
                "--artifact",
                artifact,
            ])
            .args(extra)
            .current_dir(&caller)
            .env("HOME", &private)
            .env("XDG_CONFIG_HOME", private.join(".config"))
            .env("XDG_STATE_HOME", private.join(".local/state"))
            .env("FIXTURE_API", &fixtures)
            .env("FIXTURE_SOURCE", &caller)
            .env("FIXTURE_REAL_GIT", &real_git)
            .env("PATH", std::env::join_paths(paths).unwrap())
            .output()
            .unwrap()
    };
    let observer = |extra: &[&str]| observe_request("2", "456", extra);
    assert!(!observer(&["--statement", "forged.json"]).status.success());
    assert!(
        !observer(&["--ssh-key", key.to_str().unwrap()])
            .status
            .success()
    );
    let result = success(observer(&[]));
    assert_eq!(result["source"], source);
    assert_eq!(result["records"][0]["tier"], "full");
    assert!(result["records"][0]["newly_recorded"].as_bool().unwrap());
    let exported = base64::engine::general_purpose::STANDARD
        .decode(result["object_pack_base64"].as_str().unwrap())
        .unwrap();
    assert!(
        !exported
            .windows(b"OPENSSH PRIVATE KEY".len())
            .any(|bytes| bytes == b"OPENSSH PRIVATE KEY")
    );
    let mut importer = Command::new("git")
        .arg("-C")
        .arg(&caller)
        .args(["index-pack", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    importer.stdin.take().unwrap().write_all(&exported).unwrap();
    assert!(importer.wait().unwrap().success());
    let export_check = root.path().join("export-check");
    std::fs::create_dir(&export_check).unwrap();
    git(&export_check, &["init", "-q"], b"");
    let mut import = Command::new("git")
        .arg("-C")
        .arg(&export_check)
        .args(["index-pack", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    import.stdin.take().unwrap().write_all(&exported).unwrap();
    assert!(import.wait().unwrap().success());
    let private_oid = git(
        &caller,
        &["hash-object", "--stdin"],
        &std::fs::read(&key).unwrap(),
    );
    assert!(
        !Command::new("git")
            .arg("-C")
            .arg(&export_check)
            .args(["cat-file", "-e", &private_oid])
            .output()
            .unwrap()
            .status
            .success()
    );
    let record = result["records"][0]["record"].as_str().unwrap();
    let verified = evidence::verify_record(&caller, record, &allowed).unwrap();
    assert_eq!(verified.subject[0].name, source);
    evidence::put(&caller, record, &allowed).unwrap();
    assert!(
        !success(observer(&[]))["records"][0]["newly_recorded"]
            .as_bool()
            .unwrap()
    );
    let mut next_job: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixtures.join("jobs.json")).unwrap()).unwrap();
    next_job["jobs"][1]["id"] = serde_json::json!(12);
    std::fs::write(
        fixtures.join("jobs.json"),
        serde_json::to_vec(&next_job).unwrap(),
    )
    .unwrap();
    let moved_key = private.join("offline-throwaway-key");
    std::fs::rename(&key, &moved_key).unwrap();
    let degraded = success(observer(&[]));
    assert_eq!(degraded["records"][0]["tier"], "degraded");
    assert_eq!(degraded["metrics"]["fresh_records"], 1);
    assert_eq!(degraded["metrics"]["full_records"], 0);
    assert_eq!(degraded["metrics"]["full_trust_share"], 0.0);
    std::fs::rename(&moved_key, &key).unwrap();
    let replay = success(observer(&[]));
    assert_eq!(
        replay["records"][0]["record"],
        degraded["records"][0]["record"]
    );
    assert_eq!(replay["records"][0]["tier"], "degraded");
    assert_eq!(replay["metrics"]["fresh_records"], 0);
    assert!(replay["metrics"]["full_trust_share"].is_null());
    let refresh_artifact = || {
        assert!(Command::new("python3").args(["-c",
            "import pathlib,sys,zipfile; p=pathlib.Path(sys.argv[1]); z=zipfile.ZipFile(p/'artifact.zip','w'); [z.write(p/n,n) for n in ('receipt.json','source.pack')]; z.close()"])
            .arg(&fixtures).status().unwrap().success());
        let zip = std::fs::read(fixtures.join("artifact.zip")).unwrap();
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixtures.join("metadata.json")).unwrap())
                .unwrap();
        metadata["size_in_bytes"] = serde_json::json!(zip.len());
        metadata["digest"] = format!("sha256:{}", hex::encode(Sha256::digest(&zip))).into();
        std::fs::write(
            fixtures.join("metadata.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
    };
    let mut pr_run: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixtures.join("run.json")).unwrap()).unwrap();
    pr_run["event"] = "pull_request".into();
    std::fs::write(
        fixtures.join("run.json"),
        serde_json::to_vec(&pr_run).unwrap(),
    )
    .unwrap();
    next_job["jobs"][1]["id"] = serde_json::json!(13);
    std::fs::write(
        fixtures.join("jobs.json"),
        serde_json::to_vec(&next_job).unwrap(),
    )
    .unwrap();
    let mut updated_receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixtures.join("receipt.json")).unwrap()).unwrap();
    updated_receipt["event"] = "pull_request".into();
    std::fs::write(
        fixtures.join("receipt.json"),
        serde_json::to_vec(&updated_receipt).unwrap(),
    )
    .unwrap();
    refresh_artifact();
    let pr_result = success(observer(&[]));
    assert_eq!(pr_result["full_trust_eligible"], false);
    assert_eq!(pr_result["records"][0]["tier"], "degraded");
    assert!(pr_result["trust_limitation"].is_string());
    // The unsigned predecessor is published to the fixture's public repository;
    // the protected observer fetches it independently, then authenticates its old job.
    let predecessor = pr_result["records"][0]["record"].as_str().unwrap();
    let old_pack = base64::engine::general_purpose::STANDARD
        .decode(pr_result["object_pack_base64"].as_str().unwrap())
        .unwrap();
    let mut import_old = Command::new("git")
        .arg("-C")
        .arg(&caller)
        .args(["index-pack", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    import_old
        .stdin
        .take()
        .unwrap()
        .write_all(&old_pack)
        .unwrap();
    assert!(import_old.wait().unwrap().success());
    evidence::put(&caller, predecessor, &allowed).unwrap();
    let now = chrono::Utc::now();
    let mut rerun = pr_run.clone();
    rerun["run_attempt"] = 3.into();
    rerun["event"] = "merge_group".into();
    std::fs::write(
        fixtures.join("run3.json"),
        serde_json::to_vec(&rerun).unwrap(),
    )
    .unwrap();
    let mut rerun_jobs = next_job.clone();
    for job in rerun_jobs["jobs"].as_array_mut().unwrap() {
        job["run_attempt"] = 3.into();
    }
    rerun_jobs["jobs"][1]["id"] = 14.into();
    rerun_jobs["jobs"][1]["started_at"] = (now - chrono::Duration::minutes(4)).to_rfc3339().into();
    rerun_jobs["jobs"][1]["completed_at"] =
        (now - chrono::Duration::minutes(1)).to_rfc3339().into();
    std::fs::write(
        fixtures.join("jobs3.json"),
        serde_json::to_vec(&rerun_jobs).unwrap(),
    )
    .unwrap();
    let mut rerun_receipt = updated_receipt.clone();
    rerun_receipt["event"] = "merge_group".into();
    rerun_receipt["run_attempt"] = "3".into();
    std::fs::write(
        fixtures.join("receipt3.json"),
        serde_json::to_vec(&rerun_receipt).unwrap(),
    )
    .unwrap();
    assert!(Command::new("python3").args(["-c",
        "import pathlib,sys,zipfile; p=pathlib.Path(sys.argv[1]); z=zipfile.ZipFile(p/'artifact3.zip','w'); z.write(p/'receipt3.json','receipt.json'); z.write(p/'source.pack','source.pack'); z.close()"])
        .arg(&fixtures).status().unwrap().success());
    let rerun_zip = std::fs::read(fixtures.join("artifact3.zip")).unwrap();
    let mut rerun_metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixtures.join("metadata.json")).unwrap()).unwrap();
    rerun_metadata["id"] = 789.into();
    rerun_metadata["name"] = "ci-evidence-source-3".into();
    rerun_metadata["size_in_bytes"] = serde_json::json!(rerun_zip.len());
    rerun_metadata["digest"] = format!("sha256:{}", hex::encode(Sha256::digest(&rerun_zip))).into();
    std::fs::write(
        fixtures.join("metadata3.json"),
        serde_json::to_vec(&rerun_metadata).unwrap(),
    )
    .unwrap();
    let linked = success(observe_request("3", "789", &["--re-attest", predecessor]));
    assert_eq!(linked["records"][0]["tier"], "full");
    assert_eq!(linked["metrics"]["re_attested_records"], 1);
    assert!(
        linked["metrics"]["re_attestation_elapsed_ms"]
            .as_i64()
            .unwrap()
            >= 599_000
    );
    let linked_record = linked["records"][0]["record"].as_str().unwrap();
    let imported = root.path().join("linked-export");
    std::fs::create_dir(&imported).unwrap();
    git(&imported, &["init", "-q"], b"");
    let linked_pack = base64::engine::general_purpose::STANDARD
        .decode(linked["object_pack_base64"].as_str().unwrap())
        .unwrap();
    let mut importer = Command::new("git")
        .arg("-C")
        .arg(&imported)
        .args(["index-pack", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    importer
        .stdin
        .take()
        .unwrap()
        .write_all(&linked_pack)
        .unwrap();
    assert!(importer.wait().unwrap().success());
    assert_eq!(
        evidence::verify_record(&imported, linked_record, &allowed)
            .unwrap()
            .predicate
            .re_attests
            .as_deref(),
        Some(predecessor)
    );
    assert_eq!(
        evidence::show(&imported, predecessor)
            .unwrap()
            .predicate
            .tier,
        TrustTier::Degraded
    );
    let linked_replay = success(observe_request("3", "789", &["--re-attest", predecessor]));
    assert_eq!(linked_replay["metrics"]["re_attested_records"], 0);
    assert!(linked_replay["metrics"]["re_attestation_elapsed_ms"].is_null());
    // An authenticated result with a changed old-job body cannot supply a re-attestation.
    let mut changed_old = next_job.clone();
    changed_old["jobs"][1]["extra"] = "different historical body".into();
    std::fs::write(
        fixtures.join("jobs.json"),
        serde_json::to_vec(&changed_old).unwrap(),
    )
    .unwrap();
    assert!(
        !observe_request("3", "789", &["--re-attest", predecessor])
            .status
            .success()
    );
    std::fs::write(
        fixtures.join("jobs.json"),
        serde_json::to_vec(&next_job).unwrap(),
    )
    .unwrap();
    // Authenticated artifact bytes can still contain malformed objects. Strict import
    // refuses an extra unreachable bad commit, even though the source itself is valid.
    let bad = git(
        &caller,
        &[
            "hash-object",
            "-w",
            "--literally",
            "-t",
            "commit",
            "--stdin",
        ],
        b"not a valid commit\n",
    );
    let prefix = fixtures.join("malformed");
    let packed = git(
        &caller,
        &["pack-objects", prefix.to_str().unwrap()],
        format!("{object_list}{bad}\n").as_bytes(),
    );
    let malformed = std::fs::read(fixtures.join(format!("malformed-{packed}.pack"))).unwrap();
    std::fs::write(fixtures.join("source.pack"), &malformed).unwrap();
    updated_receipt["pack_bytes"] = serde_json::json!(malformed.len());
    updated_receipt["pack_sha256"] = hex::encode(Sha256::digest(&malformed)).into();
    std::fs::write(
        fixtures.join("receipt.json"),
        serde_json::to_vec(&updated_receipt).unwrap(),
    )
    .unwrap();
    refresh_artifact();
    assert!(!observer(&[]).status.success());
    // A source ZIP with a changed byte is refused before an observation is produced.
    let mut damaged = std::fs::read(fixtures.join("artifact.zip")).unwrap();
    damaged[10] ^= 1;
    std::fs::write(fixtures.join("artifact.zip"), damaged).unwrap();
    assert!(!observer(&[]).status.success());
}
