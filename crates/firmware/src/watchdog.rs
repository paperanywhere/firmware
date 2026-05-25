//! Hardware Task Watchdog Timer with per-core liveness gating.
//!
//! Uses the ESP32-S3's RTC Watchdog (RWDT). The WDT is fed by a single
//! feeder task on core 0 — but ONLY when both cores' heartbeats have
//! advanced since the last feed. If either core's heartbeat task stops
//! ticking the WDT will not be fed and the chip resets within
//! `STAGE0_TIMEOUT`.
//!
//! Why per-core gating: a single feeder running on core 0 that
//! unconditionally feeds the WDT would mask a core-1 hang (runtime /
//! panel actor wedged) because the feeder itself is still alive. We
//! want the WDT to bite on either-core silence.
//!
//! Deep sleep: `Rwdt::enable()` sets `wdt_pause_in_slp=true`, so a
//! `PowerPolicy::ScheduledWake` that calls `rtc.sleep_deep()` does not
//! trip the watchdog. After the RTC wakes, the chip cold-boots and
//! re-init brings the WDT back up.

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_time::{Duration, Timer};
use esp_hal::rtc_cntl::{Rwdt, RwdtStage};

/// Stage-0 reset timeout. 60 s is comfortably above the worst-case
/// wake-cycle latency (HTTP register + image fetch + panel refresh,
/// observed peak ~20 s) and short enough that a hang is user-visible
/// within a minute.
pub const STAGE0_TIMEOUT_SECS: u64 = 60;

/// Feeder cadence — checks per-core heartbeats and feeds the WDT
/// if both have advanced. 5 s gives 12× margin against the 60 s
/// stage-0 timeout, so a single missed feed (e.g. core 0 briefly
/// busy under HTTP load) doesn't trigger a false reset.
const FEED_INTERVAL: Duration = Duration::from_secs(5);

/// Per-core heartbeat counters. Incremented by `touch_core{0,1}`
/// from whatever task represents that core's liveness signal. The
/// feeder reads these to decide whether to feed the WDT.
static CORE0_TICK: AtomicU32 = AtomicU32::new(0);
static CORE1_TICK: AtomicU32 = AtomicU32::new(0);

/// Owned Rwdt handle. Initialized once at boot via `init()`. The
/// feeder task locks briefly to feed; nothing else touches it.
static RWDT_CELL: BlockingMutex<CriticalSectionRawMutex, RefCell<Option<Rwdt>>> =
    BlockingMutex::new(RefCell::new(None));

/// Configure and enable the RWDT. Call once during boot before
/// spawning the feeder task.
pub fn init(mut rwdt: Rwdt) {
    rwdt.set_timeout(
        RwdtStage::Stage0,
        esp_hal::time::Duration::from_secs(STAGE0_TIMEOUT_SECS),
    );
    rwdt.enable();
    rwdt.feed();
    RWDT_CELL.lock(|c| *c.borrow_mut() = Some(rwdt));
    log::info!(
        "wdt: RTC watchdog armed (stage0 reset @ {}s, both-core gated)",
        STAGE0_TIMEOUT_SECS,
    );
}

/// Mark core 0 as alive. Called from the core-0 heartbeat task.
pub fn touch_core0() {
    CORE0_TICK.fetch_add(1, Ordering::Relaxed);
}

/// Mark core 1 as alive. Called from the core-1 heartbeat task.
pub fn touch_core1() {
    CORE1_TICK.fetch_add(1, Ordering::Relaxed);
}

/// WDT feeder. Spawn on core 0 once.
#[embassy_executor::task]
pub async fn feeder_task() -> ! {
    // Boot grace: don't bite while core 1's executor is still
    // starting up — first core-1 heartbeat may take a few seconds
    // to land after the cross-core handoff.
    Timer::after(Duration::from_secs(15)).await;
    let mut last0 = CORE0_TICK.load(Ordering::Relaxed);
    let mut last1 = CORE1_TICK.load(Ordering::Relaxed);
    loop {
        Timer::after(FEED_INTERVAL).await;
        let n0 = CORE0_TICK.load(Ordering::Relaxed);
        let n1 = CORE1_TICK.load(Ordering::Relaxed);
        let core0_alive = n0 != last0;
        let core1_alive = n1 != last1;
        if core0_alive && core1_alive {
            RWDT_CELL.lock(|c| {
                if let Some(w) = c.borrow_mut().as_mut() {
                    w.feed();
                }
            });
            last0 = n0;
            last1 = n1;
        } else {
            // Don't advance last{0,1} — we want the deltas to keep
            // showing the stale window until the WDT fires or the
            // wedged core recovers.
            log::warn!(
                "wdt: STALE — core0 ticks={} (Δ={}) core1 ticks={} (Δ={}) — NOT feeding RWDT",
                n0,
                n0.wrapping_sub(last0),
                n1,
                n1.wrapping_sub(last1),
            );
        }
    }
}
