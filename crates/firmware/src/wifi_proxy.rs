//! Cross-core WiFi proxy. Companion to `http_proxy`: lets the runtime
//! on core 1 use a `WifiLink` whose implementation actually lives on
//! core 0 with the embassy-net Stack + esp-radio controller.
//!
//! Same shape as `http_proxy`: request channel + per-kind reply
//! Signals for the async methods (`associate`); sync read methods
//! (`local_ip`, `rssi_dbm`, `gateway_v4`) snapshot from the global
//! chrome KV, which a small polling loop in [`wifi_proxy_task`]
//! keeps up to date from the actual stack.
//!
//! Reads-from-chrome means the runtime never blocks on a Signal for
//! these. Worst case is a slightly-stale RSSI / IP between proxy
//! polls (cadence: 200 ms). Good enough for chrome rendering and
//! for the runtime's "wait for DHCP" poll — that loop's already
//! 100 ms wide.

use alloc::string::String;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use paperanywhere_ports::{WifiCreds, WifiLink, chrome};

use crate::wifi::{FwWifi, WifiError};

pub enum WifiRequest {
    Associate(WifiCreds),
    Disconnect,
}

pub const REQUEST_CHANNEL_DEPTH: usize = 2;
pub static REQ_CHANNEL: Channel<CriticalSectionRawMutex, WifiRequest, REQUEST_CHANNEL_DEPTH> =
    Channel::new();

pub static ASSOCIATE_REPLY: Signal<CriticalSectionRawMutex, Result<(), WifiError>> = Signal::new();
pub static DISCONNECT_REPLY: Signal<CriticalSectionRawMutex, Result<(), WifiError>> =
    Signal::new();

/// Client-side `WifiLink` impl. Lives on core 1 in the runtime task.
/// Zero-sized.
pub struct WifiProxyClient;

impl WifiLink for WifiProxyClient {
    type Error = WifiError;

    async fn associate(&mut self, creds: &WifiCreds) -> Result<(), Self::Error> {
        REQ_CHANNEL
            .send(WifiRequest::Associate(creds.clone()))
            .await;
        ASSOCIATE_REPLY.wait().await
    }

    fn disconnect(&mut self) -> Result<(), Self::Error> {
        // disconnect is sync in the trait; we can't await a reply
        // here. Fire-and-forget: enqueue the request if the channel
        // has space, otherwise drop it. The runtime calls disconnect
        // at most once per wake before sleep, and a missed
        // disconnect just means the chip stays associated a bit
        // longer — not a correctness issue.
        let _ = REQ_CHANNEL.try_send(WifiRequest::Disconnect);
        Ok(())
    }

    fn rssi_dbm(&self) -> Option<i16> {
        chrome::snapshot().rssi_dbm
    }

    fn local_ip(&self) -> Option<[u8; 4]> {
        parse_ipv4(chrome::snapshot().ip.as_deref()?)
    }

    fn gateway_v4(&self) -> Option<[u8; 4]> {
        parse_ipv4(chrome::snapshot().gateway_v4.as_deref()?)
    }
}

fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut i = 0;
    for part in s.split('.') {
        if i >= 4 {
            return None;
        }
        out[i] = part.parse().ok()?;
        i += 1;
    }
    if i == 4 { Some(out) } else { None }
}

/// Core-0 task: owns `FwWifi` (which holds the !Send embassy-net
/// Stack reference). Pumps the request channel for `Associate` /
/// `Disconnect`, and periodically polls the wifi state into the
/// global chrome KV so the runtime's sync read methods on
/// `WifiProxyClient` always see a recent value.
///
/// The poll cadence is 200 ms — fast enough for the runtime's DHCP
/// wait loop (100 ms iterations), slow enough that we're not
/// burning core 0's executor on chrome updates.
#[embassy_executor::task]
pub async fn wifi_proxy_task(wifi: &'static mut FwWifi) -> ! {
    log::info!("wifi_proxy: starting on core 0");
    loop {
        // Race the request channel against the periodic poll. Whichever
        // fires first runs; the other resumes on the next iteration.
        let timer = Timer::after(Duration::from_millis(200));
        let req = REQ_CHANNEL.receive();
        match embassy_futures::select::select(req, timer).await {
            embassy_futures::select::Either::First(WifiRequest::Associate(creds)) => {
                let result = wifi.associate(&creds).await;
                publish_state(wifi);
                ASSOCIATE_REPLY.signal(result);
            }
            embassy_futures::select::Either::First(WifiRequest::Disconnect) => {
                let result = wifi.disconnect();
                publish_state(wifi);
                DISCONNECT_REPLY.signal(result);
            }
            embassy_futures::select::Either::Second(_) => {
                publish_state(wifi);
            }
        }
    }
}

/// Push `wifi.{local_ip,rssi_dbm,gateway_v4}` into chrome so the
/// runtime's WifiProxyClient sees up-to-date values via
/// `chrome::snapshot()`.
fn publish_state(wifi: &FwWifi) {
    let ip = wifi.local_ip().map(format_ipv4);
    let gw = wifi.gateway_v4().map(format_ipv4);
    let rssi = wifi.rssi_dbm();
    // Batch the three writes under a single critical section so the
    // panel actor's snapshot doesn't catch a half-updated chrome
    // state. None of these auto-invalidate from inside with_mut —
    // we fire one invalidate at the end if anything changed.
    let changed = chrome::with_mut(|s| {
        let mut any = false;
        if s.rssi_dbm != rssi {
            s.rssi_dbm = rssi;
            any = true;
        }
        // ip + gw are stored as heapless strings in chrome; rebuild
        // from the [u8; 4] each tick is cheap (stack-allocated).
        let new_ip = ip.as_deref();
        let cur_ip = s.ip.as_deref();
        if cur_ip != new_ip {
            s.ip = new_ip.map(hstring_from_str);
            any = true;
        }
        let new_gw = gw.as_deref();
        let cur_gw = s.gateway_v4.as_deref();
        if cur_gw != new_gw {
            s.gateway_v4 = new_gw.map(hstring_from_str);
            any = true;
        }
        any
    });
    if changed {
        chrome::invalidate(chrome::RefreshKind::Fast);
    }
}

fn format_ipv4(ip: [u8; 4]) -> String {
    alloc::format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

fn hstring_from_str<const N: usize>(s: &str) -> heapless::String<N> {
    let mut h: heapless::String<N> = heapless::String::new();
    let cap = s.len().min(N);
    let safe_end = s
        .char_indices()
        .take_while(|(i, ch)| i + ch.len_utf8() <= cap)
        .last()
        .map(|(i, ch)| i + ch.len_utf8())
        .unwrap_or(0);
    let _ = h.push_str(&s[..safe_end]);
    h
}
