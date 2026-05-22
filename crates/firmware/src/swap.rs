//! `SwapAlloc` — a paged virtual-memory abstraction backed by any
//! [`BlockDevice`] (today, an SD card; tomorrow, anything else
//! block-oriented). Lets the firmware treat a multi-megabyte buffer
//! as if it were resident RAM, while only a small page-cache window
//! actually lives in heap at any moment.
//!
//! ## Why this exists
//!
//! Cardstock cards that ship large resources — pre-rasterised graph
//! framebuffers, hourly NOAA forecasts, font/icon atlases — overrun
//! the firmware heap on E-series boards even with 8 MB PSRAM once
//! you account for the panel framebuffer (~330 KB on a 1404×1872
//! mono panel) + TLS receive buffers + JSON parse scratch. Holding
//! a 2 MB graph raster on top is not feasible.
//!
//! The escape hatch: spill to the SD card. ESP32 has no MMU so we
//! can't do real page-fault swap — but we can do *explicit* paged
//! IO. `SwapAlloc::reserve` returns a handle to a fixed-size buffer
//! living in a file on the SD card; reads + writes through the
//! handle hit a small in-RAM page cache, paging cold pages out to
//! disk on demand under LRU.
//!
//! ## Layout
//!
//! ```text
//!   ┌─────────────── SD-backed buffer (e.g. 2 MB) ────────────────┐
//!   │ page 0 │ page 1 │ page 2 │ page 3 │  ...  │ page N-1        │
//!   └────┬───┴────┬───┴────────┴────────┴───────┴─────────────────┘
//!        │        │
//!        ▼        ▼                    Resident set (e.g. 8 slots)
//!   ┌────────┬────────┐     ┌────────┬────────┬────────┬────────┐
//!   │ page 0 │ page 1 │  …  │ slot 0 │ slot 1 │ slot 2 │ ...    │
//!   │ in RAM │ in RAM │     └────────┴────────┴────────┴────────┘
//!   └────────┴────────┘                LRU eviction policy
//! ```
//!
//! ## API shape
//!
//! ```ignore
//! let mut alloc = SwapAlloc::with_window(&mut bdev, file_id, /*bytes=*/2_097_152, /*resident_pages=*/8)?;
//! alloc.write(addr, &src_bytes)?;
//! let mut buf = [0u8; 4096];
//! alloc.read(addr, &mut buf)?;
//! // Resident set managed automatically; cold pages flush as LRU evicts.
//! ```
//!
//! Reads/writes are routed through the resident-set cache. On a
//! cache miss the LRU page is evicted (written back to disk if
//! dirty); the requested page is paged in.
//!
//! ## Today's status
//!
//! Algorithm + cache + LRU eviction are implemented and unit-tested
//! against an in-memory `BlockDevice` (see the `tests` module at the
//! bottom of this file). The SD-backed `BlockDevice` impl lives in
//! `crate::sd`; once the SPI-bus-sharing refactor lands (the panel
//! currently owns SPI2 exclusively) the SD impl plugs into the same
//! `SwapAlloc::with_window` constructor unchanged.

#![allow(dead_code)]

use alloc::vec::Vec;
use alloc::boxed::Box;
use core::fmt;

/// Page size in bytes. 4 KB matches typical SD-card sector
/// transfers and lines up with `embedded_sdmmc::Block`'s 512 B
/// blocks at exactly 8 blocks per page — clean math for paging.
pub const PAGE_SIZE: usize = 4096;

/// Bytes-per-block on the underlying device. embedded-sdmmc uses
/// 512 B blocks (mandated by SD card spec); leaving this as a
/// const so a future BlockDevice with a different native size can
/// be slotted in.
pub const BLOCK_SIZE: usize = 512;

/// Number of blocks per page (PAGE_SIZE / BLOCK_SIZE).
pub const BLOCKS_PER_PAGE: usize = PAGE_SIZE / BLOCK_SIZE;

/// Abstract block-addressable storage. Modelled after
/// `embedded_sdmmc::BlockDevice` but simplified — `SwapAlloc`
/// translates page-level reads/writes into block-level ones.
///
/// Implementations:
///   * `sd::FwSd` — real SD card via the firmware's SD driver.
///   * `MemoryBlockDevice` in this file's tests — in-RAM Vec for
///     host-side validation of the swap logic.
pub trait BlockDevice {
    type Error: fmt::Debug;

    /// Read `buf.len()` bytes starting at `block_idx * BLOCK_SIZE`.
    /// `buf.len()` must be a multiple of BLOCK_SIZE.
    fn read_blocks(&mut self, block_idx: u32, buf: &mut [u8]) -> Result<(), Self::Error>;

    /// Write `buf.len()` bytes starting at `block_idx * BLOCK_SIZE`.
    /// `buf.len()` must be a multiple of BLOCK_SIZE.
    fn write_blocks(&mut self, block_idx: u32, buf: &[u8]) -> Result<(), Self::Error>;
}

/// Errors the swap layer can surface independently of the
/// underlying BlockDevice's own errors.
#[derive(Debug)]
pub enum SwapError<E: fmt::Debug> {
    /// The address (or len) falls outside the reserved window.
    OutOfBounds { addr: u32, len: u32, window_bytes: u32 },
    /// The underlying BlockDevice surfaced an error during a
    /// read or write (likely SD media failure).
    Block(E),
    /// `with_window` was called with `resident_pages == 0`.
    NoResidentPages,
}

impl<E: fmt::Debug> From<E> for SwapError<E> {
    fn from(e: E) -> Self {
        SwapError::Block(e)
    }
}

/// One resident page slot in the LRU cache.
struct ResidentSlot {
    /// Logical page index this slot currently holds. `None` =
    /// empty slot (cold start before first miss).
    page: Option<u32>,
    /// Page contents. Always exactly PAGE_SIZE bytes; we use Vec
    /// rather than `[u8; PAGE_SIZE]` so the cache can sit in heap
    /// and not blow the .bss budget.
    bytes: Vec<u8>,
    /// Has this slot been written since it was paged in? Drives
    /// the write-back-on-evict decision.
    dirty: bool,
    /// Monotonic last-touched counter, used for LRU. Replaced by
    /// a real intrusive list if we ever profile this hot — N is
    /// small (default 8) so a linear scan of all slots is fine.
    last_used: u64,
}

/// Paged virtual-memory allocator backed by a block device.
pub struct SwapAlloc<B: BlockDevice> {
    bdev: B,
    /// First block on the underlying device this allocator owns.
    /// Lets multiple SwapAllocs (or other block-aligned consumers)
    /// coexist on the same SD without overlapping each other.
    base_block: u32,
    /// Total reserved size in bytes. Read/write addresses must
    /// fall within `0..window_bytes`.
    window_bytes: u32,
    /// In-RAM resident page set. Length is fixed at construction
    /// (`with_window`'s `resident_pages` argument).
    slots: Vec<ResidentSlot>,
    /// Monotonic "now" counter for LRU. Bumps on every touch.
    tick: u64,
}

impl<B: BlockDevice> SwapAlloc<B> {
    /// Reserve a paged window backed by `bdev` starting at
    /// `base_block`. `bytes` is rounded up to a page boundary;
    /// `resident_pages` slots × PAGE_SIZE will be allocated in
    /// heap up front.
    ///
    /// Caller is responsible for ensuring the underlying device
    /// has enough capacity from `base_block` onward to fit the
    /// requested window. A future improvement is to query the
    /// device's block count and validate up front.
    pub fn with_window(
        bdev: B,
        base_block: u32,
        bytes: u32,
        resident_pages: usize,
    ) -> Result<Self, SwapError<B::Error>> {
        if resident_pages == 0 {
            return Err(SwapError::NoResidentPages);
        }
        let window_bytes = round_up_page(bytes);
        let mut slots = Vec::with_capacity(resident_pages);
        for _ in 0..resident_pages {
            slots.push(ResidentSlot {
                page: None,
                bytes: alloc::vec![0u8; PAGE_SIZE],
                dirty: false,
                last_used: 0,
            });
        }
        Ok(SwapAlloc {
            bdev,
            base_block,
            window_bytes,
            slots,
            tick: 0,
        })
    }

    /// Reserved window size in bytes (post page-rounding).
    pub fn capacity_bytes(&self) -> u32 {
        self.window_bytes
    }

    /// How many pages live in the resident set.
    pub fn resident_capacity(&self) -> usize {
        self.slots.len()
    }

    /// Read `buf.len()` bytes starting at logical address `addr`.
    /// May cross page boundaries; each page touched is faulted
    /// into the resident set and bumped to MRU.
    pub fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), SwapError<B::Error>> {
        self.bounds_check(addr, buf.len() as u32)?;
        let mut remaining = buf;
        let mut cursor = addr;
        while !remaining.is_empty() {
            let page_idx = cursor / PAGE_SIZE as u32;
            let page_off = (cursor % PAGE_SIZE as u32) as usize;
            let take = remaining.len().min(PAGE_SIZE - page_off);
            let slot_idx = self.fault_in(page_idx)?;
            remaining[..take]
                .copy_from_slice(&self.slots[slot_idx].bytes[page_off..page_off + take]);
            self.touch(slot_idx);
            cursor += take as u32;
            remaining = &mut remaining[take..];
        }
        Ok(())
    }

    /// Write `buf.len()` bytes starting at logical address `addr`.
    /// Each affected page is faulted in (so we don't lose data on
    /// neighbouring bytes), updated in the resident slot, and
    /// flagged dirty. Write-back to disk happens on eviction or
    /// explicit `flush`.
    pub fn write(&mut self, addr: u32, buf: &[u8]) -> Result<(), SwapError<B::Error>> {
        self.bounds_check(addr, buf.len() as u32)?;
        let mut remaining = buf;
        let mut cursor = addr;
        while !remaining.is_empty() {
            let page_idx = cursor / PAGE_SIZE as u32;
            let page_off = (cursor % PAGE_SIZE as u32) as usize;
            let take = remaining.len().min(PAGE_SIZE - page_off);
            let slot_idx = self.fault_in(page_idx)?;
            self.slots[slot_idx].bytes[page_off..page_off + take]
                .copy_from_slice(&remaining[..take]);
            self.slots[slot_idx].dirty = true;
            self.touch(slot_idx);
            cursor += take as u32;
            remaining = &remaining[take..];
        }
        Ok(())
    }

    /// Flush every dirty resident slot to the underlying device.
    /// Called before reading the file back from another process
    /// (e.g. the panel actor) or before tearing down the swap.
    pub fn flush(&mut self) -> Result<(), SwapError<B::Error>> {
        for slot_idx in 0..self.slots.len() {
            if self.slots[slot_idx].dirty {
                self.write_back(slot_idx)?;
            }
        }
        Ok(())
    }

    /// Move the slot to MRU position (newest).
    fn touch(&mut self, slot_idx: usize) {
        self.tick = self.tick.wrapping_add(1);
        self.slots[slot_idx].last_used = self.tick;
    }

    /// Ensure `page_idx` is resident; return its slot index. May
    /// evict the LRU slot (writing back if dirty).
    fn fault_in(&mut self, page_idx: u32) -> Result<usize, SwapError<B::Error>> {
        // Already resident?
        for (i, slot) in self.slots.iter().enumerate() {
            if slot.page == Some(page_idx) {
                return Ok(i);
            }
        }
        // Find a slot to (re)use: prefer empty, fall back to LRU.
        let target = self
            .slots
            .iter()
            .enumerate()
            .min_by_key(|(_, s)| (s.page.is_some() as u8, s.last_used))
            .map(|(i, _)| i)
            .expect("resident_pages > 0 ensured at construction");
        if self.slots[target].dirty {
            self.write_back(target)?;
        }
        // Page in.
        let block_idx = self.base_block + page_idx * BLOCKS_PER_PAGE as u32;
        // Take the bytes buffer out briefly so we can hand a
        // mutable slice to `read_blocks` without holding `&mut self`
        // through the call.
        let mut buf = core::mem::take(&mut self.slots[target].bytes);
        buf.resize(PAGE_SIZE, 0);
        self.bdev.read_blocks(block_idx, &mut buf)?;
        self.slots[target].bytes = buf;
        self.slots[target].page = Some(page_idx);
        self.slots[target].dirty = false;
        Ok(target)
    }

    /// Persist a dirty slot to disk + clear its dirty flag. Slot's
    /// page mapping is left intact so the caller can keep using
    /// it; eviction proper happens in `fault_in` overwriting the
    /// `page` field afterward.
    fn write_back(&mut self, slot_idx: usize) -> Result<(), SwapError<B::Error>> {
        let page = self.slots[slot_idx]
            .page
            .expect("write_back on empty slot");
        let block_idx = self.base_block + page * BLOCKS_PER_PAGE as u32;
        let buf = core::mem::take(&mut self.slots[slot_idx].bytes);
        let _ = Box::new(0); // suppress unused-Box-import if no other call sites
        self.bdev.write_blocks(block_idx, &buf)?;
        self.slots[slot_idx].bytes = buf;
        self.slots[slot_idx].dirty = false;
        Ok(())
    }

    fn bounds_check(&self, addr: u32, len: u32) -> Result<(), SwapError<B::Error>> {
        let end = addr.checked_add(len).ok_or(SwapError::OutOfBounds {
            addr,
            len,
            window_bytes: self.window_bytes,
        })?;
        if end > self.window_bytes {
            return Err(SwapError::OutOfBounds {
                addr,
                len,
                window_bytes: self.window_bytes,
            });
        }
        Ok(())
    }
}

fn round_up_page(bytes: u32) -> u32 {
    let mask = (PAGE_SIZE as u32) - 1;
    (bytes + mask) & !mask
}

// ── Host-testable mock + unit tests ──────────────────────────────
//
// Compiled out of the firmware build via `#[cfg(test)]`; only
// reachable from `cargo test`. The host's std is in scope under
// test so std::vec etc. works.

#[cfg(test)]
mod tests {
    use super::*;

    /// In-RAM BlockDevice for unit tests. Tracks total reads /
    /// writes so tests can assert eviction actually happens.
    struct MemoryBlockDevice {
        storage: Vec<u8>,
        reads: u32,
        writes: u32,
    }

    impl MemoryBlockDevice {
        fn with_bytes(bytes: usize) -> Self {
            Self {
                storage: alloc::vec![0u8; bytes],
                reads: 0,
                writes: 0,
            }
        }
    }

    impl BlockDevice for MemoryBlockDevice {
        type Error = core::convert::Infallible;

        fn read_blocks(&mut self, block_idx: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
            let off = block_idx as usize * BLOCK_SIZE;
            buf.copy_from_slice(&self.storage[off..off + buf.len()]);
            self.reads += 1;
            Ok(())
        }

        fn write_blocks(&mut self, block_idx: u32, buf: &[u8]) -> Result<(), Self::Error> {
            let off = block_idx as usize * BLOCK_SIZE;
            self.storage[off..off + buf.len()].copy_from_slice(buf);
            self.writes += 1;
            Ok(())
        }
    }

    #[test]
    fn round_up_page_basic() {
        assert_eq!(round_up_page(0), 0);
        assert_eq!(round_up_page(1), PAGE_SIZE as u32);
        assert_eq!(round_up_page(PAGE_SIZE as u32), PAGE_SIZE as u32);
        assert_eq!(round_up_page(PAGE_SIZE as u32 + 1), 2 * PAGE_SIZE as u32);
    }

    #[test]
    fn write_then_read_roundtrip_single_page() {
        let bdev = MemoryBlockDevice::with_bytes(8 * PAGE_SIZE);
        let mut alloc = SwapAlloc::with_window(bdev, 0, PAGE_SIZE as u32, 2).unwrap();
        let src = [0xab; 64];
        alloc.write(100, &src).unwrap();
        let mut dst = [0u8; 64];
        alloc.read(100, &mut dst).unwrap();
        assert_eq!(dst, src);
    }

    #[test]
    fn cross_page_write_then_read() {
        let bdev = MemoryBlockDevice::with_bytes(8 * PAGE_SIZE);
        let mut alloc = SwapAlloc::with_window(bdev, 0, 4 * PAGE_SIZE as u32, 4).unwrap();
        // 6 KB write spans two pages.
        let src: Vec<u8> = (0..6 * 1024).map(|i| (i % 251) as u8).collect();
        alloc.write(PAGE_SIZE as u32 - 1024, &src).unwrap();
        let mut dst = vec![0u8; src.len()];
        alloc.read(PAGE_SIZE as u32 - 1024, &mut dst).unwrap();
        assert_eq!(dst, src);
    }

    #[test]
    fn lru_evicts_cold_page_and_writes_back_dirty() {
        let bdev = MemoryBlockDevice::with_bytes(8 * PAGE_SIZE);
        // 4-page window, only 2 resident — guarantees eviction.
        let mut alloc = SwapAlloc::with_window(bdev, 0, 4 * PAGE_SIZE as u32, 2).unwrap();
        // Write a marker into each page.
        for p in 0..4 {
            let marker = [p as u8 + 1; 16];
            alloc.write(p * PAGE_SIZE as u32, &marker).unwrap();
        }
        alloc.flush().unwrap();
        // Read each page back; markers must round-trip even though
        // only 2 pages can be resident at once.
        for p in 0..4 {
            let mut buf = [0u8; 16];
            alloc.read(p * PAGE_SIZE as u32, &mut buf).unwrap();
            assert_eq!(buf, [p as u8 + 1; 16]);
        }
        // At least 2 writes must have happened (the two evicted
        // dirty pages); reads should be at least 4 (each page
        // missed at least once).
        assert!(alloc.bdev.writes >= 2);
        assert!(alloc.bdev.reads >= 4);
    }

    #[test]
    fn out_of_bounds_is_caught() {
        let bdev = MemoryBlockDevice::with_bytes(8 * PAGE_SIZE);
        let mut alloc = SwapAlloc::with_window(bdev, 0, PAGE_SIZE as u32, 2).unwrap();
        let mut buf = [0u8; 16];
        let r = alloc.read(PAGE_SIZE as u32 - 8, &mut buf);
        assert!(matches!(r, Err(SwapError::OutOfBounds { .. })));
    }

    #[test]
    fn zero_resident_pages_is_rejected() {
        let bdev = MemoryBlockDevice::with_bytes(8 * PAGE_SIZE);
        let r = SwapAlloc::with_window(bdev, 0, PAGE_SIZE as u32, 0);
        assert!(matches!(r, Err(SwapError::NoResidentPages)));
    }
}
