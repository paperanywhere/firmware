//! Panel-renderer actor task.
//!
//! Owns the concrete [`boards::Panel`] exclusively. Consumes
//! [`PaintCmd`]s from a static channel and translates each one into
//! the right sequence of `EpaperPanel` trait calls plus a refresh.
//! All other tasks (runtime polling loop, OTA install path, anyone
//! touching the OTA progress signal) send through the channel — they
//! never directly hold a `&mut Panel`.
//!
//! On ESP32-S3 this task is spawned on the **second core** (core 1).
//! Core 0 keeps running WiFi, embassy-net, and the runtime polling
//! loop; core 1 handles the panel exclusively. This means the sync
//! SPI bytes of `write_chunk` (~100 ms for a 48 KB frame) and the
//! wait_idle async polling no longer compete with WiFi servicing for
//! executor time — they run on a CPU that has nothing else to do.
//!
//! On a single-core fallback (esp32, esp32c3, esp32c6, etc.) the
//! actor still works — it just runs cooperatively on core 0 like
//! every other task, and the SPI windows still block other tasks
//! (the same caveat the actor pattern carried before dual-core was
//! wired in). The board-specific spawning is gated in `boot.rs`.
//!
//! ## Why an actor pattern?
//!
//! Before this, every panel call site (`panel.set_status`,
//! `panel.refresh().await`, `panel.render_adoption_screen`, etc.)
//! grabbed `&mut panel` for the duration of its work. That meant
//! multiple tasks wanted exclusive access at different times, with
//! the contention shoehorned into a single `&mut P` parameter.
//! Worse, every multi-second refresh held the executor on its
//! caller's task, starving WiFi packet servicing and tripping the AP
//! into deauthenticating us.
//!
//! With the actor: producers compose a `PaintCmd`, push it down the
//! channel, and continue immediately. The actor consumes commands
//! one at a time and does the SPI + LUT-execution work in isolation.
//! On dual-core that's true parallelism with WiFi. On single core
//! it at least centralises the "panel ownership" question in one
//! task body, making the eventual async-SPI / DMA migration (task
//! #90) touch only this file.

use embassy_futures::select::{Either3, select3};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use log::info;
use paperanywhere_ports::{EpaperPanel, OtaPhase};
use paperanywhere_ports::chrome::{self, RefreshKind};
use paperanywhere_runtime::{PaintChannel, PaintCmd, SeqCmd, mark_processed};

use crate::boards;

/// Embassy task: the panel-renderer actor. Spawns once on boot and
/// runs forever.
///
/// `panel` is consumed and held inside the task's stack frame for
/// the duration of the program. `paint_rx` is the static channel
/// from which paint commands arrive. `ota_signal` is the OTA
/// progress signal — kept separate from the channel because the
/// OTA install path may write to it from interrupt-adjacent
/// contexts and the Signal's coalesce-to-latest behaviour is the
/// right semantic for progress events (intermediate ticks don't
/// matter; only the freshest does).
#[embassy_executor::task]
pub async fn panel_actor_task(
    panel: &'static mut boards::Panel,
    paint_rx: &'static PaintChannel,
    ota_signal: &'static Signal<CriticalSectionRawMutex, OtaPhase>,
) -> ! {
    info!("panel_actor: starting on core {}", current_core_id());

    // Async-SPI migration (task #90) moved panel initialisation into
    // the actor: boot.rs only stages the boot-screen content into the
    // compositor's framebuffer pre-executor, then we run the actual
    // controller init + first refresh here. Reason: `block_on` with a
    // noop waker can't fire the interrupt waker that esp-hal's async
    // SPI driver registers, so a pre-executor block_on(panel.init())
    // busy-loops forever. By the time this task runs the embassy
    // executor is alive and wakers work.
    panel.init().await;
    // First refresh flushes the pre-staged boot screen (whatever
    // boot.rs put into the compositor framebuffer + chrome state) to
    // the panel hardware. Subsequent refreshes happen via the
    // PaintCmd → commit_view path below.
    info!("panel_actor: flushing boot-screen content via initial refresh");
    panel.compose();
    panel.refresh().await;

    // Tracks whether we're inside an OTA cycle so we can suspend
    // normal view changes (adoption / boot / image) while the user's
    // pa-dev push is in progress. Mirrors the runtime's `ota_active`
    // latch from before this refactor — that latch is now the
    // actor's job.
    let mut ota_active = false;

    // Tracks which "view" the panel is currently showing. The actor
    // needs this when a ChromeChanged signal fires so it can refresh
    // the SAME view (boot screen, adoption, image, halt) — not
    // unconditionally redraw the boot template, which is what was
    // happening before and caused the panel to bounce
    // adoption → boot after register fired chrome::set_device_uuid.
    //
    // Starts as Boot since boot.rs pre-stages the boot template into
    // the compositor framebuffer before we ever get here.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum CurrentView {
        Boot,
        Adoption,
        Main,
        Image,
        Halt,
        // OTA progress is handled separately (the OTA latch above)
        // and ChromeChanged is dropped during ota_active, so there's
        // no need for a CurrentView::Ota variant.
    }
    let mut current_view = CurrentView::Boot;

    // Last-painted framebuffer hash. Used to skip the actual panel
    // refresh when the new framebuffer matches what's already on
    // screen (e.g. the runtime's wake cycle repaints the adoption
    // screen every iteration but the bytes never change). Seeded
    // here from the boot-screen refresh so the next command (which
    // for an unclaimed device is typically a bunch of chrome
    // updates then ShowAdoption) only refreshes when the framebuffer
    // actually changes.
    let mut last_painted_hash: Option<u64> = panel.pending_hash();

    loop {
        // Race three sources:
        //   1. Paint channel — explicit view-transition commands from
        //      the runtime / boot path.
        //   2. OTA progress signal — preempts everything else so the
        //      progress bar updates promptly.
        //   3. Chrome dirty signal — any chrome::set_* call fires this,
        //      requesting a fast (default) or full refresh without
        //      needing a PaintCmd round-trip. This is the auto-refresh
        //      that compositor "subscribes" to via the shared-KV store.
        //
        // For chrome events we synthesise a sentinel command
        // (`ChromeChanged`) so the OTA-mode latch logic below treats
        // it the same as any other "non-OTA paint" — i.e. dropped
        // when ota_active is set. That prevents a rogue chrome update
        // mid-OTA from overwriting the progress view.
        // (seq, cmd) pairs from the channel get matched back to
        // PaintHandle awaiters via `mark_processed` below. The two
        // synthesised sources (OTA progress + chrome dirty) don't
        // have an originating handle, so they use seq=0 — which the
        // monotonic watch then ignores when publishing.
        let (seq, cmd): (u32, PaintCmd) = match select3(
            paint_rx.receive(),
            ota_signal.wait(),
            chrome::dirty_signal().wait(),
        )
        .await
        {
            Either3::First(SeqCmd { seq, cmd }) => (seq, cmd),
            Either3::Second(phase) => (0, PaintCmd::OtaProgress(phase)),
            Either3::Third(kind) => (0, PaintCmd::ChromeChanged(kind)),
        };

        // Latch into / out of OTA mode based on the command kind.
        // While ota_active, drop incoming view-change commands
        // (boot / adoption / halt / image) — they would overwrite
        // the progress view mid-install, which is exactly what we're
        // trying to avoid.
        match &cmd {
            PaintCmd::OtaProgress(phase) => {
                if !matches!(phase, OtaPhase::Idle) {
                    ota_active = true;
                }
                // Failed is terminal; the actor stays on the
                // failure view forever (user power-cycles to clear).
                // Applied is followed by an immediate software_reset
                // from the OTA install path so we never observe the
                // post-Applied state here.
            }
            _ if ota_active => {
                // Drop the queued view-change. The OTA install path
                // holds the executor's attention until the install
                // concludes. Still mark the seq processed so any
                // PaintHandle waiter doesn't hang forever — from the
                // submitter's perspective, the cmd HAS been handled
                // (just by being dropped, not rendered).
                log::debug!("panel_actor: dropping {} during OTA mode", cmd_name(&cmd));
                mark_processed(seq);
                continue;
            }
            _ => {}
        }

        // Track which view the panel is showing, BEFORE dispatching.
        // ChromeChanged needs this to decide what (if anything) to
        // re-render — earlier the actor always redrew the boot
        // template, which caused the panel to bounce back to the
        // boot screen the moment a post-register chrome::set_*
        // fired the dirty signal.
        match &cmd {
            PaintCmd::RedrawBootScreen => current_view = CurrentView::Boot,
            PaintCmd::ShowAdoption { .. } => current_view = CurrentView::Adoption,
            PaintCmd::ShowMain { .. } => current_view = CurrentView::Main,
            PaintCmd::ShowHalt { .. } => current_view = CurrentView::Halt,
            PaintCmd::ShowImage { .. } => current_view = CurrentView::Image,
            PaintCmd::ChromeChanged(kind) => {
                // Re-render the CURRENT view (not unconditionally
                // boot). Only the boot screen is currently chrome-
                // driven enough to benefit from a chrome-triggered
                // refresh — adoption / image / halt all carry their
                // own args through PaintCmd, so they need an
                // explicit ShowXxx to re-render. Once #97 phase 2
                // migrates those args into chrome too, this match
                // grows arms for each view.
                match current_view {
                    CurrentView::Boot => {
                        if matches!(kind, RefreshKind::Full) {
                            panel.force_full_next_refresh();
                        }
                        panel.redraw_boot_screen();
                        commit_view(panel, &mut last_painted_hash, "chrome-boot").await;
                    }
                    other => {
                        log::debug!(
                            "panel_actor: ChromeChanged({:?}) suppressed in {:?} view (re-render needs explicit ShowXxx)",
                            kind, other
                        );
                    }
                }
                mark_processed(seq);
                continue;
            }
            _ => {} // chrome-update cmds + countdown ticks don't change view
        }

        handle_cmd(panel, cmd, &mut last_painted_hash).await;
        // Publish completion AFTER the cmd is fully handled (compose +
        // refresh + hash dedup all done). Any `PaintHandle::await_processed`
        // for this seq now resolves on its next watch poll.
        mark_processed(seq);
    }
}

/// Dispatch a single paint command. Pulled out of the actor loop so
/// the borrow of `panel` lives only for the duration of the command,
/// and so it can be unit-tested against a `NoopPanel` if we ever
/// need it.
async fn handle_cmd(
    panel: &mut boards::Panel,
    cmd: PaintCmd,
    last_painted_hash: &mut Option<u64>,
) {
    match cmd {
        // ── Chrome / state updates (no immediate refresh) ──
        PaintCmd::UpdateChrome { battery_mv, rssi_dbm } => {
            panel.on_battery_sample(battery_mv);
            panel.on_wifi_state_changed(rssi_dbm);
        }
        PaintCmd::UpdateStatus(status) => {
            panel.set_status(status);
        }
        PaintCmd::UpdateIp(ip) => {
            panel.set_ip(ip.as_str());
        }
        PaintCmd::UpdateGateway(gw) => {
            // IpStr (heapless::String<24>) derefs to str — `as_deref`
            // on the Option yields the &str the trait wants without
            // any extra conversion.
            panel.set_gateway(gw.as_deref());
        }
        PaintCmd::UpdateDeviceId(id) => {
            panel.set_device_id(id.as_str());
        }
        PaintCmd::WifiDisconnected => {
            panel.on_wifi_state_changed(None);
            panel.set_wifi_link_state(paperanywhere_ports::WifiLinkState::Disconnected);
        }
        PaintCmd::UpdateWifiLinkState(state) => {
            panel.set_wifi_link_state(state);
        }
        PaintCmd::UpdateSsid(ssid) => {
            panel.set_ssid(ssid.as_deref());
        }
        PaintCmd::UpdateDeviceUuid(uuid) => {
            panel.set_device_uuid(uuid.as_deref());
        }
        PaintCmd::UpdateDeviceName(name) => {
            panel.set_device_name(name.as_deref());
        }

        // ── View transitions (full refresh via compositor's
        //    force_full_next_refresh; hash-dedup skips no-ops) ──
        //    RedrawBootScreen is treated as a transition-grade refresh:
        //    when the runtime sends it (post-DHCP, to surface real
        //    network info), the user expects a clear, unmistakable
        //    visual change. Fast-LUT updates of small text inside the
        //    Network column ("connecting..." → "10.0.1.229") looked
        //    too subtle to register as an update — the panel appeared
        //    "frozen" between the pre-DHCP and post-DHCP states. Force
        //    full-LUT here so the populated boot screen lands as a
        //    clean repaint. The subsequent UpdateBootCountdown ticks
        //    still use fast LUT (the layout doesn't shift; only the
        //    bottom row's digit changes).
        PaintCmd::RedrawBootScreen => {
            panel.force_full_next_refresh();
            panel.redraw_boot_screen();
            commit_view(panel, last_painted_hash, "boot").await;
        }
        PaintCmd::ShowAdoption {
            claim_code,
            device_id,
            ip,
            adopt_url,
            retry_notice,
        } => {
            panel.render_adoption_screen(
                claim_code.as_str(),
                device_id.as_str(),
                ip.as_str(),
                adopt_url.as_str(),
                retry_notice,
            );
            commit_view(panel, last_painted_hash, "adoption").await;
        }
        PaintCmd::ShowMain {
            ip,
            last_update,
            owner_email,
            project_name,
        } => {
            panel.render_main_placeholder(
                ip.as_str(),
                last_update.as_ref().map(|s| s.as_str()),
                owner_email.as_ref().map(|s| s.as_str()),
                project_name.as_ref().map(|s| s.as_str()),
            );
            commit_view(panel, last_painted_hash, "main").await;
        }
        PaintCmd::ShowHalt { headline, detail, code } => {
            panel.render_halt_screen(headline, detail, code);
            commit_view(panel, last_painted_hash, "halt").await;
        }
        PaintCmd::ShowImage { bytes, last_update } => {
            if let Some(stamp) = last_update {
                panel.set_last_update(stamp.as_str());
            }
            panel.write_chunk(&bytes).await;
            commit_view(panel, last_painted_hash, "image").await;
        }

        // ── Live OTA progress event ──
        PaintCmd::OtaProgress(phase) => {
            panel.render_ota_progress(phase);
            commit_view(panel, last_painted_hash, "ota-progress").await;
        }

        // ── Boot-screen hold countdown tick ──
        // Same view (boot template), just re-render with the new
        // countdown value. Fast LUT (commit_view default; the boot
        // template doesn't set force_full) — only the bottom row's
        // text pixels transition, the rest of the build-info block
        // and the logo stay put.
        PaintCmd::UpdateBootCountdown(value) => {
            panel.set_boot_countdown(value);
            panel.redraw_boot_screen();
            commit_view(panel, last_painted_hash, "boot-countdown").await;
        }

        // ChromeChanged is dispatched in the main loop (see comment
        // there) — it needs view-tracking state that lives on the
        // loop's stack frame, not here. handle_cmd is intentionally
        // not reached for ChromeChanged.
        PaintCmd::ChromeChanged(_) => {
            // unreachable — main loop continues before reaching here.
        }
    }
}

/// Compose + hash-check + refresh path shared across every
/// view-changing command. Skips the refresh when the framebuffer
/// hashes equal — the runtime's adoption-screen wake cycle hits
/// this case repeatedly because nothing in the framebuffer changes
/// between iterations.
async fn commit_view(panel: &mut boards::Panel, last_hash: &mut Option<u64>, label: &str) {
    panel.compose();
    let pending = panel.pending_hash();
    if pending.is_some() && pending == *last_hash {
        log::debug!("panel_actor: {} framebuffer unchanged — skipping refresh", label);
        return;
    }
    info!("panel_actor: refreshing for {}", label);
    panel.refresh().await;
    if let Some(h) = pending {
        *last_hash = Some(h);
    }
}

fn cmd_name(cmd: &PaintCmd) -> &'static str {
    match cmd {
        PaintCmd::RedrawBootScreen => "RedrawBootScreen",
        PaintCmd::ShowAdoption { .. } => "ShowAdoption",
        PaintCmd::ShowMain { .. } => "ShowMain",
        PaintCmd::ShowHalt { .. } => "ShowHalt",
        PaintCmd::ShowImage { .. } => "ShowImage",
        PaintCmd::UpdateChrome { .. } => "UpdateChrome",
        PaintCmd::UpdateStatus(_) => "UpdateStatus",
        PaintCmd::UpdateIp(_) => "UpdateIp",
        PaintCmd::UpdateGateway(_) => "UpdateGateway",
        PaintCmd::UpdateDeviceId(_) => "UpdateDeviceId",
        PaintCmd::WifiDisconnected => "WifiDisconnected",
        PaintCmd::UpdateWifiLinkState(_) => "UpdateWifiLinkState",
        PaintCmd::UpdateSsid(_) => "UpdateSsid",
        PaintCmd::UpdateDeviceUuid(_) => "UpdateDeviceUuid",
        PaintCmd::UpdateDeviceName(_) => "UpdateDeviceName",
        PaintCmd::UpdateBootCountdown(_) => "UpdateBootCountdown",
        PaintCmd::OtaProgress(_) => "OtaProgress",
        PaintCmd::ChromeChanged(_) => "ChromeChanged",
    }
}

/// Report which CPU core this task is currently executing on. Used
/// only in the startup info!() so we can confirm dual-core spawning
/// is actually working at runtime. On ESP32-S3, 0 = PRO_CPU, 1 = APP_CPU.
fn current_core_id() -> u32 {
    // esp-hal exposes the current Cpu directly; `as u32` maps
    // `Cpu::ProCpu` to 0 and `Cpu::AppCpu` to 1 via the `#[repr(C)]`
    // discriminants on the enum.
    esp_hal::system::Cpu::current() as u32
}
