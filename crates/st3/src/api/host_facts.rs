//! `host-facts.read`: what this member's own host reports now. Sampled on each read from the
//! kernel and never written to the graph, so a fleet of pollers adds no claims or replication.

use super::*;
use st3_client::{HostFacts, HostLoad};

pub(super) async fn read(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
) -> Result<Json<HostFacts>, ApiError> {
    client_v0::require_scope(&session, "read.projections")?;
    let observed_at = client_timestamp(client_now_ms());
    let load = read_deadline::spawn_blocking(sample_load)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(HostFacts {
        host_id: client_host_id(&state.node),
        observed_at,
        load,
    }))
}

fn sample_load() -> HostLoad {
    let cpus = std::thread::available_parallelism().map_or(1, |cpus| cpus.get() as u32);
    match std::fs::read_to_string("/proc/loadavg") {
        Ok(text) => parse_load(&text, cpus),
        Err(error) => HostLoad::Unknown { reason: format!("/proc/loadavg: {error}") },
    }
}

fn parse_load(text: &str, cpus: u32) -> HostLoad {
    match text.split_whitespace().next().map(str::parse::<f64>) {
        Some(Ok(one_minute)) if one_minute.is_finite() && one_minute >= 0.0 => {
            HostLoad::Reported { one_minute, cpus }
        }
        _ => HostLoad::Unknown { reason: "/proc/loadavg is malformed".into() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_loadavg_field_is_the_one_minute_load() {
        assert_eq!(
            parse_load("0.42 0.30 0.21 2/913 12345\n", 8),
            HostLoad::Reported { one_minute: 0.42, cpus: 8 }
        );
        assert_eq!(
            parse_load("", 8),
            HostLoad::Unknown { reason: "/proc/loadavg is malformed".into() }
        );
    }
}
