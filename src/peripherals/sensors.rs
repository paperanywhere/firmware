//! Temperature + humidity sensor readout. reTerminal E-series boards include
//! an integrated sensor over I2C; bare-panel boards skip this module entirely.

pub struct Reading {
    pub temp_c: f32,
    pub humidity_pct: f32,
}

pub fn read() -> Option<Reading> {
    // M4: I2C transaction against the integrated sensor part.
    None
}
