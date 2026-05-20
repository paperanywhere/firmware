//! `log::Log` adapter that fans every captured record out to two sinks:
//!
//!   1. `env_logger`'s pretty stderr printer (so `RUST_LOG=info cargo run`
//!      still shows the full stream in the terminal where you launched it).
//!   2. A bounded ring buffer in [`crate::state::SimState`] which the egui
//!      bottom panel renders as an in-window log console.
//!
//! `log::set_logger` only accepts a `&'static dyn Log`, so we leak a single
//! instance and attach `SimState` to it lazily after main has built it. Any
//! records emitted before `attach` lands still go to stderr — they just
//! don't show in the UI (the UI doesn't exist yet anyway).

use std::sync::{Arc, OnceLock};
use std::time::Instant;

use log::{Log, Metadata, Record};

use crate::state::{LogEntry, SimState};

pub struct SimLogger {
    state: OnceLock<Arc<SimState>>,
    inner: env_logger::Logger,
}

impl Log for SimLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if !self.inner.enabled(record.metadata()) {
            return;
        }
        // Fan out 1: terminal via env_logger.
        self.inner.log(record);
        // Fan out 2: SimState ring buffer (when attached).
        if let Some(state) = self.state.get() {
            state.push_log(LogEntry {
                at: Instant::now(),
                level: record.level(),
                target: record.target().to_string(),
                message: record.args().to_string(),
            });
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// Handle returned from `init` so main can plug `SimState` in after it's
/// built. Doesn't carry the logger itself — the global one is already
/// registered.
pub struct SimLoggerHandle(&'static SimLogger);

impl SimLoggerHandle {
    pub fn attach(self, state: Arc<SimState>) {
        // Setter is idempotent in the failure direction (set returns Err if
        // already set); we never call it twice in practice so silently
        // accept either outcome.
        let _ = self.0.state.set(state);
    }
}

/// Install our adapter as `log`'s global sink. Has to happen before any
/// `log::*!` macro emits, so call this first thing in main.
pub fn init() -> SimLoggerHandle {
    let env = env_logger::Env::default().default_filter_or("info");
    let inner = env_logger::Builder::from_env(env).build();
    let level = inner.filter();
    let sim_logger: &'static SimLogger = Box::leak(Box::new(SimLogger {
        state: OnceLock::new(),
        inner,
    }));
    log::set_logger(sim_logger).expect("log::set_logger");
    log::set_max_level(level);
    SimLoggerHandle(sim_logger)
}
