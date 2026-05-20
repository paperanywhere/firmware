//! Panel-driver trait + per-controller impls. Selected at compile time via
//! board feature; firmware ships one binary per panel family.

pub trait EpaperPanel {
    /// Reset + initialize controller. Called once at boot.
    fn init(&mut self);
    /// Stream a row of pre-packed bytes into the controller buffer.
    fn write_row(&mut self, row: u32, bytes: &[u8]);
    /// Commit the buffer to the panel.
    fn refresh(&mut self);
}

#[cfg(any(feature = "board-reterminal-e1001", feature = "board-generic-esp32s3-waveshare-75"))]
pub mod uc8179;
