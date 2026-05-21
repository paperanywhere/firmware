//! Build-time rasterised Font Awesome icon bitmaps + the tiny blit
//! helper the status bar uses to paint them onto the framebuffer.
//!
//! Each icon is rasterised at [`ICON_PX`] × [`ICON_PX`] Mono1bpp by
//! `build.rs` using the same boot-screen rasteriser the logo uses,
//! and embedded as a `pub const &[u8]`. Naming convention:
//! `paperanywhere_compositor::icons::WIFI`.
//!
//! Bitmap convention matches the rasteriser's "bit set = white = no
//! ink" — to overlay an icon on a white background we walk every bit
//! in the source and call `set_pixel(on=true)` only when the source
//! bit is clear (i.e. the FA path is painted there). White pixels
//! aren't touched, which leaves whatever was already in the
//! framebuffer underneath intact.

/// Edge length used by `build.rs`. Status bar code reads this when
/// computing widget cell widths.
pub const ICON_PX: u32 = 20;

pub static WIFI: &[u8] = include_bytes!(env!("PAPERANYWHERE_ICON_WIFI"));
pub static WIFI_SLASH: &[u8] = include_bytes!(env!("PAPERANYWHERE_ICON_WIFI_SLASH"));
