//! Power management — deep sleep with RTC wake, modem sleep for always-on,
//! battery voltage monitoring via ADC.
//!
//! **Safety:** operates on RAM + RTC peripheral state only. No eFuse writes,
//! no flash encryption, no secure-boot configuration. Permanent hardware
//! operations are explicitly out of scope.

/// Deep-sleep until `seconds` elapse on the RTC. Returns `!` because control
/// resumes from `main` on next boot, not from this call.
///
/// M4 wires `esp_hal::rtc_cntl::Rtc::sleep_deep` with a `TimerWakeupSource`.
pub fn deep_sleep_for(seconds: u32) -> ! {
    esp_println::println!("power: deep_sleep_for({seconds}s) — halting (M4 wires RTC)");
    loop { core::hint::spin_loop(); }
}

/// Light yield between WS heartbeats in `always_on` mode. Lower power than
/// a busy loop but keeps WiFi + CPU live.
pub fn modem_sleep_ms(_ms: u32) {
    // M4: configure modem-sleep + use embassy-time / busy delay.
}

/// Battery voltage in millivolts, via the board's `battery_adc` ADC channel.
/// `None` on boards without a configured ADC.
pub fn battery_mv() -> Option<u16> {
    None
}
