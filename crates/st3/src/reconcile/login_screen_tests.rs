//! The login-prompt reading of a Claude seat's screen against output the seat merely displays.

/// The seat ran a command whose output is shown under the tool call, and one line of that output
/// is another process's login prompt. The seat itself is logged in and still working; only a
/// prompt Claude shows for its own session may fence the seat as unauthenticated.
#[test]
#[ignore = "fails on main: a tool-output line that starts with the login prompt reads as the seat's own login prompt"]
fn a_login_prompt_line_inside_tool_output_is_not_the_seats_login_prompt() {
    let screen = "\
● I'll check whether the nested review CLI in the sandbox is signed in.

● Bash(sandbox/bin/review --status)
  ⎿  review 2.4.1
     Login expired · Please run /login
     exit status 1

● The sandboxed review CLI is signed out, so I'll skip it and keep going.

✻ Working… (esc to interrupt)
";
    assert_eq!(
        super::claude_login_expired(screen),
        None,
        "a displayed tool result was read as the seat's own login prompt"
    );
}
