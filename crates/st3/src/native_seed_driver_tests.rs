use super::*;

#[test]
fn seed_recovery_uses_agent_control_cli_and_requires_reason() {
    let cli = Cli::try_parse_from([
        "st", "agents", "acknowledge-seed", "example",
        "--reason", "accept fresh", "--as", "person/operator",
    ]).unwrap();
    let Command::Agents { command: AgentsCommand::AcknowledgeSeed(args) } = cli.command else {
        panic!("agent seed acknowledgement expected");
    };
    assert_eq!(args.subject, "example");
    assert_eq!(args.reason, "accept fresh");
    assert_eq!(args.actor.as_deref(), Some("person/operator"));
    assert!(Cli::try_parse_from(["st", "agents", "acknowledge-seed", "example"]).is_err());
    assert!(Cli::try_parse_from(["st", "agents", "acknowledge-seed", "example", "--reason", ""]).is_err());
}

#[test]
fn rejects_effective_resume_environment_before_receipt() {
    const CHILD: &str = "ST3_TEST_NATIVE_SEED_CONFLICT";
    if let Some(variable) = std::env::var_os(CHILD) {
        let root = PathBuf::from(std::env::var_os("ST3_TEST_NATIVE_SEED_ROOT").unwrap());
        let client = Client::new(Endpoint::Unix(root.join("no-daemon.sock")));
        let args = DriverArgs {
            driver: "omp".into(),
            subject: Some("agent/example".into()),
            identity: None,
            initial_message: None,
            initial_message_id: None,
            seed: Some(root.join("missing-seed.jsonl")),
            argv: vec!["omp".into()],
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all().build().unwrap();
        let error = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(1), run_driver(&client, args, None))
                .await.expect("seed conflict must fail before daemon access")
                .unwrap_err()
        });
        assert_eq!(error.to_string(), format!(
            "native seed cannot accompany resume environment ({})", variable.to_string_lossy()
        ));
        assert!(!root.join("drivers").exists(), "no inventory or first-launch setup may occur");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let variables = [
        st3::suspension::RESUME_ENV,
        st3::suspension::CONTINUE_ENV,
        st3::suspension::CONTINUE_PATH_ENV,
    ];
    for variable in variables {
        for value in ["native", ""] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.args([
                "--exact",
                "native_seed_driver_tests::rejects_effective_resume_environment_before_receipt",
                "--nocapture",
            ]);
            for variable in variables {
                child.env_remove(variable);
            }
            let output = child.env(variable, value).env(CHILD, variable)
                .env("ST3_TEST_NATIVE_SEED_ROOT", root.path())
                .env("ST3_DRIVER_STATE_DIR", root.path().join("drivers"))
                .env("HOME", root.path()).output().unwrap();
            assert!(output.status.success(), "{variable}={value:?}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr));
        }
    }
}
