//! Config loading. Looks for `paperanywhere-sim.json` in the user's local
//! data dir; writes a default scaffold there on first run so the user has a
//! file to edit instead of CLI flags.

use std::fs;
use std::path::PathBuf;

use directories::ProjectDirs;
use log::warn;
use paperanywhere_ports::ColorMode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimConfig {
    /// Backend root, e.g. `http://localhost:8080`. The runtime appends
    /// `/api/device/state` etc. directly to this.
    pub backend_url: String,
    /// Bearer token issued by `/api/device/claim` on a real device. To
    /// simulate without claiming, insert a row directly into `device_tokens`
    /// in the backend's DB and paste the raw token here.
    pub device_token: String,
    /// Width of the simulated panel in pixels. Default matches reTerminal
    /// E1001 (7.5" 800×480 mono).
    pub panel_width_px: u32,
    pub panel_height_px: u32,
    /// One of: `mono_1bpp` (default), `gray_4`, `gray_16`, `color_7`. Drives
    /// the unpack path in `crate::panel::VirtualPanel`. Strings match the
    /// backend's color-mode enum so the same name flows everywhere.
    pub color_mode: String,
    /// Where to keep `last_applied_image_id` between runs.
    pub nvs_path: PathBuf,
}

impl SimConfig {
    pub fn load_or_init() -> Self {
        let path = config_file_path();
        let mut cfg = match fs::read_to_string(&path) {
            Ok(body) => match serde_json::from_str::<SimConfig>(&body) {
                Ok(cfg) => cfg,
                Err(e) => {
                    warn!("sim config {:?} parse error: {} — using defaults", path, e);
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Self::default();
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let body = serde_json::to_string_pretty(&cfg).expect("serialize default config");
                if let Err(e) = fs::write(&path, body) {
                    warn!("could not write default config to {:?}: {}", path, e);
                } else {
                    log::info!("wrote default sim config to {:?}", path);
                }
                cfg
            }
            Err(e) => {
                warn!("sim config {:?} read error: {} — using defaults", path, e);
                Self::default()
            }
        };

        // ── Env var overrides ──
        //
        // Lets a `docker-compose up` flow target the running backend without
        // editing the JSON file:
        //
        //   PAPERANYWHERE_SIM_BACKEND_URL=http://localhost:8080 \
        //   PAPERANYWHERE_SIM_DEVICE_TOKEN=<paste from db> \
        //   cargo run -p paperanywhere-sim
        //
        // Env always wins over file because shell ergonomics > file editing
        // for transient redirects (a different compose project, a remote
        // staging backend, etc.).
        if let Ok(v) = std::env::var("PAPERANYWHERE_SIM_BACKEND_URL") {
            log::info!("config: backend_url overridden by env");
            cfg.backend_url = v;
        }
        if let Ok(v) = std::env::var("PAPERANYWHERE_SIM_DEVICE_TOKEN") {
            log::info!("config: device_token overridden by env");
            cfg.device_token = v;
        }
        if let Ok(v) = std::env::var("PAPERANYWHERE_SIM_PANEL_WIDTH") {
            if let Ok(n) = v.parse() {
                cfg.panel_width_px = n;
            }
        }
        if let Ok(v) = std::env::var("PAPERANYWHERE_SIM_PANEL_HEIGHT") {
            if let Ok(n) = v.parse() {
                cfg.panel_height_px = n;
            }
        }
        if let Ok(v) = std::env::var("PAPERANYWHERE_SIM_COLOR_MODE") {
            cfg.color_mode = v;
        }

        cfg
    }

    /// Resolve the `color_mode` string in the config into the enum the
    /// panel unpacker uses. Unknown strings log a warning and fall back to
    /// Mono1bpp so the sim still renders something.
    pub fn parsed_color_mode(&self) -> ColorMode {
        match self.color_mode.as_str() {
            "mono_1bpp" => ColorMode::Mono1bpp,
            "mono_red_1bpp" => ColorMode::MonoRed1bpp,
            "mono_yellow_1bpp" => ColorMode::MonoYellow1bpp,
            "gray_4" => ColorMode::Gray4,
            "gray_16" => ColorMode::Gray16,
            "color_7" => ColorMode::Color7,
            other => {
                warn!("unknown color_mode '{}' — defaulting to mono_1bpp", other);
                ColorMode::Mono1bpp
            }
        }
    }

    pub fn window_width(&self) -> f32 {
        // Side panel + the rendered framebuffer at 1:1, capped so we don't
        // open a 4000px-wide window on a 1080p screen.
        let side = 320.0_f32;
        let panel_w = (self.panel_width_px as f32).min(1280.0);
        side + panel_w + 32.0
    }

    pub fn window_height(&self) -> f32 {
        ((self.panel_height_px as f32) + 80.0).min(900.0).max(480.0)
    }
}

impl Default for SimConfig {
    fn default() -> Self {
        let dirs = ProjectDirs::from("io", "paperanywhere", "paperanywhere-sim");
        let nvs_path = dirs
            .as_ref()
            .map(|d| d.data_dir().join("sim_nvs.json"))
            .unwrap_or_else(|| PathBuf::from("paperanywhere-sim.nvs.json"));
        Self {
            backend_url: "http://localhost:8080".into(),
            device_token: "REPLACE_ME_WITH_REAL_TOKEN".into(),
            panel_width_px: 800,
            panel_height_px: 480,
            color_mode: "mono_1bpp".into(),
            nvs_path,
        }
    }
}

fn config_file_path() -> PathBuf {
    // Explicit override wins — useful for keeping per-project sim configs
    // alongside a docker-compose checkout.
    if let Ok(p) = std::env::var("PAPERANYWHERE_SIM_CONFIG") {
        return PathBuf::from(p);
    }
    if let Some(dirs) = ProjectDirs::from("io", "paperanywhere", "paperanywhere-sim") {
        let dir = dirs.config_dir();
        return dir.join("paperanywhere-sim.json");
    }
    PathBuf::from("paperanywhere-sim.json")
}
