//! Battery gauge implementations.
//!
//! Per-board readout lives here so the runtime doesn't grow `cfg`
//! branches per product. The active board's [`BoardConfig`] picks
//! the gauge constructor at build time (see [`new_from_resources`]).
//!
//! ## reTerminal E1001 family
//!
//! Same chassis platform across E1001/E1002/E1003/E1004 — all four
//! gate a voltage divider on GPIO21 (`bsp_battery_enable`, active
//! high) and feed the divided voltage into GPIO1 on ADC1. The
//! divider halves the pack voltage so we read up to ~2.1 V on a
//! fully-charged 4.2 V LiPo, comfortably inside ADC1's 12 dB
//! attenuation range (~3.1 V full-scale). Calibrated mV is recovered
//! via the SAR-ADC factory curve, then doubled to undo the divider.
//!
//! The enable pin matters: leaving GPIO21 high during deep sleep
//! burns a small but continuous current through the divider, which
//! defeats the whole point of 3-month battery life. We drive it
//! high only for the duration of a sample (~5 ms total) and drop it
//! back to low before returning.
//!
//! ## Other boards
//!
//! Boards with a USB-only carrier (`generic_esp32s3_waveshare_75`)
//! return [`NoBatteryGauge`] which always reports `None`. When a
//! board lands that uses a real fuel-gauge IC (MAX17048 etc.) it
//! gets a new impl here and `new_from_resources` picks it up via
//! the existing `cfg`.

use paperanywhere_ports::{BatteryGauge, BatterySample, lipo_percent_from_mv};

use crate::resources::BatteryHardware;

/// Gauge for boards with no battery hardware. `sample` always
/// returns `None`. Zero-sized; costs nothing at runtime.
pub struct NoBatteryGauge;

impl BatteryGauge for NoBatteryGauge {
    async fn sample(&mut self) -> Option<BatterySample> {
        None
    }
}

#[cfg(feature = "board-reterminal-e1001")]
mod reterminal {
    use embassy_time::{Duration, Timer};
    use esp_hal::Blocking;
    use esp_hal::analog::adc::{Adc, AdcCalCurve, AdcConfig, AdcPin, Attenuation};
    use esp_hal::gpio::{Level, Output, OutputConfig};
    use esp_hal::peripherals::{ADC1, GPIO1};

    use paperanywhere_ports::{BatteryGauge, BatterySample, lipo_percent_from_mv};

    use crate::resources::BatteryHardware;

    /// reTerminal E-series voltage-divider gauge. Owns:
    ///   - ADC1 in blocking mode (single conversion, no DMA).
    ///   - The calibrated AdcPin for GPIO1 (`AdcCalCurve` uses the
    ///     factory characterisation eFuse on ESP32-S3 — most accurate
    ///     scheme esp-hal offers).
    ///   - The active-high enable output on GPIO21.
    pub struct ReTerminalBatteryGauge {
        adc: Adc<'static, ADC1<'static>, Blocking>,
        pin: AdcPin<GPIO1<'static>, ADC1<'static>, AdcCalCurve<ADC1<'static>>>,
        enable: Output<'static>,
    }

    impl ReTerminalBatteryGauge {
        pub fn new(hw: BatteryHardware) -> Self {
            let mut cfg: AdcConfig<ADC1<'static>> = AdcConfig::new();
            // 12 dB attenuation gives a usable input range of ~0..3.1 V
            // on ESP32-S3 — well above our worst-case post-divider
            // 2.1 V on a 4.2 V pack.
            let pin = cfg.enable_pin_with_cal::<_, AdcCalCurve<ADC1<'static>>>(
                hw.batt_sense,
                Attenuation::_11dB,
            );
            let adc = Adc::new(hw.adc1, cfg);
            let enable = Output::new(
                hw.batt_enable,
                Level::Low,
                OutputConfig::default(),
            );
            Self { adc, pin, enable }
        }
    }

    impl BatteryGauge for ReTerminalBatteryGauge {
        async fn sample(&mut self) -> Option<BatterySample> {
            // 1. Power the divider.
            self.enable.set_high();
            // 2. Let the divider settle. The on-board RC network
            //    needs a couple of ms to reach the rail; sampling
            //    too early reads back ~0 mV.
            Timer::after(Duration::from_millis(2)).await;

            // 3. Take a small batch + median them. Single-shot ADC
            //    reads on ESP32-S3 carry ~±50 mV of noise; a median
            //    of 5 keeps the status bar from flickering 1% up
            //    and down between samples.
            let mut samples: [u16; 5] = [0; 5];
            for slot in samples.iter_mut() {
                *slot = self.adc.read_blocking(&mut self.pin);
            }
            samples.sort_unstable();
            let median_mv = samples[samples.len() / 2];

            // 4. Drop the enable line so we're not bleeding through
            //    the divider for the rest of the wake cycle.
            self.enable.set_low();

            // 5. Compensate for the resistor divider (Seeed's
            //    ESPHome YAML applies a `multiply: 2.0`, so the
            //    on-board divider is 1:1 with ~equal upper/lower
            //    resistors). Saturating-mul guards against a
            //    pathological cal that overshoots 2^16 mV.
            let pack_mv = (median_mv as u32).saturating_mul(2).min(u16::MAX as u32) as u16;

            // 6. SoC from the shared LiPo curve. Boards with a real
            //    fuel-gauge chip would skip this and use the chip's
            //    own reading instead.
            Some(BatterySample {
                mv: pack_mv,
                percent: lipo_percent_from_mv(pack_mv),
            })
        }
    }
}

#[cfg(feature = "board-reterminal-e1001")]
pub use reterminal::ReTerminalBatteryGauge;

/// Concrete gauge type the rest of the firmware sees for the active
/// board feature. Boards that share the reTerminal chassis use the
/// same impl; the USB-only carrier falls back to `NoBatteryGauge`.
#[cfg(feature = "board-reterminal-e1001")]
pub type FwBatteryGauge = ReTerminalBatteryGauge;
#[cfg(not(feature = "board-reterminal-e1001"))]
pub type FwBatteryGauge = NoBatteryGauge;

/// Construct the gauge from the hardware bundle main.rs collected.
/// Returns `NoBatteryGauge` when `hw` is None (board has no battery
/// pins wired) or when the active board feature has no gauge impl.
pub fn new_from_resources(hw: Option<BatteryHardware>) -> FwBatteryGauge {
    #[cfg(feature = "board-reterminal-e1001")]
    {
        let hw = hw.expect(
            "reTerminal E1001 build requires battery hardware in FirmwareResources",
        );
        ReTerminalBatteryGauge::new(hw)
    }
    #[cfg(not(feature = "board-reterminal-e1001"))]
    {
        let _ = hw;
        NoBatteryGauge
    }
}
