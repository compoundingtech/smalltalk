//! Plan a checkpoint on a private, previously copied store and report what it would drop.
//! Usage: cargo run --release -p st3 --example retention_plan -- CLONE DAY [SCRATCH]
//! DAY names the checkpoint's cut, such as 2026-10-10. With SCRATCH, the plan is also proved on a
//! copy written there, as a real checkpoint proves it. Never supply a live store or an immutable
//! capture: this opens and migrates CLONE.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use serde_json::json;
use smallclaims::store::checkpoint::checkpoint_cut;
use st3::store::Store;

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = PathBuf::from(args.next().context("private clone path is required")?);
    let day = args
        .next()
        .context("the checkpoint day is required")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("the day must be UTF-8"))?;
    let scratch = args.next().map(PathBuf::from);
    ensure!(args.next().is_none(), "expected CLONE DAY [SCRATCH]");
    ensure!(!path.is_symlink(), "clone must not be a symlink");
    ensure!(
        path.metadata()?.permissions().mode() & 0o777 == 0o600,
        "clone must be mode 0600"
    );
    let cut = checkpoint_cut(&day)?;
    let opened = Instant::now();
    let store = Store::open(&path, "retention-plan")?;
    let open_ms = opened.elapsed().as_millis();
    let planned = Instant::now();
    let sealed = store.checkpoint_sealed_set(cut)?;
    let plan = store.runtime.plan_checkpoint_drops(&sealed);
    let plan_ms = planned.elapsed().as_millis();
    let proof = match &scratch {
        Some(scratch) => {
            let proved = Instant::now();
            let proof = store.prove_checkpoint_plan(&sealed, &plan, scratch)?;
            Some(json!({"proof": proof, "elapsed_ms": proved.elapsed().as_millis()}))
        }
        None => None,
    };
    let by_kind = plan
        .by_kind
        .iter()
        .map(|(kind, count)| (kind.clone(), json!({"sealed": count.sealed, "dropped": count.dropped})))
        .collect::<BTreeMap<_, _>>();
    let result = json!({
        "checkpoint": day,
        "cut_unix_ms": cut,
        "rules_digest": plan.rules_digest,
        "sealed_envelopes": plan.sealed_envelopes,
        "sealed_claims": plan.sealed_claims,
        "dropped_envelopes": plan.envelopes.len(),
        "dropped_claims": plan.claims.len(),
        "drop_digest": plan.drop_digest,
        "retained_digest": plan.retained_digest,
        "by_kind": by_kind,
        "store_open_ms": open_ms,
        "plan_ms": plan_ms,
        "proof": proof,
        "release_build": !cfg!(debug_assertions),
    });
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
