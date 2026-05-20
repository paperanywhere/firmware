//! Short audio chirp on `Update::Applied` — opt-in per device via dashboard.

pub fn beep_ms(_ms: u32) {
    // M4: PWM on the buzzer GPIO from board config.
}
