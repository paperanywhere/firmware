//! Cross-core HTTP proxy. Core 1 produces requests; core 0 owns the
//! `FwHttp` + embassy-net `Stack` and executes them.
//!
//! ## Why this exists
//!
//! `embassy_net::Stack` is `!Send` (PhantomData<*const ()>), so its
//! runner task + every task that builds a `TcpSocket` against it must
//! share an embassy executor. esp-radio's own tasks must also live on
//! core 0 (esp-wifi-sys#412 crashes if its task migrates). That pins
//! the WHOLE network stack to a single executor on core 0.
//!
//! Until this module landed, the runtime polling loop (+ HTTP client)
//! shared that executor — every chrome::set_*, every paint dispatch,
//! every JSON parse competed with `net_task` for CPU time. The
//! observed symptom was first-register taking ~90 s on a fresh boot:
//! the runtime fanout (chrome dirty signal → multiple paint submits
//! → post-DHCP countdown → HTTP request setup) starved net_task long
//! enough that TCP handshake retransmissions piled up.
//!
//! With this proxy:
//!
//!   * Core 0 executor runs ONLY `net_task` + [`http_proxy_task`].
//!     The proxy task holds `FwHttp` and waits on [`REQ_CHANNEL`];
//!     when a request arrives it calls the underlying `FwHttp` method
//!     and signals the matching reply slot.
//!   * Core 1 executor runs `runtime_task` (uses [`HttpProxyClient`]
//!     as its [`HttpTransport`]) + `panel_actor_task`. Cross-core sync
//!     is the existing critical_section primitive.
//!
//! Result: net_task always has core 0's executor pinned to it, the
//! runtime can't starve it no matter how heavy a wake-cycle gets.
//!
//! ## Reply-slot pattern
//!
//! Each request kind has its own `&'static Signal<…>` reply slot. The
//! runtime is single-threaded relative to itself, so request kinds
//! never overlap (one register at a time, one get_state at a time,
//! etc.). For the rare case where two kinds run concurrently the
//! distinct slots keep them from interleaving.
//!
//! If we ever need concurrent same-kind requests we'd switch to a
//! request-id correlator + a shared reply channel. Today's wake-cycle
//! shape doesn't need that — KISS.
//!
//! ## stream_blob
//!
//! Returns the full image as a `Vec<u8>` over the reply slot. Worst
//! case for our supported panels is ~200 KB (13.3" Color7); the
//! PSRAM heap is 8 MB so this is well-budgeted. A chunked-channel
//! version that lets the runtime pipe bytes straight to the panel
//! without buffering is a follow-up — the current callers all
//! collect into a Vec on the runtime side anyway, so the extra
//! buffering doesn't change memory footprint.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use paperanywhere_ports::{
    DeviceAck, DeviceIdentity, DeviceRegistration, DeviceState, HttpTransport,
};

use crate::http::{FwHttp, HttpError};

/// Owned request payload — the actor task holds these by value while
/// it processes them. Each variant carries the input data needed for
/// the corresponding `HttpTransport` method.
pub enum HttpRequest {
    Register(DeviceIdentity),
    GetState {
        token: String,
    },
    PostAck {
        token: String,
        ack: DeviceAck,
    },
    StreamBlob {
        token: String,
        blob_url: String,
    },
}

/// Bounded request queue. 4 slots is way past anything the runtime
/// emits — at most we have a register + state + ack queued back-to-
/// back, which is 3 requests. 4 leaves room for a future heartbeat
/// or claim-code refresh without resizing.
pub const REQUEST_CHANNEL_DEPTH: usize = 4;
pub static REQ_CHANNEL: Channel<CriticalSectionRawMutex, HttpRequest, REQUEST_CHANNEL_DEPTH> =
    Channel::new();

/// One reply slot per request kind. Each is a `Signal` that the proxy
/// fills with the call's `Result` after dispatching. The runtime
/// awaits the matching slot.
pub static REGISTER_REPLY: Signal<CriticalSectionRawMutex, Result<DeviceRegistration, HttpError>> =
    Signal::new();
pub static GET_STATE_REPLY: Signal<CriticalSectionRawMutex, Result<DeviceState, HttpError>> =
    Signal::new();
pub static POST_ACK_REPLY: Signal<CriticalSectionRawMutex, Result<(), HttpError>> = Signal::new();
pub static STREAM_BLOB_REPLY: Signal<CriticalSectionRawMutex, Result<Vec<u8>, HttpError>> =
    Signal::new();

/// Client-side HttpTransport implementation. Lives on core 1 in the
/// runtime task. Each method enqueues a request to [`REQ_CHANNEL`]
/// and awaits the matching reply slot.
///
/// Zero-sized: holds no state. The actual `FwHttp` instance lives on
/// core 0 inside [`http_proxy_task`].
pub struct HttpProxyClient;

impl HttpTransport for HttpProxyClient {
    type Error = HttpError;

    async fn register(
        &mut self,
        identity: &DeviceIdentity,
    ) -> Result<DeviceRegistration, Self::Error> {
        REQ_CHANNEL
            .send(HttpRequest::Register(identity.clone()))
            .await;
        REGISTER_REPLY.wait().await
    }

    async fn get_state(&mut self, token: &str) -> Result<DeviceState, Self::Error> {
        REQ_CHANNEL
            .send(HttpRequest::GetState { token: token.to_string() })
            .await;
        GET_STATE_REPLY.wait().await
    }

    async fn stream_blob(
        &mut self,
        token: &str,
        blob_url: &str,
        on_chunk: &mut (dyn FnMut(&[u8]) -> Result<(), ()> + Send),
    ) -> Result<(), Self::Error> {
        REQ_CHANNEL
            .send(HttpRequest::StreamBlob {
                token: token.to_string(),
                blob_url: blob_url.to_string(),
            })
            .await;
        let bytes = STREAM_BLOB_REPLY.wait().await?;
        // Replay the buffered blob through the caller's chunk callback
        // in 4 KB pieces so the caller's panel-write path doesn't get
        // one giant slice it has to internally re-chunk. Matches what
        // the direct HTTP impl would have produced via TCP reads.
        for chunk in bytes.chunks(4096) {
            on_chunk(chunk).map_err(|_| HttpError::SocketIo)?;
        }
        Ok(())
    }

    async fn post_ack(&mut self, token: &str, ack: &DeviceAck) -> Result<(), Self::Error> {
        REQ_CHANNEL
            .send(HttpRequest::PostAck {
                token: token.to_string(),
                ack: ack.clone(),
            })
            .await;
        POST_ACK_REPLY.wait().await
    }
}

/// Core-0 task: owns `FwHttp` + the embassy-net `Stack`, drains
/// [`REQ_CHANNEL`], dispatches to the matching `FwHttp` method,
/// and signals the corresponding reply slot.
///
/// The `&'static mut FwHttp` is owned for the lifetime of the task.
/// embassy-net's Stack is `!Send`, but this task lives on the same
/// executor as `net_task` (both pinned to core 0), so the !Send
/// constraint is satisfied — the future never moves cores.
#[embassy_executor::task]
pub async fn http_proxy_task(http: &'static mut FwHttp) -> ! {
    log::info!("http_proxy: starting on core 0");
    loop {
        let req = REQ_CHANNEL.receive().await;
        match req {
            HttpRequest::Register(identity) => {
                let result = http.register(&identity).await;
                REGISTER_REPLY.signal(result);
            }
            HttpRequest::GetState { token } => {
                let result = http.get_state(&token).await;
                GET_STATE_REPLY.signal(result);
            }
            HttpRequest::PostAck { token, ack } => {
                let result = http.post_ack(&token, &ack).await;
                POST_ACK_REPLY.signal(result);
            }
            HttpRequest::StreamBlob { token, blob_url } => {
                // Collect into a Vec on this side, then ship it across
                // the reply slot. See module-level docs on the
                // buffered-vs-streaming tradeoff.
                let mut buf: Vec<u8> = Vec::new();
                let result = http
                    .stream_blob(&token, &blob_url, &mut |chunk| {
                        buf.extend_from_slice(chunk);
                        Ok(())
                    })
                    .await
                    .map(|_| buf);
                STREAM_BLOB_REPLY.signal(result);
            }
        }
    }
}
