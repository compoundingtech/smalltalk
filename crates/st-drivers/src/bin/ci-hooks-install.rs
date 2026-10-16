//! Install the matching rendered hooks for CI without the legacy runner CLI.

fn main() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::args_os().len() == 1,
        "ci-hooks-install takes no arguments"
    );
    let dir = st_drivers::hooks::install(false)?;
    let root = st_drivers::hooks::hooks_root()?;
    println!(
        "installed hook set {} in {}\nreceipt {}",
        st_drivers::hooks::hookset_id(),
        dir.display(),
        root.join("current.json").display()
    );
    Ok(())
}
