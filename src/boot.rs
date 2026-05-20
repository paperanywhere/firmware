//! Boot flow. On every cold boot:
//! 1. Read NVS. If unprovisioned → run claim flow (WiFi credential capture + claim code render).
//! 2. If provisioned → choose `scheduled_wake` or `always_on` main loop.

use esp_println::println;
use paperanywhere_proto::PowerPolicy;

use crate::boards::BoardConfig;

pub fn run(board: BoardConfig) -> ! {
    println!("boot: panel {}x{} model_id={}", board.panel_width_px, board.panel_height_px, board.panel_model_id);

    // M4 work — sequence is sketched here, real impl follows.
    let provisioned = crate::nvs::load_device_token().is_some();
    if !provisioned {
        println!("boot: unprovisioned, entering claim flow");
        crate::nvs::claim_flow_stub(board);
    }

    match board.default_power_policy {
        PowerPolicy::ScheduledWake => scheduled_wake_loop(board),
        PowerPolicy::AlwaysOn => always_on_loop(board),
    }
}

fn scheduled_wake_loop(board: BoardConfig) -> ! {
    // 1. associate WiFi
    // 2. open WSS with ?token=<device_token>
    // 3. send Hello { last_event_id }
    // 4. handle Update / Sleep
    // 5. download blob, stream to panel
    // 6. Ack Applied
    // 7. deep_sleep(sleep_until)
    println!("scheduled_wake: sleep_interval={}s", board.default_sleep_interval_sec);
    loop {
        crate::power::deep_sleep_for(board.default_sleep_interval_sec);
    }
}

fn always_on_loop(board: BoardConfig) -> ! {
    println!("always_on: heartbeat loop");
    loop {
        // pump WS, react to Updates, send Heartbeats.
        crate::power::modem_sleep_ms(1000);
    }
}
