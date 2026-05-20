//! Virtual e-paper panel for the sim.
//!
//! Implements [`EpaperPanel`] by routing the runtime's bytes through the
//! same `Uc8179` driver the firmware uses. The driver writes via
//! `embedded-hal` SPI + GPIO traits; here those traits are implemented by
//! a tiny set of fakes that snoop the byte stream:
//!
//!   * `RecordingSpi` watches the DC pin to distinguish commands from data,
//!     and accumulates `DTM2` data writes into a frame-RAM buffer.
//!   * `RecordingDc` / `RecordingRst` are dummy `OutputPin`s that just hold
//!     a `Cell<bool>` shared with the SPI so it sees the current DC level.
//!   * `AlwaysIdleBusy` is an `InputPin` that always reports HIGH so
//!     `wait_idle` returns immediately (the sim doesn't model the panel's
//!     refresh latency).
//!   * `NoOpDelay` skips all of UC8179's per-command delays.
//!
//! When the driver issues `CMD_DISPLAY_REFRESH (0x12)`, the recorder
//! unpacks its accumulated frame RAM into the shared RGB framebuffer and
//! pings egui to repaint. Same protocol exercises in both worlds — if the
//! driver sends bytes the firmware would also send, the sim shows what the
//! firmware would render.
//!
//! Today this only handles Mono1bpp (what UC8179 produces). Color7 panels
//! land when we add their controller driver (UC8159 etc.) following the
//! same pattern.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::{ErrorType, InputPin, OutputPin};
use embedded_hal::spi::{ErrorType as SpiErrorType, SpiDevice};
use paperanywhere_panel_uc8179::{CMD_DISPLAY_REFRESH, CMD_DTM2, Pins, Uc8179};
use paperanywhere_ports::EpaperPanel;

use crate::state::SimState;

/// Adapter that the runtime sees. It owns a fully-wired `Uc8179<...fakes...>`
/// and forwards `init` / `write_chunk` / `refresh` to it. The fakes share
/// state via `Rc<RefCell>`-ish primitives (here we use `Cell` plus shared
/// `Arc<SimState>` for the framebuffer; everything stays on one thread —
/// the runtime task — so this is fine without a mutex on the hot path).
pub struct VirtualPanel {
    driver: Uc8179<RecordingSpi, RecordingPin, RecordingPin, AlwaysIdleBusy, NoOpDelay>,
}

impl VirtualPanel {
    pub fn new(state: Arc<SimState>, _color_mode: paperanywhere_ports::ColorMode) -> Self {
        // The DC level toggles between every command and data write — shared
        // between the DC pin (writes it) and the SPI device (reads it to
        // distinguish CMD bytes from frame data). Arc<Mutex<_>> rather than
        // Rc<Cell<_>> so the resulting VirtualPanel is `Send`, which tokio's
        // multi-threaded runtime requires when spawning the runtime task.
        let dc_level = SharedBool::new(false);
        let spi = RecordingSpi {
            state,
            dc_level: dc_level.clone(),
            mode: Arc::new(Mutex::new(RxMode::AwaitingCommand)),
        };
        let pins = Pins {
            rst: RecordingPin::detached(),
            dc: RecordingPin::with_level(dc_level),
            busy: AlwaysIdleBusy,
        };
        // Match the firmware's reTerminal-E1001 panel polarity by default.
        // If we someday simulate a non-inverted panel variant we add a
        // SimConfig flag and wire it through.
        let driver = Uc8179::new(spi, pins, NoOpDelay, true);
        Self { driver }
    }
}

impl EpaperPanel for VirtualPanel {
    fn init(&mut self) {
        self.driver.init();
    }

    fn write_chunk(&mut self, bytes: &[u8]) {
        self.driver.write_chunk(bytes);
    }

    fn refresh(&mut self) {
        self.driver.refresh();
    }
}

// ── shared state primitives ──

/// `Arc<Mutex<bool>>` cosmetically. Used for the DC pin level, which the SPI
/// recorder peeks to distinguish command bytes from frame data.
#[derive(Clone)]
struct SharedBool(Arc<Mutex<bool>>);

impl SharedBool {
    fn new(initial: bool) -> Self {
        Self(Arc::new(Mutex::new(initial)))
    }
    fn set(&self, v: bool) {
        *self.0.lock().unwrap() = v;
    }
    fn get(&self) -> bool {
        *self.0.lock().unwrap()
    }
}

// ── fake DC / RST pin ──

/// Output pin that records its level in a shared cell. The DC pin uses
/// this; the RST pin uses the "detached" variant whose writes go nowhere.
struct RecordingPin {
    level: Option<SharedBool>,
}

impl RecordingPin {
    fn with_level(level: SharedBool) -> Self {
        Self { level: Some(level) }
    }
    fn detached() -> Self {
        Self { level: None }
    }
}

impl ErrorType for RecordingPin {
    type Error = Infallible;
}

impl OutputPin for RecordingPin {
    fn set_low(&mut self) -> Result<(), Self::Error> {
        if let Some(c) = self.level.as_ref() {
            c.set(false);
        }
        Ok(())
    }
    fn set_high(&mut self) -> Result<(), Self::Error> {
        if let Some(c) = self.level.as_ref() {
            c.set(true);
        }
        Ok(())
    }
}

// ── fake BUSY pin (always idle) ──

struct AlwaysIdleBusy;

impl ErrorType for AlwaysIdleBusy {
    type Error = Infallible;
}

impl InputPin for AlwaysIdleBusy {
    fn is_high(&mut self) -> Result<bool, Self::Error> {
        // UC8179's BUSY is active-low → high means "idle, ready". The sim
        // doesn't model refresh latency.
        Ok(true)
    }
    fn is_low(&mut self) -> Result<bool, Self::Error> {
        Ok(false)
    }
}

// ── no-op Delay ──

struct NoOpDelay;

impl DelayNs for NoOpDelay {
    fn delay_ns(&mut self, _ns: u32) {}
}

// ── recording SPI ──

#[derive(Clone, Copy, Debug)]
enum RxMode {
    /// Next byte arriving on the wire is a UC8179 command opcode.
    AwaitingCommand,
    /// Driver is in DTM2 (0x13) data plane — frame bytes are pixel data.
    StreamingFrameData,
    /// Some other command's data follows — record but ignore for now.
    StreamingOtherData,
}

struct RecordingSpi {
    state: Arc<SimState>,
    dc_level: SharedBool,
    mode: Arc<Mutex<RxMode>>,
}

impl SpiErrorType for RecordingSpi {
    type Error = Infallible;
}

impl SpiDevice for RecordingSpi {
    fn transaction(
        &mut self,
        operations: &mut [embedded_hal::spi::Operation<'_, u8>],
    ) -> Result<(), Self::Error> {
        for op in operations {
            match op {
                embedded_hal::spi::Operation::Write(bytes) => self.absorb(bytes),
                embedded_hal::spi::Operation::Read(buf) => {
                    // UC8179 never asks the controller for anything in our
                    // command flow, but fill with zeros to be safe.
                    buf.fill(0);
                }
                embedded_hal::spi::Operation::Transfer(read, write) => {
                    self.absorb(write);
                    read.fill(0);
                }
                embedded_hal::spi::Operation::TransferInPlace(buf) => {
                    self.absorb(buf);
                    // No real device to echo, leave buf untouched.
                }
                embedded_hal::spi::Operation::DelayNs(_) => {}
            }
        }
        Ok(())
    }
}

impl RecordingSpi {
    fn absorb(&mut self, bytes: &[u8]) {
        if !self.dc_level.get() {
            // DC low = command byte stream. First byte is the opcode; any
            // trailing bytes in this same chunk would still be commands,
            // which the UC8179 driver never does (it always splits cmd /
            // data). We still handle multi-byte command chunks defensively.
            for &b in bytes {
                self.handle_command(b);
            }
        } else {
            // DC high = data byte stream. What we do with it depends on
            // which command preceded it.
            let mode = *self.mode.lock().unwrap();
            match mode {
                RxMode::StreamingFrameData => {
                    let mut fb = self.state.framebuffer.lock().unwrap();
                    // Defer the actual unpack until refresh; we just keep
                    // a staging buffer here. The unpack at refresh time is
                    // identical to what the old non-protocol VirtualPanel
                    // did, so the user sees the same image.
                    fb.staging.extend_from_slice(bytes);
                }
                _ => {
                    // We record but don't act on other command's data —
                    // panel config bytes, etc. Useful for future debugging.
                }
            }
        }
    }

    fn handle_command(&mut self, op: u8) {
        match op {
            CMD_DTM2 => *self.mode.lock().unwrap() = RxMode::StreamingFrameData,
            CMD_DISPLAY_REFRESH => {
                // Commit: unpack staging into the visible framebuffer. Pull
                // the staging bytes out by `mem::take` so we have a clean
                // mutable borrow of `fb` for the unpack target — and an
                // empty staging buffer drops in afterwards, ready for the
                // next frame.
                let mut fb = self.state.framebuffer.lock().unwrap();
                let staging = core::mem::take(&mut fb.staging);
                unpack_mono_1bpp_inplace(&staging, &mut fb);
                fb.generation = fb.generation.wrapping_add(1);
                let generation = fb.generation;
                drop(fb);
                self.state
                    .set_status(format!("panel: refreshed (gen {})", generation));
                self.state
                    .push_activity(format!("uc8179 refresh #{}", generation));
                self.state.request_repaint();
                *self.mode.lock().unwrap() = RxMode::AwaitingCommand;
            }
            _ => *self.mode.lock().unwrap() = RxMode::StreamingOtherData,
        }
    }
}

/// Mono1bpp unpack — 8 pixels per byte, MSB-first.
///
/// Convention: `bit = 0` is white (paper), `bit = 1` is black (ink). This
/// matches what the panel sees on the SPI wire — the UC8179 driver inverts
/// renderer-friendly "1 = white" bytes before sending, and our
/// `RecordingSpi` captures the post-inversion stream. Unpacking with the
/// same convention as the real panel means the sim shows what the device
/// would actually render.
fn unpack_mono_1bpp_inplace(src: &[u8], fb: &mut crate::state::Framebuffer) {
    let width = fb.width as usize;
    let height = fb.height as usize;
    let mut out = vec![255u8; width * height * 3];
    let stride_bytes = (width + 7) / 8;
    for (row_idx, row_bytes) in src.chunks(stride_bytes).take(height).enumerate() {
        for (byte_idx, &b) in row_bytes.iter().enumerate() {
            for bit in 0..8 {
                let x = byte_idx * 8 + bit;
                if x >= width {
                    break;
                }
                let inked = (b & (1 << (7 - bit))) != 0;
                let v = if inked { 0 } else { 255 };
                let idx = (row_idx * width + x) * 3;
                out[idx] = v;
                out[idx + 1] = v;
                out[idx + 2] = v;
            }
        }
    }
    fb.pixels = out;
}
