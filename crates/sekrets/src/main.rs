use clap::Parser;

#[derive(Parser)]
#[command(
    name = "sekrets",
    version,
    about = "Run any CLI with credentials no seat can read"
)]
struct Cli {
    #[arg(long, global = true)]
    json: bool,
    #[command(flatten)]
    args: sekrets::cli::SekretsArgs,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let code = match sekrets::cli::run(cli.args, cli.json) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("sekrets: {error:#}");
            1
        }
    };
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    std::process::ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(1))
}
