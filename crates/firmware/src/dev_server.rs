//! Dev-only HTTP server bound to the device's WiFi IP.
//!
//! Spawned as an embassy task by `boot::run` when
//! `nvs.load_is_dev_build()` is true. Listens on port 80 and handles
//! a tiny REST-y surface:
//!
//!   - `GET /info` — JSON: { fw_version, build_time, board, channel, ip,
//!     mac, has_battery, panel_w, panel_h }. Used by `pa-dev info`.
//!   - `PUT /firmware` — body is the new flashable .bin. Verified
//!     against `X-PA-Sha256` (lowercase hex). On success the device
//!     activates the new slot and software-resets.
//!
//! Auth: none. Assumes LAN trust on the dev channel. A future iteration
//! could require a shared secret from NVS, but for now the gate is the
//! is_dev_build NVS flag — production firmware doesn't include this
//! module at all (it's spawned only on dev builds).
//!
//! Implementation notes:
//!   - One concurrent connection. Embassy is single-task on core 0;
//!     accepting a second connection would need either a multi-socket
//!     pool or a queue. Dev iteration doesn't need parallelism.
//!   - The HTTP parser is hand-rolled and only understands what the
//!     CLI sends. No chunked encoding, no Connection: keep-alive, no
//!     query strings. The request line + headers must fit in 1 KB.

use alloc::format;
use alloc::string::{String, ToString};
use core::str;

use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;
use embedded_storage::Storage;
use esp_bootloader_esp_idf::{
    ota::OtaImageState, ota_updater::OtaUpdater, partitions::PARTITION_TABLE_MAX_LEN,
};
use esp_hal::peripherals::FLASH;
use esp_storage::FlashStorage;
use log::{info, warn};
use sha2::{Digest, Sha256};

/// Sized for one HTTP request's request-line + headers. The CLI's PUT
/// /firmware is roughly 200 bytes of headers; the GET /info request is
/// ~80 bytes. 1 KB has comfortable headroom.
const HEADER_BUF_LEN: usize = 1024;

/// Flash-sector buffer for the OTA write path. The bare panel driver
/// also uses 4 KB; matching it keeps the erase pattern aligned.
const SECTOR_SIZE: usize = 4096;

/// Network-buffer sizes for the listening TcpSocket. Tight for HTTP/1.1
/// over a LAN; could grow for larger firmware payloads. The PUT body
/// is streamed, not buffered, so even a 1 MB upload fits in 8 KB rx.
const TCP_RX: usize = 8 * 1024;
const TCP_TX: usize = 2 * 1024;

/// Information the /info endpoint surfaces. Built once at boot since
/// most fields are static; refreshed in-task for the dynamic ones (ip).
pub struct DevServerCtx {
    pub fw_version: &'static str,
    pub build_time: &'static str,
    pub board_slug: &'static str,
    pub panel_width_px: u32,
    pub panel_height_px: u32,
    pub mac: [u8; 6],
}

/// Embassy task: serve HTTP forever. Bound to the wifi stack — when
/// the stack hasn't acquired a DHCP lease yet, `accept` simply blocks
/// until it does. Re-spawned only via chip reboot.
#[embassy_executor::task]
pub async fn run(stack: &'static Stack<'static>, ctx: &'static DevServerCtx) -> ! {
    static_assertions::const_assert!(HEADER_BUF_LEN >= 256);
    info!(
        "dev_server: listening on :80 (board={}, fw={})",
        ctx.board_slug, ctx.fw_version
    );

    let mut rx = [0u8; TCP_RX];
    let mut tx = [0u8; TCP_TX];
    loop {
        let mut socket = TcpSocket::new(*stack, &mut rx, &mut tx);
        socket.set_timeout(Some(Duration::from_secs(30)));
        if let Err(e) = socket.accept(80).await {
            warn!("dev_server: accept failed: {:?}", e);
            Timer::after(Duration::from_millis(500)).await;
            continue;
        }
        info!(
            "dev_server: connection from {:?}",
            socket.remote_endpoint()
        );

        if let Err(e) = handle(&mut socket, ctx).await {
            warn!("dev_server: handler error: {:?}", e);
        }

        // Polite half-close + small grace so RST doesn't race the
        // client reading our response.
        socket.close();
        Timer::after(Duration::from_millis(200)).await;
        socket.abort();
        let _ = socket.flush().await;
    }
}

#[derive(Debug)]
enum ServerError {
    Io,
    BadRequest,
    NotFound,
    HashMismatch,
    SizeMismatch,
    Flash,
    Bootloader,
    Unauthorized,
}

async fn handle(socket: &mut TcpSocket<'_>, ctx: &'static DevServerCtx) -> Result<(), ServerError> {
    let mut header_buf = [0u8; HEADER_BUF_LEN];
    let header_end = read_headers(socket, &mut header_buf).await?;
    let req = parse_request(&header_buf[..header_end])?;

    match (req.method, req.path) {
        ("GET", "/info") => respond_info(socket, ctx).await,
        ("PUT", "/firmware") => handle_firmware_put(socket, ctx, &req).await,
        _ => respond_status(socket, 404, "not found").await,
    }
}

#[derive(Debug)]
struct Request<'a> {
    method: &'a str,
    path: &'a str,
    content_length: Option<usize>,
    sha256_hex: Option<&'a str>,
}

fn parse_request<'a>(buf: &'a [u8]) -> Result<Request<'a>, ServerError> {
    let head = str::from_utf8(buf).map_err(|_| ServerError::BadRequest)?;
    let mut lines = head.split("\r\n");
    let req_line = lines.next().ok_or(ServerError::BadRequest)?;
    let mut parts = req_line.split_ascii_whitespace();
    let method = parts.next().ok_or(ServerError::BadRequest)?;
    let path = parts.next().ok_or(ServerError::BadRequest)?;
    let _proto = parts.next().ok_or(ServerError::BadRequest)?;

    let mut content_length = None;
    let mut sha256_hex = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let mut hp = line.splitn(2, ':');
        let name = hp.next().unwrap_or("").trim();
        let value = hp.next().unwrap_or("").trim();
        if name.eq_ignore_ascii_case("Content-Length") {
            content_length = value.parse().ok();
        } else if name.eq_ignore_ascii_case("X-PA-Sha256") {
            sha256_hex = Some(value);
        }
    }
    Ok(Request {
        method,
        path,
        content_length,
        sha256_hex,
    })
}

async fn read_headers(
    socket: &mut TcpSocket<'_>,
    buf: &mut [u8; HEADER_BUF_LEN],
) -> Result<usize, ServerError> {
    let mut pos = 0;
    loop {
        if pos >= buf.len() {
            return Err(ServerError::BadRequest);
        }
        let n = socket.read(&mut buf[pos..]).await.map_err(|_| ServerError::Io)?;
        if n == 0 {
            return Err(ServerError::BadRequest);
        }
        pos += n;
        // Look for the \r\n\r\n terminator.
        if let Some(end) = find_subslice(&buf[..pos], b"\r\n\r\n") {
            return Ok(end + 4);
        }
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    for i in 0..=(hay.len() - needle.len()) {
        if hay[i..i + needle.len()].eq(needle) {
            return Some(i);
        }
    }
    None
}

// ── GET /info ────────────────────────────────────────────────────────────────

async fn respond_info(
    socket: &mut TcpSocket<'_>,
    ctx: &'static DevServerCtx,
) -> Result<(), ServerError> {
    let ip = socket
        .local_endpoint()
        .map(|ep| ep.addr.to_string())
        .unwrap_or_else(|| String::from("0.0.0.0"));
    let mac = format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        ctx.mac[0], ctx.mac[1], ctx.mac[2], ctx.mac[3], ctx.mac[4], ctx.mac[5]
    );
    let body = format!(
        r#"{{"fw_version":"{}","build_time":"{}","board":"{}","channel":"dev","ip":"{}","mac":"{}","panel":{{"width_px":{},"height_px":{}}}}}"#,
        ctx.fw_version, ctx.build_time, ctx.board_slug, ip, mac, ctx.panel_width_px, ctx.panel_height_px
    );
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes()).await.map_err(|_| ServerError::Io)?;
    socket.write_all(body.as_bytes()).await.map_err(|_| ServerError::Io)?;
    socket.flush().await.map_err(|_| ServerError::Io)?;
    Ok(())
}

// ── PUT /firmware ───────────────────────────────────────────────────────────

async fn handle_firmware_put(
    socket: &mut TcpSocket<'_>,
    _ctx: &'static DevServerCtx,
    req: &Request<'_>,
) -> Result<(), ServerError> {
    let expected_len = req.content_length.ok_or(ServerError::BadRequest)?;
    if expected_len == 0 {
        return respond_status(socket, 400, "Content-Length: 0").await;
    }
    let expected_sha = req.sha256_hex.ok_or(ServerError::Unauthorized)?;
    if expected_sha.len() != 64 {
        return Err(ServerError::Unauthorized);
    }

    info!(
        "dev_server: PUT /firmware ({} bytes, sha256 {})",
        expected_len,
        &expected_sha[..16]
    );

    // Open the OTA target. Safe to steal FLASH because the runtime
    // task is the only other consumer and is sleeping on get_state
    // while we handle the connection.
    let flash = unsafe { FLASH::steal() };
    let mut storage = FlashStorage::new(flash).multicore_auto_park();
    let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let mut updater = OtaUpdater::new(&mut storage, &mut pt_buf).map_err(|e| {
        warn!("dev_server: OtaUpdater::new failed: {:?}", e);
        ServerError::Bootloader
    })?;
    let (mut region, next_subtype) = updater.next_partition().map_err(|_| ServerError::Bootloader)?;
    info!("dev_server: writing into slot {:?}", next_subtype);

    let mut sector_buf = [0u8; SECTOR_SIZE];
    let mut sector_filled: usize = 0;
    let mut flash_offset: u32 = 0;
    let mut hasher = Sha256::new();
    let mut total_read: usize = 0;
    let mut read_buf = [0u8; 1024];

    while total_read < expected_len {
        let want = (expected_len - total_read).min(read_buf.len());
        let n = socket
            .read(&mut read_buf[..want])
            .await
            .map_err(|_| ServerError::Io)?;
        if n == 0 {
            return Err(ServerError::SizeMismatch);
        }
        let mut chunk = &read_buf[..n];
        hasher.update(chunk);
        total_read += n;

        while !chunk.is_empty() {
            let space = SECTOR_SIZE - sector_filled;
            let take = chunk.len().min(space);
            sector_buf[sector_filled..sector_filled + take].copy_from_slice(&chunk[..take]);
            sector_filled += take;
            chunk = &chunk[take..];
            if sector_filled == SECTOR_SIZE {
                region
                    .write(flash_offset, &sector_buf)
                    .map_err(|_| ServerError::Flash)?;
                flash_offset += SECTOR_SIZE as u32;
                sector_filled = 0;
            }
        }
    }
    if sector_filled > 0 {
        region
            .write(flash_offset, &sector_buf[..sector_filled])
            .map_err(|_| ServerError::Flash)?;
        flash_offset += sector_filled as u32;
    }

    if flash_offset as usize != expected_len {
        return Err(ServerError::SizeMismatch);
    }

    let digest = hasher.finalize();
    let got = hex_lower(&digest);
    if got != expected_sha.to_ascii_lowercase() {
        warn!(
            "dev_server: sha256 mismatch\n  got      {}\n  expected {}",
            got, expected_sha
        );
        return Err(ServerError::HashMismatch);
    }
    drop(region);

    updater
        .activate_next_partition()
        .map_err(|_| ServerError::Bootloader)?;
    updater
        .set_current_ota_state(OtaImageState::New)
        .map_err(|_| ServerError::Bootloader)?;

    info!(
        "dev_server: install complete ({} bytes, sha256 ok). rebooting into new slot in 200ms",
        flash_offset
    );
    respond_status(socket, 200, "installed; rebooting").await?;
    Timer::after(Duration::from_millis(200)).await;
    esp_hal::system::software_reset();
}

// ── Response helpers ────────────────────────────────────────────────────────

async fn respond_status(
    socket: &mut TcpSocket<'_>,
    code: u16,
    msg: &str,
) -> Result<(), ServerError> {
    let phrase = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let body = format!("{} {}\n", code, msg);
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        code, phrase, body.len()
    );
    socket.write_all(head.as_bytes()).await.map_err(|_| ServerError::Io)?;
    socket.write_all(body.as_bytes()).await.map_err(|_| ServerError::Io)?;
    socket.flush().await.map_err(|_| ServerError::Io)?;
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let hi = b >> 4;
        let lo = b & 0x0F;
        s.push(nibble(hi));
        s.push(nibble(lo));
    }
    s
}

fn nibble(n: u8) -> char {
    if n < 10 {
        (b'0' + n) as char
    } else {
        (b'a' + n - 10) as char
    }
}
