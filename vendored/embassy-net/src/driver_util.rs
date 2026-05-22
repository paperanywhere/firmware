use core::task::Context;

use embassy_net_driver::{Capabilities, Checksum, Driver, RxToken, TxToken};
use smoltcp::phy::{self, Medium};
use smoltcp::time::Instant;

pub(crate) struct DriverAdapter<'d, 'c, T>
where
    T: Driver,
{
    // must be Some when actually using this to rx/tx
    pub cx: Option<&'d mut Context<'c>>,
    pub inner: &'d mut T,
    pub medium: Medium,
}

impl<'d, 'c, T> phy::Device for DriverAdapter<'d, 'c, T>
where
    T: Driver,
{
    type RxToken<'a>
        = RxTokenAdapter<T::RxToken<'a>>
    where
        Self: 'a;
    type TxToken<'a>
        = TxTokenAdapter<T::TxToken<'a>>
    where
        Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let result = self
            .inner
            .receive(unwrap!(self.cx.as_deref_mut()))
            .map(|(rx, tx)| (RxTokenAdapter(rx), TxTokenAdapter(tx)));
        if result.is_none() {
            crate::diag::NET_RX_NONE
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        result
    }

    /// Construct a transmit token.
    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        let result = self.inner.transmit(unwrap!(self.cx.as_deref_mut())).map(TxTokenAdapter);
        if result.is_none() {
            crate::diag::NET_TX_NONE
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
        result
    }

    /// Get a description of device capabilities.
    fn capabilities(&self) -> phy::DeviceCapabilities {
        fn convert(c: Checksum) -> phy::Checksum {
            match c {
                Checksum::Both => phy::Checksum::Both,
                Checksum::Tx => phy::Checksum::Tx,
                Checksum::Rx => phy::Checksum::Rx,
                Checksum::None => phy::Checksum::None,
            }
        }
        let caps: Capabilities = self.inner.capabilities();
        let mut smolcaps = phy::DeviceCapabilities::default();

        smolcaps.max_transmission_unit = caps.max_transmission_unit;
        smolcaps.max_burst_size = caps.max_burst_size;
        smolcaps.medium = self.medium;
        smolcaps.checksum.ipv4 = convert(caps.checksum.ipv4);
        smolcaps.checksum.tcp = convert(caps.checksum.tcp);
        smolcaps.checksum.udp = convert(caps.checksum.udp);
        #[cfg(feature = "proto-ipv4")]
        {
            smolcaps.checksum.icmpv4 = convert(caps.checksum.icmpv4);
        }
        #[cfg(feature = "proto-ipv6")]
        {
            smolcaps.checksum.icmpv6 = convert(caps.checksum.icmpv6);
        }

        smolcaps
    }
}

pub(crate) struct RxTokenAdapter<T>(T)
where
    T: RxToken;

impl<T> phy::RxToken for RxTokenAdapter<T>
where
    T: RxToken,
{
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        self.0.consume(|buf| {
            crate::diag::NET_RX_PKTS
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // Snapshot the first few header bytes of the first 12
            // RX packets so the firmware can dump them via a slot
            // accessor — useful for confirming SYN-ACK arrival
            // when a TCP connect hangs.
            let n = crate::diag::NET_RX_PKTS
                .load(core::sync::atomic::Ordering::Relaxed) as usize;
            if n >= 1 && n <= 12 {
                let head_len = buf.len().min(54);
                let mut slot = [0u8; 54];
                slot[..head_len].copy_from_slice(&buf[..head_len]);
                crate::diag::stash_rx(n - 1, slot, head_len as u8, buf.len() as u16);
            }
            #[cfg(feature = "packet-trace")]
            trace!("embassy device rx: {:02x}", buf);
            f(buf)
        })
    }
}

pub(crate) struct TxTokenAdapter<T>(T)
where
    T: TxToken;

impl<T> phy::TxToken for TxTokenAdapter<T>
where
    T: TxToken,
{
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        self.0.consume(len, |buf| {
            let r = f(buf);
            crate::diag::NET_TX_PKTS
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let n = crate::diag::NET_TX_PKTS
                .load(core::sync::atomic::Ordering::Relaxed) as usize;
            // Stash first-N TX headers in a ring so the firmware can
            // dump them later. Confirms SYNs reach the driver edge
            // (vs being lost in smoltcp's poll).
            if n >= 1 && n <= 30 {
                let head_len = buf.len().min(54);
                let mut slot = [0u8; 54];
                slot[..head_len].copy_from_slice(&buf[..head_len]);
                crate::diag::stash_tx(n - 1, slot, head_len as u8, buf.len() as u16);
            }
            #[cfg(feature = "packet-trace")]
            trace!("embassy device tx: {:02x}", buf);
            r
        })
    }
}
