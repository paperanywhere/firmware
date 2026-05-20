//! Shared state between the egui UI thread and the runtime thread.
//!
//! `SimState` holds the things the UI needs to read out of the runtime:
//! the current framebuffer (pixels the panel would be showing), a recent
//! activity log, mock battery/RSSI, and the most recent `last_applied`
//! image id. Everything is `Mutex` rather than `RwLock` because the lock
//! windows are short and contention is essentially zero (one writer thread,
//! one reader thread).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use log::Level;

use crate::config::SimConfig;

/// Cap on the in-window log buffer. Old entries roll off; the terminal still
/// has the full stream via env_logger.
const LOG_CAP: usize = 500;
const ACTIVITY_CAP: usize = 64;

/// What the panel surface currently shows, as packed RGB bytes (3 per pixel,
/// row-major top-to-bottom, left-to-right). Allocated once at startup to
/// match the configured panel dimensions. RGB rather than grayscale so the
/// same buffer can render Color7 (ACeP) panels.
///
/// `staging` accumulates the controller's frame-RAM bytes (whatever the
/// UC8179 driver writes via DTM2) until a refresh; at that point we unpack
/// them into `pixels` for egui to display.
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub staging: Vec<u8>,
    /// Bumped on every `refresh()` so the UI knows whether to upload a new
    /// texture or keep showing the cached one.
    pub generation: u64,
}

impl Framebuffer {
    fn new(width: u32, height: u32) -> Self {
        // Initialise to white — that's the panel's "no image yet" appearance.
        let pixels = vec![255u8; (width as usize) * (height as usize) * 3];
        let staging_cap = (width as usize) * (height as usize) / 8 + 64;
        Self { width, height, pixels, staging: Vec::with_capacity(staging_cap), generation: 0 }
    }
}

/// One entry in the recent-activity feed shown in the side panel.
#[derive(Debug, Clone)]
pub struct ActivityEntry {
    pub at: Instant,
    pub line: String,
}

/// One captured `log::Record` shown in the bottom log console. We snapshot
/// the level, the target (module path), and the formatted message at capture
/// time so the renderer can be a pure read.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub at: Instant,
    pub level: Level,
    pub target: String,
    pub message: String,
}

pub struct SimState {
    pub framebuffer: Mutex<Framebuffer>,
    pub activity: Mutex<VecDeque<ActivityEntry>>,
    pub logs: Mutex<VecDeque<LogEntry>>,
    pub mock_battery_mv: Mutex<u16>,
    pub mock_rssi_dbm: Mutex<i16>,
    pub last_image_id: Mutex<Option<String>>,
    pub status: Mutex<String>,
    /// Stashed at app-startup so the runtime thread can ping the UI to
    /// repaint without us having to do dirty polling.
    repaint_ctx: Mutex<Option<egui::Context>>,
}

impl SimState {
    pub fn new(config: &SimConfig) -> Self {
        Self {
            framebuffer: Mutex::new(Framebuffer::new(config.panel_width_px, config.panel_height_px)),
            activity: Mutex::new(VecDeque::with_capacity(ACTIVITY_CAP)),
            logs: Mutex::new(VecDeque::with_capacity(LOG_CAP)),
            mock_battery_mv: Mutex::new(4_100), // ~full Li-ion
            mock_rssi_dbm: Mutex::new(-55),
            last_image_id: Mutex::new(None),
            status: Mutex::new("starting".into()),
            repaint_ctx: Mutex::new(None),
        }
    }

    pub fn set_repaint_ctx(&self, ctx: egui::Context) {
        *self.repaint_ctx.lock().unwrap() = Some(ctx);
    }

    /// Asks egui to repaint as soon as it can. Cheap to over-call; egui
    /// coalesces.
    pub fn request_repaint(&self) {
        if let Some(ctx) = self.repaint_ctx.lock().unwrap().as_ref() {
            ctx.request_repaint();
        }
    }

    pub fn set_status(&self, s: impl Into<String>) {
        *self.status.lock().unwrap() = s.into();
        self.request_repaint();
    }

    pub fn push_activity(&self, line: impl Into<String>) {
        let mut q = self.activity.lock().unwrap();
        if q.len() >= ACTIVITY_CAP {
            q.pop_front();
        }
        q.push_back(ActivityEntry { at: Instant::now(), line: line.into() });
        drop(q);
        self.request_repaint();
    }

    /// Called from the `log::Log` adapter (see `crate::logger`) on every
    /// captured record. Bounded ring buffer + repaint nudge.
    pub fn push_log(&self, entry: LogEntry) {
        let mut q = self.logs.lock().unwrap();
        if q.len() >= LOG_CAP {
            q.pop_front();
        }
        q.push_back(entry);
        drop(q);
        self.request_repaint();
    }
}
