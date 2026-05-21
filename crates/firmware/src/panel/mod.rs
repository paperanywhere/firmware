//! Panel-driver glue. The trait + concrete protocol drivers live in their
//! own crates (`paperanywhere-ports`, `paperanywhere-panel-uc8179`) so the
//! desktop simulator can drive the same protocol code against in-memory
//! fakes. This module is just the firmware-side selection table.

// Re-export the trait so existing imports `use crate::panel::EpaperPanel`
// keep working; new code should prefer `paperanywhere_ports::EpaperPanel`.
pub use paperanywhere_ports::EpaperPanel;

// Re-export the UC8179 driver type aliases so per-board modules can name
// their concrete Panel type without listing it twice.
pub use paperanywhere_panel_uc8179 as uc8179;

/// A do-nothing panel used while a board's SPI wiring is still pending.
/// The runtime streams bytes into it, refresh logs and discards. Boards
/// that aren't fully wired yet (or that we run firmware against without
/// hardware) hand this to the runtime instead of a real driver.
pub struct NoopPanel;

impl EpaperPanel for NoopPanel {
    async fn init(&mut self) {
        esp_println::println!("panel(noop): init");
    }
    async fn write_chunk(&mut self, bytes: &[u8]) {
        esp_println::println!("panel(noop): write_chunk {} bytes", bytes.len());
    }
    async fn refresh(&mut self) {
        esp_println::println!("panel(noop): refresh");
    }
}
