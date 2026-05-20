//! Wake-on-button + debounce. Surfaces button presses as a `WakeReason::Button`
//! that the next `DeviceMsg::Hello` reports to the backend.

pub fn pressed_at_wake() -> Option<u8> {
    // M4: query RTC GPIO wake source.
    None
}
