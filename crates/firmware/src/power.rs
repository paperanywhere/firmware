//! Power management — implements [`Sleeper`] for the device. Deep sleep with
//! RTC wake for `ScheduledWake`, light spin for `AlwaysOn`, battery voltage
//! readout via ADC.
//!
//! **Safety:** operates on RAM + RTC peripheral state only. No eFuse writes,
//! no flash encryption, no secure-boot configuration. Permanent hardware
//! operations are explicitly out of scope.

use core::time::Duration;

use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::{Rtc, Rwdt};
use esp_hal::rtc_cntl::sleep::TimerWakeupSource;
use paperanywhere_ports::{PowerPolicy, Sleeper};

pub struct FwSleeper {
    rtc: Rtc<'static>,
}

impl FwSleeper {
    pub fn new(lpwr: LPWR<'static>) -> Self {
        Self { rtc: Rtc::new(lpwr) }
    }

    /// Construct a `FwSleeper` alongside an owned `Rwdt` handle, taken
    /// from the same RTC peripheral. Rwdt is a zero-sized type — its
    /// state lives in the LP_WDT registers, not in the struct — so
    /// reading it out via `ptr::read` is safe: both the original
    /// `rtc.rwdt` slot and the extracted copy operate on the same
    /// hardware. We give the extracted one to the watchdog feeder
    /// task and never touch `rtc.rwdt` again from sleep paths
    /// (sleep_deep doesn't reference it).
    pub fn new_with_rwdt(lpwr: LPWR<'static>) -> (Self, Rwdt) {
        let rtc = Rtc::new(lpwr);
        // SAFETY: Rwdt is a ZST (`pub struct Rwdt(())` — the inner
        // unit is the only field). ptr::read on a ZST is a no-op at
        // runtime. Both the original and the duplicate refer to the
        // same global RTC-WDT register block; there is no per-handle
        // state to conflict.
        let rwdt = unsafe { core::ptr::read(&rtc.rwdt as *const Rwdt) };
        (Self { rtc }, rwdt)
    }
}

impl Sleeper for FwSleeper {
    async fn sleep_for(&mut self, seconds: u32, policy: PowerPolicy) {
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
            // AlwaysOn: await an embassy timer so the executor can run
            // other tasks (embassy-net's background polling, panel
            // actor, etc.) during the wake-cycle gap. A sync busy-spin
            // here used to be THE bug that made the device unreachable
            // — the runtime_task held the CPU for the full wake
            // interval, starving every other task.
            PowerPolicy::AlwaysOn => {
                esp_println::println!("power: async sleep_for({seconds}s) (AlwaysOn)");
                embassy_time::Timer::after(embassy_time::Duration::from_secs(seconds as u64))
                    .await;
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
}
