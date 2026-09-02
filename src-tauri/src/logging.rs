//! In-memory ring-buffer log collector with optional file output and
//! WebSocket broadcast for the frontend Diagnostics viewer.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use log::{Level, LevelFilter, Log, Metadata, Record};
use parking_lot::Mutex as ParkingMutex;

use crate::state::{timestamp_now, AppLogEntry, IpcEvent, LogConfig};

const DEFAULT_CAPACITY: usize = 1000;

fn level_filter_to_u8(level: LevelFilter) -> u8 {
    match level {
        LevelFilter::Off => 0,
        LevelFilter::Error => 1,
        LevelFilter::Warn => 2,
        LevelFilter::Info => 3,
        LevelFilter::Debug => 4,
        LevelFilter::Trace => 5,
    }
}

fn u8_to_level_filter(value: u8) -> LevelFilter {
    match value {
        0 => LevelFilter::Off,
        1 => LevelFilter::Error,
        2 => LevelFilter::Warn,
        3 => LevelFilter::Info,
        4 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    }
}

fn level_to_u8(level: Level) -> u8 {
    match level {
        Level::Error => 1,
        Level::Warn => 2,
        Level::Info => 3,
        Level::Debug => 4,
        Level::Trace => 5,
    }
}

fn log_dir_path() -> PathBuf {
    let mut path = dirs_next::data_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("OxideLink");
    path.push("logs");
    path
}

struct LogInner {
    max_lines: usize,
    filter: AtomicU8,
    ring: ParkingMutex<VecDeque<AppLogEntry>>,
    file: ParkingMutex<Option<BufWriter<fs::File>>>,
    tx: ParkingMutex<Option<tokio::sync::broadcast::Sender<IpcEvent>>>,
}

/// Thread-safe ring-buffer log collector implementing the `log::Log` trait.
#[derive(Clone)]
pub struct LogCollector {
    inner: std::sync::Arc<LogInner>,
}

impl LogCollector {
    /// Build a new collector from `LogConfig` and a parsed `LevelFilter`.
    pub fn new(config: &LogConfig, max_level: LevelFilter) -> Result<Self, String> {
        let capacity = if config.max_lines == 0 {
            DEFAULT_CAPACITY
        } else {
            config.max_lines
        };

        let file_writer = if config.log_file {
            let dir = log_dir_path();
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let filename = format!("oxidelink-{}.log", today);
            let path = dir.join(&filename);

            fs::create_dir_all(&dir)
                .map_err(|e| format!("failed to create log directory {:?}: {}", dir, e))?;

            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| format!("failed to open log file {:?}: {}", path, e))?;

            Some(BufWriter::new(file))
        } else {
            None
        };

        Ok(Self {
            inner: std::sync::Arc::new(LogInner {
                max_lines: capacity,
                filter: AtomicU8::new(level_filter_to_u8(max_level)),
                ring: ParkingMutex::new(VecDeque::with_capacity(capacity.min(1024))),
                file: ParkingMutex::new(file_writer),
                tx: ParkingMutex::new(None),
            }),
        })
    }

    /// Current maximum log level.
    pub fn max_level(&self) -> LevelFilter {
        u8_to_level_filter(self.inner.filter.load(Ordering::Relaxed))
    }

    /// Change the active level filter and update the `log` crate's global max.
    pub fn set_level(&self, level: &str) -> Result<(), String> {
        let max_level = level
            .parse::<LevelFilter>()
            .map_err(|e| format!("invalid log level '{}': {}", level, e))?;
        self.inner
            .filter
            .store(level_filter_to_u8(max_level), Ordering::Relaxed);
        log::set_max_level(max_level);
        Ok(())
    }

    /// Attach or detach a broadcast sender for live `LogBatch` IPC events.
    pub fn set_event_sender(&self, tx: Option<tokio::sync::broadcast::Sender<IpcEvent>>) {
        *self.inner.tx.lock() = tx;
    }

    /// Remove all buffered entries.
    pub fn clear(&self) {
        self.inner.ring.lock().clear();
    }

    /// Retrieve recent log entries, optionally filtered by level or message/target text.
    pub fn recent(
        &self,
        level: Option<String>,
        search: Option<String>,
        limit: Option<usize>,
    ) -> Vec<AppLogEntry> {
        let ring = self.inner.ring.lock();
        let level_filter = level.as_deref().map(|l| l.to_lowercase());
        let search_filter = search.as_deref().map(|s| s.to_lowercase());

        let mut out = Vec::new();
        for entry in ring.iter().rev() {
            if let Some(ref l) = level_filter {
                if entry.level.to_lowercase() != *l {
                    continue;
                }
            }
            if let Some(ref s) = search_filter {
                let hay = format!("{} {}", entry.target, entry.message).to_lowercase();
                if !hay.contains(s) {
                    continue;
                }
            }
            out.push(entry.clone());
            if let Some(lim) = limit {
                if out.len() >= lim {
                    break;
                }
            }
        }
        // Return in chronological order.
        out.reverse();
        out
    }

    fn append(&self, entry: AppLogEntry) {
        {
            let mut ring = self.inner.ring.lock();
            if ring.len() >= self.inner.max_lines {
                ring.pop_front();
            }
            ring.push_back(entry.clone());
        }

        if let Some(writer) = self.inner.file.lock().as_mut() {
            let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
            let line = format!(
                "{} [{}] {} - {}\n",
                ts,
                entry.level.to_uppercase(),
                entry.target,
                entry.message
            );
            let _ = writer.write_all(line.as_bytes());
            let _ = writer.flush();
        }

        if let Some(tx) = self.inner.tx.lock().as_ref() {
            let _ = tx.send(IpcEvent::LogBatch { logs: vec![entry] });
        }
    }
}

impl Log for LogCollector {
    fn enabled(&self, metadata: &Metadata) -> bool {
        let max_level = self.inner.filter.load(Ordering::Relaxed);
        level_to_u8(metadata.level()) <= max_level && max_level > 0
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let entry = AppLogEntry {
            timestamp: timestamp_now(),
            level: record.level().to_string().to_lowercase(),
            target: record.target().to_string(),
            message: format!("{}", record.args()),
        };

        self.append(entry);
    }

    fn flush(&self) {
        if let Some(writer) = self.inner.file.lock().as_mut() {
            let _ = writer.flush();
        }
    }
}

/// Global collector set by [`init_logging`].
pub static GLOBAL_COLLECTOR: OnceLock<LogCollector> = OnceLock::new();

static INIT_LOCK: Mutex<()> = Mutex::new(());

/// Install the `LogCollector` as the global `log` sink. Safe to call more than
/// once; subsequent calls return the already-initialized collector.
pub fn init_logging(config: &LogConfig) -> Result<LogCollector, String> {
    let _guard = INIT_LOCK.lock().expect("logging init lock poisoned");
    if let Some(c) = GLOBAL_COLLECTOR.get() {
        return Ok(c.clone());
    }

    let max_level = config
        .level
        .parse::<LevelFilter>()
        .map_err(|e| format!("invalid log level '{}': {}", config.level, e))?;

    let collector = LogCollector::new(config, max_level)?;
    let logger = collector.clone();

    log::set_boxed_logger(Box::new(logger))
        .map_err(|e| format!("failed to install global logger: {}", e))?;
    log::set_max_level(max_level);

    let _ = GLOBAL_COLLECTOR.set(collector.clone());
    Ok(collector)
}

/// Tauri command: retrieve filtered log entries from the ring buffer.
#[tauri::command]
pub fn get_logs(
    level: Option<String>,
    search: Option<String>,
    limit: Option<usize>,
) -> Vec<AppLogEntry> {
    GLOBAL_COLLECTOR
        .get()
        .map(|c| c.recent(level, search, limit))
        .unwrap_or_default()
}

/// Tauri command: clear the in-memory log ring buffer.
#[tauri::command]
pub fn clear_logs() {
    if let Some(c) = GLOBAL_COLLECTOR.get() {
        c.clear();
    }
}

/// Tauri command: change the active log level.
#[tauri::command]
pub fn set_log_level(level: String) -> Result<(), String> {
    GLOBAL_COLLECTOR
        .get()
        .map(|c| c.set_level(&level))
        .unwrap_or(Err("logger not initialized".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_config_default_and_level_filter_updates_work() {
        let config = LogConfig::default();
        assert_eq!(config.level, "info");
        assert_eq!(config.max_lines, DEFAULT_CAPACITY);

        let collector = LogCollector::new(&config, LevelFilter::Info);
        assert!(collector.is_ok());
        let collector = match collector {
            Ok(collector) => collector,
            Err(error) => panic!("failed to construct in-memory collector: {error}"),
        };
        assert_eq!(collector.max_level(), LevelFilter::Info);
        assert!(collector.set_level("debug").is_ok());
        assert_eq!(collector.max_level(), LevelFilter::Debug);
        assert!(collector.set_level("not-a-level").is_err());
    }

    #[test]
    fn default_capacity_used_when_max_lines_zero() {
        let config = LogConfig {
            max_lines: 0,
            ..LogConfig::default()
        };
        let collector = LogCollector::new(&config, LevelFilter::Info).unwrap();
        assert_eq!(collector.inner.max_lines, DEFAULT_CAPACITY);
    }

    #[test]
    fn recent_filters_by_level_and_search() {
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Debug).unwrap();

        let make = |level: &str, target: &str, msg: &str| AppLogEntry {
            timestamp: 0,
            level: level.into(),
            target: target.into(),
            message: msg.into(),
        };

        collector.append(make("info", "crate::a", "hello world"));
        collector.append(make("warn", "crate::b", "goodbye moon"));
        collector.append(make("debug", "crate::a", "debug info"));

        let info_only = collector.recent(Some("info".into()), None, None);
        assert_eq!(info_only.len(), 1);
        assert_eq!(info_only[0].message, "hello world");

        let moon = collector.recent(None, Some("moon".into()), None);
        assert_eq!(moon.len(), 1);
        assert_eq!(moon[0].level, "warn");

        // `recent` returns the newest `limit` entries in chronological order,
        // so a limit of 2 gives the last two appended entries.
        let limited = collector.recent(None, None, Some(2));
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].message, "goodbye moon");
        assert_eq!(limited[1].message, "debug info");
    }

    // -----------------------------------------------------------------
    //  Level helper round-trip tests
    // -----------------------------------------------------------------

    #[test]
    fn level_filter_to_u8_all_variants() {
        assert_eq!(level_filter_to_u8(LevelFilter::Off), 0);
        assert_eq!(level_filter_to_u8(LevelFilter::Error), 1);
        assert_eq!(level_filter_to_u8(LevelFilter::Warn), 2);
        assert_eq!(level_filter_to_u8(LevelFilter::Info), 3);
        assert_eq!(level_filter_to_u8(LevelFilter::Debug), 4);
        assert_eq!(level_filter_to_u8(LevelFilter::Trace), 5);
    }

    #[test]
    fn u8_to_level_filter_all_variants() {
        assert_eq!(u8_to_level_filter(0), LevelFilter::Off);
        assert_eq!(u8_to_level_filter(1), LevelFilter::Error);
        assert_eq!(u8_to_level_filter(2), LevelFilter::Warn);
        assert_eq!(u8_to_level_filter(3), LevelFilter::Info);
        assert_eq!(u8_to_level_filter(4), LevelFilter::Debug);
        assert_eq!(u8_to_level_filter(5), LevelFilter::Trace);
    }

    #[test]
    fn u8_to_level_filter_above_max_clamps_to_trace() {
        assert_eq!(u8_to_level_filter(255), LevelFilter::Trace);
        assert_eq!(u8_to_level_filter(6), LevelFilter::Trace);
        assert_eq!(u8_to_level_filter(100), LevelFilter::Trace);
    }

    #[test]
    fn level_to_u8_all_variants() {
        assert_eq!(level_to_u8(Level::Error), 1);
        assert_eq!(level_to_u8(Level::Warn), 2);
        assert_eq!(level_to_u8(Level::Info), 3);
        assert_eq!(level_to_u8(Level::Debug), 4);
        assert_eq!(level_to_u8(Level::Trace), 5);
    }

    #[test]
    fn level_filter_roundtrip() {
        for level in [
            LevelFilter::Off,
            LevelFilter::Error,
            LevelFilter::Warn,
            LevelFilter::Info,
            LevelFilter::Debug,
            LevelFilter::Trace,
        ] {
            let encoded = level_filter_to_u8(level);
            let decoded = u8_to_level_filter(encoded);
            assert_eq!(decoded, level);
        }
    }

    // -----------------------------------------------------------------
    //  LogConfig defaults
    // -----------------------------------------------------------------

    #[test]
    fn log_config_default_values() {
        let config = LogConfig::default();
        assert_eq!(config.level, "info");
        assert_eq!(config.max_lines, 1000);
        assert!(config.ring_buffer);
        assert!(!config.log_file);
    }

    #[test]
    fn default_capacity_constant() {
        assert_eq!(DEFAULT_CAPACITY, 1000);
    }

    // -----------------------------------------------------------------
    //  LogCollector max_level / set_level
    // -----------------------------------------------------------------

    /// Guard to serialize tests that call `set_level` (which touches the
    /// global `log::set_max_level`).
    static LEVEL_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn log_collector_max_level_reflects_config() {
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Warn).unwrap();
        assert_eq!(collector.max_level(), LevelFilter::Warn);
    }

    #[test]
    fn log_collector_set_level_valid() {
        let _guard = LEVEL_GUARD.lock().expect("level guard poisoned");
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Off).unwrap();
        assert_eq!(collector.max_level(), LevelFilter::Off);

        assert!(collector.set_level("error").is_ok());
        assert_eq!(collector.max_level(), LevelFilter::Error);

        assert!(collector.set_level("warn").is_ok());
        assert_eq!(collector.max_level(), LevelFilter::Warn);

        assert!(collector.set_level("trace").is_ok());
        assert_eq!(collector.max_level(), LevelFilter::Trace);
    }

    #[test]
    fn log_collector_set_level_case_insensitive() {
        let _guard = LEVEL_GUARD.lock().expect("level guard poisoned");
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Off).unwrap();
        assert!(collector.set_level("INFO").is_ok());
        assert_eq!(collector.max_level(), LevelFilter::Info);
        assert!(collector.set_level("Debug").is_ok());
        assert_eq!(collector.max_level(), LevelFilter::Debug);
    }

    #[test]
    fn log_collector_set_level_invalid() {
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Info).unwrap();
        assert!(collector.set_level("verbose").is_err());
        assert!(collector.set_level("").is_err());
        assert!(collector.set_level("not-a-level").is_err());
        // Level should remain unchanged after a failed set
        assert_eq!(collector.max_level(), LevelFilter::Info);
    }

    // -----------------------------------------------------------------
    //  Ring buffer operations
    // -----------------------------------------------------------------

    #[test]
    fn log_collector_clear_empties_ring() {
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Debug).unwrap();
        collector.append(AppLogEntry {
            timestamp: 0,
            level: "info".into(),
            target: "test".into(),
            message: "hello".into(),
        });
        assert_eq!(collector.recent(None, None, None).len(), 1);
        collector.clear();
        assert_eq!(collector.recent(None, None, None).len(), 0);
    }

    #[test]
    fn log_collector_ring_buffer_eviction() {
        let config = LogConfig {
            max_lines: 3,
            ..LogConfig::default()
        };
        let collector = LogCollector::new(&config, LevelFilter::Debug).unwrap();

        for i in 0..5 {
            collector.append(AppLogEntry {
                timestamp: i,
                level: "info".into(),
                target: "test".into(),
                message: format!("msg {}", i),
            });
        }

        let entries = collector.recent(None, None, None);
        assert_eq!(entries.len(), 3, "should only keep max_lines entries");
        // Oldest entries evicted; remaining are msg 2, 3, 4 in order
        assert_eq!(entries[0].message, "msg 2");
        assert_eq!(entries[1].message, "msg 3");
        assert_eq!(entries[2].message, "msg 4");
    }

    #[test]
    fn log_collector_ring_buffer_exact_capacity() {
        let config = LogConfig {
            max_lines: 3,
            ..LogConfig::default()
        };
        let collector = LogCollector::new(&config, LevelFilter::Debug).unwrap();

        for i in 0..3 {
            collector.append(AppLogEntry {
                timestamp: i,
                level: "info".into(),
                target: "test".into(),
                message: format!("msg {}", i),
            });
        }

        let entries = collector.recent(None, None, None);
        assert_eq!(entries.len(), 3, "should keep all 3 entries");
        assert_eq!(entries[0].message, "msg 0");
        assert_eq!(entries[2].message, "msg 2");
    }

    // -----------------------------------------------------------------
    //  recent() filter tests
    // -----------------------------------------------------------------

    #[test]
    fn recent_level_filter_case_insensitive() {
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Debug).unwrap();
        collector.append(AppLogEntry {
            timestamp: 0,
            level: "info".into(),
            target: "test".into(),
            message: "hello".into(),
        });
        assert_eq!(collector.recent(Some("INFO".into()), None, None).len(), 1);
        assert_eq!(collector.recent(Some("Info".into()), None, None).len(), 1);
    }

    #[test]
    fn recent_search_case_insensitive() {
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Debug).unwrap();
        collector.append(AppLogEntry {
            timestamp: 0,
            level: "info".into(),
            target: "MyModule".into(),
            message: "Hello World".into(),
        });
        assert_eq!(collector.recent(None, Some("hello".into()), None).len(), 1);
        assert_eq!(collector.recent(None, Some("WORLD".into()), None).len(), 1);
        assert_eq!(
            collector.recent(None, Some("mymodule".into()), None).len(),
            1
        );
    }

    #[test]
    fn recent_search_no_match() {
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Debug).unwrap();
        collector.append(AppLogEntry {
            timestamp: 0,
            level: "info".into(),
            target: "test".into(),
            message: "hello".into(),
        });
        assert_eq!(
            collector
                .recent(None, Some("nonexistent".into()), None)
                .len(),
            0
        );
    }

    #[test]
    fn recent_limit_one() {
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Debug).unwrap();
        collector.append(AppLogEntry {
            timestamp: 0,
            level: "info".into(),
            target: "test".into(),
            message: "first".into(),
        });
        collector.append(AppLogEntry {
            timestamp: 1,
            level: "info".into(),
            target: "test".into(),
            message: "second".into(),
        });
        // limit=1 should return only the most recent entry
        let result = collector.recent(None, None, Some(1));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].message, "second");
    }

    #[test]
    fn recent_chronological_order() {
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Debug).unwrap();
        for i in 0..3 {
            collector.append(AppLogEntry {
                timestamp: i,
                level: "info".into(),
                target: "test".into(),
                message: format!("msg {}", i),
            });
        }
        let result = collector.recent(None, None, None);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].message, "msg 0");
        assert_eq!(result[1].message, "msg 1");
        assert_eq!(result[2].message, "msg 2");
    }

    #[test]
    fn recent_combined_level_and_search() {
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Debug).unwrap();
        collector.append(AppLogEntry {
            timestamp: 0,
            level: "info".into(),
            target: "mod_a".into(),
            message: "hello".into(),
        });
        collector.append(AppLogEntry {
            timestamp: 1,
            level: "warn".into(),
            target: "mod_b".into(),
            message: "hello".into(),
        });
        // Both match "hello" but only one is "warn"
        let result = collector.recent(Some("warn".into()), Some("hello".into()), None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].level, "warn");
    }

    #[test]
    fn recent_empty_ring() {
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Debug).unwrap();
        assert_eq!(collector.recent(None, None, None).len(), 0);
    }

    // -----------------------------------------------------------------
    //  set_event_sender / clone
    // -----------------------------------------------------------------

    #[test]
    fn log_collector_set_event_sender_none_ok() {
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Info).unwrap();
        collector.set_event_sender(None);
        // Should not panic
    }

    #[test]
    fn log_collector_clone_shares_state() {
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Debug).unwrap();
        let cloned = collector.clone();
        collector.append(AppLogEntry {
            timestamp: 0,
            level: "info".into(),
            target: "test".into(),
            message: "shared".into(),
        });
        // Clone should see the same entry (Arc-based inner)
        let result = cloned.recent(None, None, None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].message, "shared");
    }

    // -----------------------------------------------------------------
    //  Log trait enabled()
    // -----------------------------------------------------------------

    #[test]
    fn log_collector_enabled_respects_filter() {
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Warn).unwrap();
        // Warn filter: Error and Warn enabled, Info/Debug/Trace disabled
        let md_err = log::Metadata::builder().level(Level::Error).build();
        assert!(collector.enabled(&md_err));
        let md_warn = log::Metadata::builder().level(Level::Warn).build();
        assert!(collector.enabled(&md_warn));
        let md_info = log::Metadata::builder().level(Level::Info).build();
        assert!(!collector.enabled(&md_info));
        let md_debug = log::Metadata::builder().level(Level::Debug).build();
        assert!(!collector.enabled(&md_debug));
    }

    #[test]
    fn log_collector_enabled_off_filter() {
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Off).unwrap();
        let md = log::Metadata::builder().level(Level::Error).build();
        assert!(!collector.enabled(&md), "Off filter should disable all");
    }

    #[test]
    fn log_collector_enabled_trace_filter() {
        let config = LogConfig::default();
        let collector = LogCollector::new(&config, LevelFilter::Trace).unwrap();
        let md = log::Metadata::builder().level(Level::Trace).build();
        assert!(collector.enabled(&md));
        let md = log::Metadata::builder().level(Level::Error).build();
        assert!(collector.enabled(&md));
    }
}
