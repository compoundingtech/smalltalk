//! Exercise the optional setup-shell entry point against an isolated local daemon.
//! The argument is staged for review; only the person pressing Enter executes it.
fn main() -> anyhow::Result<()> {
    let command = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Pass one command to stage"))?;
    let mut args = stui::Args::default().with_setup_command(command)?;
    args.local = true;
    stui::run(args)
}
