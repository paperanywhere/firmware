//! Chunked HTTPS GET against `/api/device/blob/:image_id`. Bytes are streamed
//! straight into the panel driver — we never hold a full framebuffer in RAM.

pub struct DownloadConfig<'a> {
    pub host: &'a str,
    pub port: u16,
    pub path: &'a str,
    pub device_token: &'a str,
}

pub async fn stream_to_panel<F>(_cfg: DownloadConfig<'_>, mut _on_chunk: F) -> Result<(), ()>
where
    F: FnMut(&[u8]) -> Result<(), ()>,
{
    // M4: TLS GET with Range support, call on_chunk(...) per ~4 KB.
    Err(())
}
