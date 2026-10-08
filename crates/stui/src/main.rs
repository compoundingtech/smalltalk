use clap::Parser;

#[derive(Parser)]
#[command(name = "stui", version = stui::display_version())]
struct Cli {
    #[command(flatten)]
    options: stui::Args,
}


fn main() -> anyhow::Result<()> {
    stui::run(Cli::parse().options)
}
