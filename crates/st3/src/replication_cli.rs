use super::*;
// The same print macros as main.rs: CLI output goes through st3's writer.
use st3::{cli_print as print, cli_println as println};

pub(super) async fn run_replication(
    client: &Client,
    config: &Config,
    command: ReplicationCommand,
    json_output: bool,
) -> Result<()> {
    match command {
        ReplicationCommand::Status => {
            let status: ReplicationStatus = client.get("/v1/replication/status").await?;
            if json_output {
                return print_value(&status, true);
            }
            println!(
                "fleet\t{}",
                status.fleet_id.as_deref().unwrap_or("local-only")
            );
            if let Some(removed) = &status.removed {
                println!("removed\t{}", removed.describe(status.fleet_id.as_deref()));
            }
            println!("authority-digest\t{}", status.authority_digest);
            println!("graph-digest\t{}", status.graph_digest);
            for (table, digest) in &status.projection_digests {
                println!("table-digest\t{table}\t{digest}");
            }
            println!("envelopes\t{}", status.received_envelopes);
            println!(
                "records\tvalid={} pending={} waiting={} invalid={} repaired={} checkpointed={}",
                status.valid_records,
                status.pending_records,
                status.waiting_claims,
                status.invalid_records,
                status.repaired_records,
                status.checkpointed_envelopes
            );
            println!("unhealthy-projections\t{}", status.unhealthy_projections);
            if let Some(first) = &status.first_sync {
                println!("first-sync\t{}", render_first_sync(first, now_ms()));
            }
            for projection in &status.unhealthy {
                println!(
                    "unhealthy\t{}\t{}\t{}",
                    projection.aggregate,
                    projection.error_code.as_deref().unwrap_or(""),
                    projection.error_message.as_deref().unwrap_or("")
                );
            }
            let timings = &status.timings;
            println!(
                "timings\t{} exchanges, {} envelopes received; ms: round-trip={} export={} snapshot={} receipt={} admission={} (verify={}) projection={} repair={} signing={} sqlite={} ({} commits, {} ms)",
                timings.exchanges,
                timings.envelopes_received,
                timings.round_trip_ms,
                timings.export_ms,
                timings.snapshot_ms,
                timings.receipt_ms,
                timings.admission_ms,
                timings.verify_ms,
                timings.projection_ms,
                timings.repair_ms,
                timings.signing_ms,
                timings.sqlite_ms,
                timings.commits,
                timings.commit_ms
            );
            print!(
                "{}",
                render_replication_peers(&status.peers, &status.graph_digest, now_ms())
            );
            Ok(())
        }
        ReplicationCommand::Invalid { all } => {
            let records: Vec<ReplicaRecordView> = client
                .get(&format!(
                    "/v1/replication/records?unresolved={}",
                    if all { "false" } else { "true" }
                ))
                .await?;
            if json_output {
                return print_value(&records, true);
            }
            if records.is_empty() {
                println!(
                    "No {} replication records.",
                    if all { "stored" } else { "unresolved" }
                );
                return Ok(());
            }
            for record in records {
                println!(
                    "{}\t{}\t{}:{}\t{}\t{}",
                    record.state,
                    record.record_ref,
                    record.writer,
                    record.sequence,
                    record.subject.as_deref().unwrap_or("unknown-subject"),
                    record.error_message.as_deref().unwrap_or("")
                );
            }
            Ok(())
        }
        ReplicationCommand::Inspect { record } => {
            let record: ReplicaRecordView = client
                .get(&format!(
                    "/v1/replication/records/{}",
                    urlencoding::encode(record.strip_prefix("record/").unwrap_or(&record))
                ))
                .await?;
            if json_output {
                return print_value(&record, true);
            }
            println!("record\t{}", record.record_ref);
            println!("state\t{}", record.state);
            println!("writer\t{}", record.writer);
            println!("sequence\t{}", record.sequence);
            println!("envelope\t{}", record.envelope_hash);
            println!("position\t{}", record.position);
            println!("claim\t{}", record.claim_id.as_deref().unwrap_or(""));
            println!("subject\t{}", record.subject.as_deref().unwrap_or(""));
            println!("kind\t{}", record.kind.as_deref().unwrap_or(""));
            println!("error-code\t{}", record.error_code.as_deref().unwrap_or(""));
            println!("error\t{}", record.error_message.as_deref().unwrap_or(""));
            println!(
                "replacement\t{}",
                record.replacement_claim_id.as_deref().unwrap_or("")
            );
            Ok(())
        }
        ReplicationCommand::Diff { peer } => {
            let status: ReplicationStatus = client.get("/v1/replication/status").await?;
            let remote = status
                .peers
                .iter()
                .find(|item| item.peer == peer)
                .with_context(|| format!("peer `{peer}` is not configured"))?;
            let value = json!({
                "peer": peer,
                "status": remote.status,
                "authority": {
                    "local": status.authority_digest,
                    "remote": remote.authority_digest,
                    "equal": remote.authority_digest.as_deref() == Some(status.authority_digest.as_str()),
                },
                "differing_tables": remote.differing_tables,
                "graph": {
                    "local": status.graph_digest,
                    "remote": remote.graph_digest,
                    "waiting_reason": remote.projection_comparison_waiting_reason,
                    "equal": if remote.projection_digests.is_empty() || remote.projection_comparison_waiting { None } else { Some(peer_graphs_equal(remote, &status.graph_digest)) },
                    "compared_at_unix_ms": remote.projection_compared_at_unix_ms,
                    "coverage": if remote.projection_digests.is_empty() { "legacy-only" } else if remote.projection_comparison_waiting { "pending" } else { "all-shared-projections" },
                },
            });
            if json_output {
                return print_value(&value, true);
            }
            println!("peer\t{}\t{}", peer, remote.status);
            if !remote.differing_tables.is_empty() {
                println!("different-tables\t{}", remote.differing_tables.join(", "));
            }
            println!(
                "authority\t{}\t{}\t{}",
                if remote.authority_digest.as_deref() == Some(status.authority_digest.as_str()) {
                    "equal"
                } else {
                    "different"
                },
                status.authority_digest,
                remote.authority_digest.as_deref().unwrap_or("unknown")
            );
            println!(
                "graph\t{}\t{}\t{}",
                if remote.projection_digests.is_empty() {
                    "unverified (legacy peer)"
                } else if remote.projection_comparison_waiting {
                    "pending comparison"
                } else if peer_graphs_equal(remote, &status.graph_digest) {
                    "equal"
                } else {
                    "different"
                },
                status.graph_digest,
                remote.graph_digest.as_deref().unwrap_or("unknown")
            );
            Ok(())
        }
        ReplicationCommand::Repair {
            record,
            replacement_claim,
            reason,
            actor,
            idempotency_key,
        } => {
            let record_ref = if record.starts_with("record/") {
                record
            } else {
                format!("record/{record}")
            };
            let idempotency_key = idempotency_key.unwrap_or_else(|| {
                hex::encode(Sha256::digest(
                    format!("repair\0{record_ref}\0{replacement_claim}\0{reason}\0{actor}")
                        .as_bytes(),
                ))
            });
            let request = ReplicationRepairRequest {
                record_ref,
                replacement_claim_id: replacement_claim,
                reason,
                actor,
                idempotency_key,
            };
            let claim: ClaimRecord = client.post("/v1/replication/repair", &request).await?;
            if json_output {
                print_value(&claim, true)
            } else {
                println!("repaired\t{}", claim.id);
                Ok(())
            }
        }
        ReplicationCommand::Checkpoint {
            command: CheckpointCommand::Plan { cut },
        } => {
            let request = st3::store::CheckpointPlanRequest { day: cut };
            let plan: st3::store::CheckpointPlanView =
                client.post("/v1/checkpoint/plan", &request).await?;
            if json_output {
                return print_value(&plan, true);
            }
            print!("{}", render_checkpoint_plan(&plan));
            Ok(())
        }
        ReplicationCommand::Checkpoint {
            command: CheckpointCommand::Status,
        } => {
            let status: st3::store::CheckpointStatusView =
                client.get("/v1/checkpoint/status").await?;
            if json_output {
                return print_value(&status, true);
            }
            print!("{}", render_checkpoint_status(&status));
            Ok(())
        }
        ReplicationCommand::Checkpoint {
            command:
                CheckpointCommand::Excuse {
                    writer,
                    reason,
                    actor,
                },
        } => {
            let actor = fleet_person(actor, config)?;
            let request = st3::store::CheckpointExcuseRequest {
                writer,
                reason,
                actor,
            };
            let claim: st3::model::ClaimRecord =
                client.post("/v1/checkpoint/excuse", &request).await?;
            print_value(&claim, json_output)
        }
        ReplicationCommand::Checkpoint {
            command: CheckpointCommand::Resume { reason, actor },
        } => {
            let actor = fleet_person(actor, config)?;
            let request = st3::store::CheckpointResumeRequest { reason, actor };
            let status: st3::store::CheckpointStatusView =
                client.post("/v1/checkpoint/resume", &request).await?;
            if json_output {
                return print_value(&status, true);
            }
            print!("{}", render_checkpoint_status(&status));
            Ok(())
        }
    }
}

fn render_checkpoint_plan(plan: &st3::store::CheckpointPlanView) -> String {
    let mut output = String::new();
    let percent = |part: usize, whole: usize| {
        if whole == 0 {
            0.0
        } else {
            part as f64 * 100.0 / whole as f64
        }
    };
    output.push_str(&format!(
        "CHECKPOINT  {} · dry run, nothing changed\n",
        plan.checkpoint
    ));
    output.push_str(&format!(
        "before the cut  {} envelopes · {} claims\n",
        plan.sealed_envelopes, plan.sealed_claims
    ));
    output.push_str(&format!(
        "would drop      {} envelopes · {} claims ({:.1}%)\n",
        plan.dropped_envelopes,
        plan.dropped_claims,
        percent(plan.dropped_claims, plan.sealed_claims)
    ));
    for (kind, count) in &plan.by_kind {
        output.push_str(&format!(
            "  {kind}  {} of {}\n",
            count.dropped, count.sealed
        ));
    }
    let proof = &plan.proof;
    if proof.passed {
        output.push_str(&format!(
            "proof           passed · graph and {} subjects' readers unchanged\n",
            proof.subjects
        ));
    } else {
        output.push_str(&format!(
            "proof           FAILED · a checkpoint would not verify: {}\n",
            proof.mismatches.join(", ")
        ));
    }
    output.push_str(&format!("rules-digest    {}\n", plan.rules_digest));
    output.push_str(&format!("sealed-digest   {}\n", plan.sealed_digest));
    output.push_str(&format!("drop-digest     {}\n", plan.drop_digest));
    output.push_str(&format!("graph-digest    {}\n", proof.graph_digest));
    output
}
