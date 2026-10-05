//! Run on a private sqlite3 backup and a copied bound native transcript only.
use std::path::PathBuf;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    anyhow::ensure!(args.len() == 4, "usage: ivm_bakeoff ROOT AGENT COPIED_TRANSCRIPT RECEIPT_JSON");
    let receipt = st3::api::ivm_bakeoff::run(PathBuf::from(&args[0]), args[1].clone(), PathBuf::from(&args[2]))?;
    std::fs::write(&args[3], serde_json::to_vec_pretty(&receipt)?)?;
    println!("{}", serde_json::to_string_pretty(&receipt)?);
    Ok(())
}
