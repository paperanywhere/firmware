//! Minimal WebSocket client over TLS. Connects to `/api/device/ws?token=…`.
//! Pumps `ServerMsg` / `DeviceMsg` round-trips via the shared `paperanywhere-proto`
//! types so the wire format never diverges from the backend.

use paperanywhere_proto::{DeviceMsg, ServerMsg};

pub struct WsConfig<'a> {
    pub host: &'a str,
    pub port: u16,
    pub path: &'a str,
    pub device_token: &'a str,
}

pub struct Connection {
    // M4: holds embedded-tls + embedded-websocket session state.
    _phantom: core::marker::PhantomData<()>,
}

impl Connection {
    pub async fn open(_cfg: WsConfig<'_>) -> Result<Self, ()> {
        // M4 — set up TLS over TCP, perform HTTP Upgrade, then WS framing.
        Err(())
    }

    pub async fn send(&mut self, _msg: &DeviceMsg) -> Result<(), ()> {
        Err(())
    }

    pub async fn recv(&mut self) -> Result<ServerMsg, ()> {
        Err(())
    }
}
