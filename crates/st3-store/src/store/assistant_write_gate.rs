//! Test-support scheduling gate: delay a real mutation before it acquires the writer.
use std::{path::{Path, PathBuf}, sync::OnceLock, time::{Duration, Instant}};
use crate::model::St3Error;
struct Gate {
    root: PathBuf,
    grant_subject: Option<String>,
}
static GATE: OnceLock<Gate> = OnceLock::new();

pub fn install(root: &Path, grant_subject: Option<String>) -> Result<(), St3Error> {
    std::fs::create_dir_all(root).map_err(super::internal)?;
    GATE.set(Gate { root: root.to_owned(), grant_subject })
        .map_err(|_| St3Error::new("internal", "mutation gate already installed"))
}

pub struct Completion(&'static Path);
impl Drop for Completion {
    fn drop(&mut self) { let _ = std::fs::write(self.0.join("done"), b"done"); }
}

pub fn pause(grant_subject: &str) -> Result<Option<Completion>, St3Error> {
    let Some(gate) = GATE.get().filter(|gate|
        gate.root.join("hold").exists()
            && gate.grant_subject.as_deref().is_none_or(|target| target == grant_subject)
    ) else { return Ok(None) };
    let root = gate.root.as_path();
    let completion = Completion(root);
    std::fs::write(root.join("entered"), b"entered").map_err(super::internal)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !root.join("release").exists() {
        if Instant::now() >= deadline { return Err(St3Error::new("unavailable", "test mutation gate release deadline exceeded")); }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(Some(completion))
}
