//! egui App impl — renders the virtual e-paper panel + a side panel of
//! mock telemetry (status, battery, RSSI, last image, recent activity).
//!
//! The framebuffer is read each frame and uploaded to a GPU texture only
//! when its generation counter changes — keeps idle CPU near zero.

use std::sync::Arc;
use std::time::Instant;

use egui::{Align, Color32, ColorImage, Layout, RichText, TextureHandle, TextureOptions};
use log::Level;

use crate::state::SimState;

pub struct SimApp {
    state: Arc<SimState>,
    panel_texture: Option<TextureHandle>,
    last_texture_generation: u64,
}

impl SimApp {
    pub fn new(state: Arc<SimState>) -> Self {
        Self { state, panel_texture: None, last_texture_generation: 0 }
    }

    fn ensure_texture(&mut self, ctx: &egui::Context) {
        let fb = self.state.framebuffer.lock().unwrap();
        if self.panel_texture.is_some() && fb.generation == self.last_texture_generation {
            return;
        }
        let width = fb.width as usize;
        let height = fb.height as usize;
        let mut pixels = Vec::with_capacity(width * height);
        for rgb in fb.pixels.chunks_exact(3) {
            pixels.push(Color32::from_rgb(rgb[0], rgb[1], rgb[2]));
        }
        let image = ColorImage { size: [width, height], pixels };
        let generation = fb.generation;
        drop(fb);
        match self.panel_texture.as_mut() {
            Some(handle) => handle.set(image, TextureOptions::NEAREST),
            None => {
                self.panel_texture = Some(ctx.load_texture("panel", image, TextureOptions::NEAREST));
            }
        }
        self.last_texture_generation = generation;
    }
}

impl eframe::App for SimApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ensure_texture(ctx);

        egui::SidePanel::left("telemetry")
            .resizable(false)
            .min_width(300.0)
            .max_width(320.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.heading("paperanywhere-sim");
                ui.add_space(8.0);

                let status = self.state.status.lock().unwrap().clone();
                ui.label(RichText::new("Status").strong());
                ui.label(status);
                ui.add_space(12.0);

                ui.label(RichText::new("Mock telemetry").strong());
                let battery = *self.state.mock_battery_mv.lock().unwrap();
                let rssi = *self.state.mock_rssi_dbm.lock().unwrap();
                ui.label(format!("battery: {} mV", battery));
                ui.label(format!("rssi:    {} dBm", rssi));
                ui.add_space(12.0);

                ui.label(RichText::new("Last applied image").strong());
                match self.state.last_image_id.lock().unwrap().as_ref() {
                    Some(id) => ui.label(RichText::new(id).monospace()),
                    None => ui.label(RichText::new("(none yet)").italics()),
                };
                ui.add_space(16.0);

                ui.separator();
                ui.add_space(8.0);
                ui.label(RichText::new("Activity").strong());
                let activity = self.state.activity.lock().unwrap();
                let now = Instant::now();
                egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                    for entry in activity.iter().rev() {
                        let age = now.saturating_duration_since(entry.at).as_secs();
                        ui.with_layout(Layout::left_to_right(Align::TOP), |ui| {
                            ui.label(
                                RichText::new(format!("{:>3}s", age))
                                    .monospace()
                                    .color(Color32::from_gray(140)),
                            );
                            ui.label(&entry.line);
                        });
                    }
                });
            });

        egui::TopBottomPanel::bottom("logs")
            .resizable(true)
            .min_height(120.0)
            .default_height(220.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Logs").strong());
                    ui.label(
                        RichText::new("(RUST_LOG controls level; ring buffer caps at 500)")
                            .color(Color32::from_gray(130))
                            .small(),
                    );
                });
                ui.separator();
                let logs = self.state.logs.lock().unwrap();
                let now = Instant::now();
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        for entry in logs.iter() {
                            render_log_line(ui, entry, now);
                        }
                    });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);
            ui.label(RichText::new("Virtual e-paper panel").strong());
            ui.add_space(8.0);
            if let Some(tex) = self.panel_texture.as_ref() {
                let size = tex.size_vec2();
                ui.add(egui::Image::from_texture(tex).fit_to_exact_size(size));
            } else {
                ui.label("(no panel yet)");
            }
        });

        // Keep the activity feed's "n seconds ago" labels fresh.
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    }
}

fn render_log_line(ui: &mut egui::Ui, entry: &crate::state::LogEntry, now: Instant) {
    let (level_text, level_color) = match entry.level {
        Level::Error => ("ERR", Color32::from_rgb(230, 80, 80)),
        Level::Warn => ("WRN", Color32::from_rgb(230, 180, 60)),
        Level::Info => ("INF", Color32::from_gray(220)),
        Level::Debug => ("DBG", Color32::from_gray(160)),
        Level::Trace => ("TRC", Color32::from_gray(120)),
    };
    // Short module slug — keep just the last component so the panel doesn't
    // get dominated by `paperanywhere_runtime::lib`-style prefixes.
    let target_short = entry.target.rsplit("::").next().unwrap_or(&entry.target);
    let age = now.saturating_duration_since(entry.at).as_secs();
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!("{:>4}s", age))
                .color(Color32::from_gray(110))
                .monospace(),
        );
        ui.label(
            RichText::new(level_text)
                .color(level_color)
                .monospace()
                .strong(),
        );
        ui.label(
            RichText::new(format!("{:<14}", target_short))
                .color(Color32::from_gray(140))
                .monospace(),
        );
        ui.label(RichText::new(&entry.message).color(level_color));
    });
}
