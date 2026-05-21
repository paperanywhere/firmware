//! Power management — implements [`Sleeper`] for the device. Deep sleep with
//! RTC wake for `ScheduledWake`, light spin for `AlwaysOn`, battery voltage
//! readout via ADC.
//!
//! **Safety:** operates on RAM + RTC peripheral state only. No eFuse writes,
//! no flash encryption, no secure-boot configuration. Permanent hardware
//! operations are explicitly out of scope.

use core::time::Duration;

use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::Rtc;
use esp_hal::rtc_cntl::sleep::TimerWakeupSource;
use paperanywhere_ports::{PowerPolicy, Sleeper};

pub struct FwSleeper {
    rtc: Rtc<'static>,
}

impl FwSleeper {
    pub fn new(lpwr: LPWR<'static>) -> Self {
        Self { rtc: Rtc::new(lpwr) }
    }
}

impl Sleeper for FwSleeper {
    fn sleep_for(&mut self, seconds: u32, policy: PowerPolicy) {
        match policy {
            // `sleep_deep` is `-> !` — when the RTC wakes, the chip resets
            // and `main()` runs again from cold. From the runtime's point of
            // view this looks like sleep_for returned normally; in reality
            // we'll never reach the next instruction here.
            PowerPolicy::ScheduledWake => {
                esp_println::println!("power: deep_sleep_for({seconds}s)");
                // Let the UART FIFO drain before the deep_sleep
                // transition. Without this delay, the last ~10 chars
                // of the most recent log line (often a `warn!`/`error!`
                // explaining WHY we're sleeping) get truncated, which
                // makes serial-monitoring failures genuinely confusing.
                esp_hal::delay::Delay::new().delay_millis(200);
                let timer = TimerWakeupSource::new(Duration::from_secs(seconds as u64));
                self.rtc.sleep_deep(&[&timer]);
            }
            // AlwaysOn: a busy yield. The radio stays up, the polling cycle
            // resumes when the loop checks `next_check_at`. Real modem-sleep
            // (lower-current idle) requires embassy-time + esp-rtos's idle
            // hook; deferred to a follow-up.
            PowerPolicy::AlwaysOn => {
                esp_println::println!("power: busy_wait_for({seconds}s) (AlwaysOn)");
                for _ in 0..(seconds.saturating_mul(2_000_000)) {
                    core::hint::spin_loop();
                }
            }
        }
    }

    fn unix_now(&self) -> u64 {
        // Without an NTP client + RTC sync we don't know wall-clock time.
        // Returning 0 makes the runtime treat `next_check_at` as "sleep the
        // full interval" (since `next_check_at.saturating_sub(0) == next`),
        // which is the right fallback.
        0
    }

    fn battery_mv(&self) -> Option<u16> {
        battery_mv()
    }
}

/// Battery voltage in millivolts. Wired in a follow-up once the board's
/// `battery_adc` channel is bound to an `esp_hal::analog::adc::Adc`.
pub fn battery_mv() -> Option<u16> {
    None
}
