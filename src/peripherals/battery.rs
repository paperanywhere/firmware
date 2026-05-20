//! Battery voltage monitor — ADC read on `board.battery_adc`. Returns millivolts.

pub fn read_mv() -> Option<u16> {
    // M4: ADC1 channel from board config, voltage divider, calibration.
    None
}

pub fn state_of_charge(mv: u16) -> u8 {
    // Approximate Li-ion SoC curve. Real boards should swap in a panel-specific table.
    match mv {
        0..=3300 => 0,
        3301..=3500 => 15,
        3501..=3700 => 40,
        3701..=3900 => 70,
        3901..=4100 => 90,
        _ => 100,
    }
}
