//! Power management — deep sleep with RTC wake, modem sleep for always-on.

pub fn deep_sleep_for(seconds: u32) -> ! {
    // M4: configure RTC timer, esp_hal::sleep::deep_sleep(...).
    esp_println::println!("power: deep_sleep_for({}s) — placeholder, halting", seconds);
    loop {
        core::hint::spin_loop();
    }
}

pub fn modem_sleep_ms(_ms: u32) {
    // M4: light/modem sleep via embassy_time::Timer.
}

pub fn battery_mv() -> Option<u16> {
    // M4: read ADC via board.battery_adc.
    None
}
