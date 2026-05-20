//! HTTP/HTTPS client over `embassy_net::tcp::TcpSocket`, with optional TLS
//! wrap via `embedded-tls`. Hand-rolled HTTP/1.1: one connection per call,
//! `Authorization: Bearer <device_token>` on every request.
//!
//! ## TLS
//!
//! When the backend URL parses as `https://`, the TCP socket is wrapped in
//! an `embedded_tls::TlsConnection` before any HTTP bytes flow. The handshake
//! uses ECDHE + AES-128-GCM-SHA256, seeded by the chip's hardware RNG via
//! `esp_hal::rng::Rng` (we add a CryptoRng marker because the on-die TRNG is
//! cryptographically suitable — the esp-hal type itself doesn't carry the
//! marker because it predates the `rand_core` 0.6 crypto-rng split).
//!
//! Server certs are currently **not verified** — `embedded-tls`'s
//! `UnsecureProvider` accepts any cert the server presents. That's fine for
//! talking to a private backend on a trusted LAN, *not* for production. A
//! follow-up will land cert pinning by feeding a small CA bundle into
//! `TlsConfig::with_ca`.
//!
//! ## Buffers
//!
//! TLS record buffers are 16 KB each (the spec maximum). They live as fields
//! on `FwHttp`, which itself lives in a `StaticCell` — so they end up in
//! `.bss`, not on the task stack (which is only 8 KB) and not on the heap
//! (where they'd fragment after a few hundred calls). Total static footprint
//! for the HTTP client is ~38 KB.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use embassy_net::Stack;
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
// HTTP exchange helpers are written against embedded-io 0.7 traits because
// that's what `embedded-tls` exposes on its `TlsConnection`. Raw TCP sockets
// expose 0.6 traits via embassy-net; the `Eio0607` adapter below wraps the
// socket so both transports satisfy a single shared bound.
use embedded_io_07::{Error as EioError07, ErrorKind as EioErrorKind07, ErrorType as EioErrorType07};
use embedded_io_async_07::{Read, Write};
use embedded_tls::{Aes128GcmSha256, TlsConfig, TlsConnection, TlsContext, UnsecureProvider};
use paperanywhere_ports::{DeviceAck, DeviceState, HttpTransport, parse_device_state};

const TCP_RX_BUF: usize = 4 * 1024;
const TCP_TX_BUF: usize = 1 * 1024;
const TLS_RECORD_BUF: usize = 16 * 1024;

#[allow(dead_code)]
#[derive(Debug)]
pub enum HttpError {
    UrlParse,
    DnsResolve,
    SocketConnect,
    SocketIo,
    Tls,
    BadStatus(u16),
    HeaderParse,
    ResponseTooLarge,
    BodyDecode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Http,
    Https,
}

pub struct FwHttp {
    stack: &'static Stack<'static>,
    host: String,
    port: u16,
    scheme: Scheme,
    tcp_rx_buf: [u8; TCP_RX_BUF],
    tcp_tx_buf: [u8; TCP_TX_BUF],
    tls_read_buf: [u8; TLS_RECORD_BUF],
    tls_write_buf: [u8; TLS_RECORD_BUF],
}

impl FwHttp {
    /// Construct from the device's stored backend URL. Accepts
    /// `http://host[:port]` and `https://host[:port]`. Falls back to a dev
    /// default if the URL doesn't parse (mostly to keep first-boot reasonable
    /// before NVS gets provisioned).
    pub fn new(stack: &'static Stack<'static>, backend_url: Option<&str>) -> Self {
        let raw = backend_url.unwrap_or(DEFAULT_BACKEND_URL);
        let (scheme, host, port) = parse_backend_url(raw).unwrap_or_else(|| {
            esp_println::println!(
                "http: backend_url {:?} unparseable, falling back to default",
                raw
            );
            parse_backend_url(DEFAULT_BACKEND_URL).expect("default backend URL is valid")
        });
        Self {
            stack,
            host,
            port,
            scheme,
            tcp_rx_buf: [0; TCP_RX_BUF],
            tcp_tx_buf: [0; TCP_TX_BUF],
            tls_read_buf: [0; TLS_RECORD_BUF],
            tls_write_buf: [0; TLS_RECORD_BUF],
        }
    }
}

/// Fallback for when NVS has no backend_url yet — points at the dev backend
/// on the local LAN. Production firmware should always have NVS populated
/// by the provisioning step.
const DEFAULT_BACKEND_URL: &str = "http://192.168.1.100:8080";

fn parse_backend_url(url: &str) -> Option<(Scheme, String, u16)> {
    let (scheme, rest, default_port) = if let Some(r) = url.strip_prefix("https://") {
        (Scheme::Https, r, 443u16)
    } else if let Some(r) = url.strip_prefix("http://") {
        (Scheme::Http, r, 80u16)
    } else {
        return None;
    };
    let host_port = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match host_port.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (host_port.to_string(), default_port),
    };
    Some((scheme, host, port))
}

impl HttpTransport for FwHttp {
    type Error = HttpError;

    async fn get_state(&mut self, token: &str) -> Result<DeviceState, Self::Error> {
        let mut response = Vec::with_capacity(2048);
        request_with_full_body(
            self,
            "GET",
            token,
            None,
            "application/json",
            "/api/device/state",
            &mut response,
        )
        .await?;
        parse_device_state(core::str::from_utf8(&response).map_err(|_| HttpError::BodyDecode)?)
            .ok_or(HttpError::BodyDecode)
    }

    async fn stream_blob(
        &mut self,
        token: &str,
        blob_url: &str,
        on_chunk: &mut (dyn FnMut(&[u8]) -> Result<(), ()> + Send),
    ) -> Result<(), Self::Error> {
        request_with_streamed_body(self, "GET", token, blob_url, on_chunk).await
    }

    async fn post_ack(&mut self, token: &str, ack: &DeviceAck) -> Result<(), Self::Error> {
        let body = ack.to_json();
        let mut sink = Vec::new();
        request_with_full_body(
            self,
            "POST",
            token,
            Some(body.as_bytes()),
            "application/json",
            "/api/device/ack",
            &mut sink,
        )
        .await
    }
}

// ── Outer wrappers: open the socket, branch on scheme, dispatch to inner ──
//
// We disjointly borrow the buffer fields of `FwHttp` so the compiler is happy
// to have the socket + (optionally) the TLS connection alive simultaneously,
// both holding `&mut` to different parts of `self`.

#[allow(clippy::too_many_arguments)]
async fn request_with_full_body(
    http: &mut FwHttp,
    method: &str,
    token: &str,
    body: Option<&[u8]>,
    content_type: &str,
    path: &str,
    response: &mut Vec<u8>,
) -> Result<(), HttpError> {
    // DHCP runs in the background as soon as `wifi.associate` brings the
    // link up. The first /state call after a fresh wake hits this before
    // an IP is assigned; the call resolves immediately on subsequent ones.
    http.stack.wait_config_up().await;
    let addr = resolve(http.stack, &http.host).await?;
    // Disjoint borrows — each &mut points at a different field of `http`.
    let FwHttp {
        stack,
        host,
        port,
        scheme,
        tcp_rx_buf,
        tcp_tx_buf,
        tls_read_buf,
        tls_write_buf,
    } = http;
    let mut socket = TcpSocket::new(**stack, &mut tcp_rx_buf[..], &mut tcp_tx_buf[..]);
    socket.connect((addr, *port)).await.map_err(|_| HttpError::SocketConnect)?;

    match scheme {
        Scheme::Http => {
            let mut adapted = Eio0607(socket);
            http_exchange_full(
                &mut adapted, host, method, path, token, content_type, body, response,
            )
            .await?;
            adapted.0.close();
        }
        Scheme::Https => {
            let mut tls = TlsConnection::<_, Aes128GcmSha256>::new(
                Eio0607(socket),
                &mut tls_read_buf[..],
                &mut tls_write_buf[..],
            );
            tls_handshake(&mut tls, host).await?;
            http_exchange_full(
                &mut tls, host, method, path, token, content_type, body, response,
            )
            .await?;
            // Drop closes both TLS and the underlying socket.
        }
    }
    Ok(())
}

async fn request_with_streamed_body(
    http: &mut FwHttp,
    method: &str,
    token: &str,
    path: &str,
    on_chunk: &mut (dyn FnMut(&[u8]) -> Result<(), ()> + Send),
) -> Result<(), HttpError> {
    http.stack.wait_config_up().await;
    let addr = resolve(http.stack, &http.host).await?;
    let FwHttp {
        stack,
        host,
        port,
        scheme,
        tcp_rx_buf,
        tcp_tx_buf,
        tls_read_buf,
        tls_write_buf,
    } = http;
    let mut socket = TcpSocket::new(**stack, &mut tcp_rx_buf[..], &mut tcp_tx_buf[..]);
    socket.connect((addr, *port)).await.map_err(|_| HttpError::SocketConnect)?;

    match scheme {
        Scheme::Http => {
            let mut adapted = Eio0607(socket);
            http_exchange_stream(&mut adapted, host, method, path, token, on_chunk).await?;
            adapted.0.close();
        }
        Scheme::Https => {
            let mut tls = TlsConnection::<_, Aes128GcmSha256>::new(
                Eio0607(socket),
                &mut tls_read_buf[..],
                &mut tls_write_buf[..],
            );
            tls_handshake(&mut tls, host).await?;
            http_exchange_stream(&mut tls, host, method, path, token, on_chunk).await?;
        }
    }
    Ok(())
}

async fn tls_handshake<S>(
    tls: &mut TlsConnection<'_, S, Aes128GcmSha256>,
    host: &str,
) -> Result<(), HttpError>
where
    S: Read + Write,
{
    let config = TlsConfig::new().with_server_name(host).enable_rsa_signatures();
    let rng = EspRng(esp_hal::rng::Rng::new());
    // `UnsecureProvider::new::<CipherSuite>` is the constructor; the
    // turbofish goes on `new`, not on the struct (it's how the type spec
    // for `CipherSuite` is provided since the no-arg version of the struct
    // only carries the RNG).
    let provider = UnsecureProvider::<(), _>::new::<Aes128GcmSha256>(rng);
    let ctx = TlsContext::new(&config, provider);
    tls.open(ctx).await.map_err(|e| {
        esp_println::println!("tls: handshake failed: {:?}", e);
        HttpError::Tls
    })
}

// ── Inner protocol helpers: work on any AsyncRead + AsyncWrite transport ──

#[allow(clippy::too_many_arguments)]
async fn http_exchange_full<T>(
    transport: &mut T,
    host: &str,
    method: &str,
    path: &str,
    token: &str,
    content_type: &str,
    body: Option<&[u8]>,
    response: &mut Vec<u8>,
) -> Result<(), HttpError>
where
    T: Read + Write,
{
    write_request_head(transport, method, host, path, token, content_type, body).await?;
    if let Some(b) = body {
        transport.write_all(b).await.map_err(|_| HttpError::SocketIo)?;
    }
    transport.flush().await.map_err(|_| HttpError::SocketIo)?;
    let status = read_response_head(transport, response).await?;
    if !(200..300).contains(&status) {
        return Err(HttpError::BadStatus(status));
    }
    let mut chunk = [0u8; 1024];
    loop {
        match transport.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    Ok(())
}

async fn http_exchange_stream<T>(
    transport: &mut T,
    host: &str,
    method: &str,
    path: &str,
    token: &str,
    on_chunk: &mut (dyn FnMut(&[u8]) -> Result<(), ()> + Send),
) -> Result<(), HttpError>
where
    T: Read + Write,
{
    write_request_head(transport, method, host, path, token, "application/octet-stream", None)
        .await?;
    transport.flush().await.map_err(|_| HttpError::SocketIo)?;
    let mut head_carry = Vec::with_capacity(512);
    let status = read_response_head(transport, &mut head_carry).await?;
    if !(200..300).contains(&status) {
        return Err(HttpError::BadStatus(status));
    }
    if !head_carry.is_empty() {
        on_chunk(&head_carry).map_err(|_| HttpError::SocketIo)?;
    }
    let mut buf = [0u8; 1024];
    loop {
        match transport.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => on_chunk(&buf[..n]).map_err(|_| HttpError::SocketIo)?,
            Err(_) => break,
        }
    }
    Ok(())
}

async fn write_request_head<T>(
    transport: &mut T,
    method: &str,
    host: &str,
    path: &str,
    token: &str,
    content_type: &str,
    body: Option<&[u8]>,
) -> Result<(), HttpError>
where
    T: Write,
{
    let content_length = body.map(|b| b.len()).unwrap_or(0);
    // The FW version stamp is reported on every request so the backend's
    // /state handler can decide whether to attach a firmware_update offer
    // without needing a separate registration round-trip. `User-Agent`
    // duplicates it for log/metrics consumers that don't read custom
    // headers.
    let fw_version = crate::FW_VERSION;
    let head = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Authorization: Bearer {token}\r\n\
         User-Agent: paperanywhere-firmware/{fw_version}\r\n\
         X-PA-FW-Version: {fw_version}\r\n\
         Accept: application/json\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {content_length}\r\n\
         Connection: close\r\n\
         \r\n"
    );
    transport
        .write_all(head.as_bytes())
        .await
        .map_err(|_| HttpError::SocketIo)
}

async fn read_response_head<T>(transport: &mut T, carry: &mut Vec<u8>) -> Result<u16, HttpError>
where
    T: Read,
{
    let mut head = Vec::with_capacity(512);
    let mut buf = [0u8; 256];
    let mut header_end = None;
    while header_end.is_none() {
        let n = transport
            .read(&mut buf)
            .await
            .map_err(|_| HttpError::SocketIo)?;
        if n == 0 {
            return Err(HttpError::HeaderParse);
        }
        head.extend_from_slice(&buf[..n]);
        if let Some(pos) = find_double_crlf(&head) {
            header_end = Some(pos);
        }
        if head.len() > 8 * 1024 {
            return Err(HttpError::ResponseTooLarge);
        }
    }
    let split = header_end.unwrap();
    let headers_bytes = &head[..split];
    if split + 4 < head.len() {
        carry.extend_from_slice(&head[split + 4..]);
    }
    let headers_str = core::str::from_utf8(headers_bytes).map_err(|_| HttpError::HeaderParse)?;
    let status_line = headers_str.lines().next().ok_or(HttpError::HeaderParse)?;
    let mut parts = status_line.split_whitespace();
    let _ = parts.next(); // HTTP/1.1
    let status: u16 = parts
        .next()
        .ok_or(HttpError::HeaderParse)?
        .parse()
        .map_err(|_| HttpError::HeaderParse)?;
    Ok(status)
}

fn find_double_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|w| w == b"\r\n\r\n")
}

async fn resolve(stack: &Stack<'static>, host: &str) -> Result<embassy_net::IpAddress, HttpError> {
    // IP literal short-circuit — common for backends on the local LAN.
    if let Ok(addr) = host.parse::<embassy_net::Ipv4Address>() {
        return Ok(addr.into());
    }
    let results = stack
        .dns_query(host, DnsQueryType::A)
        .await
        .map_err(|_| HttpError::DnsResolve)?;
    results.first().copied().ok_or(HttpError::DnsResolve)
}

// ── embedded-io 0.6 → 0.7 bridge ──
//
// embassy-net's TcpSocket implements `embedded_io_async::Read` + `Write` at
// version 0.6. embedded-tls's TlsConnection expects them at version 0.7.
// Both trait families are semantically identical — same method names, same
// shapes — they just don't unify because the crates went through a major
// bump. This newtype re-exposes the 0.6 traits as 0.7 traits.

struct Eio0607<S>(S);

#[derive(Debug)]
struct Eio0607Error<E>(E);

impl<E: core::fmt::Debug> core::fmt::Display for Eio0607Error<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

// `embedded_io@0.7::Error` requires `core::error::Error` (a no_std trait that
// exists in core since 1.81). The blanket impl below gives our wrapper the
// trait it needs via Debug + Display.
impl<E: core::fmt::Debug> core::error::Error for Eio0607Error<E> {}

impl<E: embedded_io::Error> EioError07 for Eio0607Error<E> {
    fn kind(&self) -> EioErrorKind07 {
        // ErrorKind enums between 0.6 and 0.7 have the same variants; the
        // simple-and-loose path is to round-trip through the catch-all,
        // which loses fidelity but never lies. Per-variant remap is a
        // mechanical follow-up if we ever surface these in dashboards.
        match self.0.kind() {
            embedded_io::ErrorKind::NotFound => EioErrorKind07::NotFound,
            embedded_io::ErrorKind::PermissionDenied => EioErrorKind07::PermissionDenied,
            embedded_io::ErrorKind::ConnectionRefused => EioErrorKind07::ConnectionRefused,
            embedded_io::ErrorKind::ConnectionReset => EioErrorKind07::ConnectionReset,
            embedded_io::ErrorKind::ConnectionAborted => EioErrorKind07::ConnectionAborted,
            embedded_io::ErrorKind::NotConnected => EioErrorKind07::NotConnected,
            embedded_io::ErrorKind::AddrInUse => EioErrorKind07::AddrInUse,
            embedded_io::ErrorKind::AddrNotAvailable => EioErrorKind07::AddrNotAvailable,
            embedded_io::ErrorKind::BrokenPipe => EioErrorKind07::BrokenPipe,
            embedded_io::ErrorKind::AlreadyExists => EioErrorKind07::AlreadyExists,
            embedded_io::ErrorKind::InvalidInput => EioErrorKind07::InvalidInput,
            embedded_io::ErrorKind::InvalidData => EioErrorKind07::InvalidData,
            embedded_io::ErrorKind::TimedOut => EioErrorKind07::TimedOut,
            embedded_io::ErrorKind::Interrupted => EioErrorKind07::Interrupted,
            embedded_io::ErrorKind::Unsupported => EioErrorKind07::Unsupported,
            embedded_io::ErrorKind::OutOfMemory => EioErrorKind07::OutOfMemory,
            embedded_io::ErrorKind::WriteZero => EioErrorKind07::WriteZero,
            _ => EioErrorKind07::Other,
        }
    }
}

impl<S> EioErrorType07 for Eio0607<S>
where
    S: embedded_io::ErrorType,
{
    type Error = Eio0607Error<S::Error>;
}

impl<S> Read for Eio0607<S>
where
    S: embedded_io_async::Read,
{
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.0.read(buf).await.map_err(Eio0607Error)
    }
}

impl<S> Write for Eio0607<S>
where
    S: embedded_io_async::Write,
{
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.0.write(buf).await.map_err(Eio0607Error)
    }
    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.0.flush().await.map_err(Eio0607Error)
    }
}

// ── RNG adapter ──
//
// esp-hal's `Rng` already impls `rand_core::RngCore` (under the `unstable`
// feature, which we have on). It doesn't carry the `CryptoRng` marker
// because esp-hal predates the rand_core 0.6 crypto-rng split — we add the
// marker on a local newtype since the on-die TRNG is cryptographically
// suitable per the chip docs.

struct EspRng(esp_hal::rng::Rng);

impl rand_core::RngCore for EspRng {
    fn next_u32(&mut self) -> u32 {
        rand_core::RngCore::next_u32(&mut self.0)
    }
    fn next_u64(&mut self) -> u64 {
        rand_core::RngCore::next_u64(&mut self.0)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        rand_core::RngCore::fill_bytes(&mut self.0, dest)
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        rand_core::RngCore::try_fill_bytes(&mut self.0, dest)
    }
}

impl rand_core::CryptoRng for EspRng {}
