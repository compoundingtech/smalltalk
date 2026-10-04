//! The wrapper shares the provider's PTY: its diagnostics must use a private file.
use super::*;
use std::sync::Mutex;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, Registry};

const WARNING_INTERVAL: Duration = Duration::from_secs(60);

struct DriverLog {
    state: Mutex<LogState>,
}

struct LogState {
    file: Option<File>,
    last_warning: BTreeMap<(&'static str, u32), Instant>,
}

#[derive(Default)]
struct Fields(serde_json::Map<String, Value>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .insert(field.name().into(), json!(format!("{value:?}")));
    }
}

impl Layer<Registry> for DriverLog {
    fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, Registry>) {
        let metadata = event.metadata();
        if *metadata.level() > tracing::Level::WARN {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let site = (
            metadata.file().unwrap_or(metadata.target()),
            metadata.line().unwrap_or(0),
        );
        let now = Instant::now();
        if state
            .last_warning
            .get(&site)
            .is_some_and(|last| now.duration_since(*last) < WARNING_INTERVAL)
        {
            return;
        }
        state.last_warning.insert(site, now);
        let Some(file) = state.file.as_mut() else {
            return;
        };
        let mut fields = Fields::default();
        event.record(&mut fields);
        // Logging is best-effort. Even an unwritable log must neither stop delivery nor fall
        // back to the inherited stderr, which is the harness terminal.
        let _ = writeln!(
            file,
            "{}",
            json!({
                "unixMs": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),
                "level": metadata.level().as_str(),
                "target": metadata.target(),
                "fields": fields.0,
            })
        );
    }
}

pub(super) fn install(state_dir: &Path) -> tracing::dispatcher::DefaultGuard {
    let file = (|| -> std::io::Result<File> {
        fs::create_dir_all(state_dir)?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(state_dir.join("driver.log"))
    })()
    .ok();
    tracing::subscriber::set_default(tracing_subscriber::registry().with(DriverLog {
        state: Mutex::new(LogState {
            file,
            last_warning: BTreeMap::new(),
        }),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct InheritedOutput(Arc<AtomicUsize>);
    impl Layer<Registry> for InheritedOutput {
        fn on_event(&self, _event: &tracing::Event<'_>, _context: Context<'_, Registry>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn driver_warnings_are_private_and_rate_limited_even_if_the_log_cannot_open() {
        let tmp = tempfile::tempdir().unwrap();
        let inherited = Arc::new(AtomicUsize::new(0));
        let _inherited = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(InheritedOutput(inherited.clone())),
        );
        {
            let _log = install(tmp.path());
            for _ in 0..10 {
                tracing::warn!("bounded transcript discovery failed");
            }
            tracing::warn!("a distinct warning site");
        }
        let lines = fs::read_to_string(tmp.path().join("driver.log")).unwrap();
        assert_eq!(lines.lines().count(), 2);
        assert!(lines.contains("bounded transcript discovery failed"));
        let permissions = fs::metadata(tmp.path().join("driver.log"))
            .unwrap()
            .permissions();
        assert_eq!(permissions.mode() & 0o777, 0o600);
        let blocked = tmp.path().join("not-a-directory");
        fs::write(&blocked, "occupied").unwrap();
        {
            let _log = install(&blocked);
            tracing::warn!("logging must never fall back to the PTY");
        }
        assert_eq!(inherited.load(Ordering::SeqCst), 0);
    }
}
