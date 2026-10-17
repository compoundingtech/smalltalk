use super::*;

#[test]
fn plain_st_requires_two_terminals_and_no_seat() {
    for arguments in [1, 2] {
        for stdin in [false, true] {
            for stdout in [false, true] {
                for seat in [false, true] {
                    assert_eq!(
                        plain_ui_requested(arguments, stdin, stdout, seat),
                        arguments == 1 && stdin && stdout && !seat
                    );
                }
            }
        }
    }
    assert!(Cli::try_parse_from(["st"]).is_err());
}

#[test]
fn terminal_ui_arguments_and_help_match_the_documented_surface() {
    Cli::command().debug_assert();
    for flag in [
        "--client",
        "--local",
        "--space",
        "--demo",
        "--glass",
        "--glasses",
    ] {
        let mut argv = vec!["st", "ui", flag];
        if matches!(flag, "--space" | "--glass") {
            argv.push("harbor");
        }
        assert!(matches!(
            Cli::try_parse_from(argv).unwrap().command,
            Command::Ui(_)
        ));
    }
    assert!(Cli::try_parse_from(["st", "ui", "--client", "--local"]).is_err());
    // `--classic` is gone: it still parses, so stui can say what to run instead, and help
    // no longer names it.
    assert!(matches!(
        Cli::try_parse_from(["st", "ui", "--classic"]).unwrap().command,
        Command::Ui(_)
    ));
    assert!(Cli::try_parse_from(["st", "ui", "pair"]).is_err());
    assert!(Cli::try_parse_from(["st", "ui", "--space"]).is_err());
    assert!(
        Cli::try_parse_from([
            "st", "ui", "--demo", "--dump", "--click", "5,8", "--pane", "home"
        ])
        .is_ok()
    );
    for flag in ["--version", "-V"] {
        let error = match Cli::try_parse_from(["st", "ui", flag]) {
            Err(error) => error, Ok(_) => panic!("version exits"),
        };
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
    }
    let error = match Cli::try_parse_from(["st", "ui", "--help"]) {
        Err(error) => error,
        Ok(_) => panic!("help exits"),
    };
    assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
    assert!(error.to_string().contains("st ui"));
    let help = cli_help::root_help(false);
    assert!(help.contains("Open the terminal interface"));
    assert!(help.contains("Run st in a terminal"));
}
