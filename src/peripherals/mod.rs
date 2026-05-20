//! Optional peripherals exposed by integrated boards (buttons, sensors, buzzer).
//! Each module is gated by the board's capability flags so dead code is excluded
//! from boards that lack the peripheral.

pub mod battery;
pub mod buttons;
pub mod sensors;
pub mod buzzer;
