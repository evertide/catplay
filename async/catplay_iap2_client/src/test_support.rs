//! Wire-encoding capture handle for hardware-free session tests.
//! FileTransfer is intentionally unavailable on this link.
use std::sync::{Arc, Mutex};

use catplay_csm::{
    decoder::{AsCsmPacket, CsmPacketBox, CsmPacketRegistry},
    msg::{StartLocationInformation, StopLocationInformation},
};

use crate::{CsmClientHandle, CsmRemote, CsmSessionResult, CsmSessionStatus};

pub struct Capture {
    remote: CsmRemote,
    server: bool,
    packets: Mutex<Vec<CsmPacketBox>>,
}

impl Capture {
    pub fn new() -> Arc<Self> {
        Self::with_remote(CsmRemote::usb_gadget(), true)
    }

    pub fn with_remote(remote: CsmRemote, server: bool) -> Arc<Self> {
        Arc::new(Self {
            remote,
            server,
            packets: Mutex::new(Vec::new()),
        })
    }

    pub fn take(&self) -> Vec<CsmPacketBox> {
        std::mem::take(&mut *self.packets.lock().unwrap())
    }

    pub fn location_commands(&self) -> Vec<CsmPacketBox> {
        self.take()
            .into_iter()
            .filter(|packet| packet.cast::<StartLocationInformation>().is_some() || packet.cast::<StopLocationInformation>().is_some())
            .collect()
    }
}

impl CsmClientHandle for Capture {
    fn disconnect(&self) {}
    fn is_server(&self) -> bool {
        self.server
    }
    fn is_closed(&self) -> bool {
        false
    }
    fn is_downgrade(&self) -> bool {
        false
    }
    fn is_writable(&self) -> bool {
        true
    }
    fn remote(&self) -> &CsmRemote {
        &self.remote
    }
    fn status(&self) -> CsmSessionStatus {
        CsmSessionStatus::Writable
    }
    fn send(&self, packet: &dyn AsCsmPacket) -> CsmSessionResult<()> {
        // Exercise the actual packet registry and wire encoders in both directions.
        let registry = CsmPacketRegistry::static_registry();
        let bytes = registry.encode(packet).unwrap();
        self.packets
            .lock()
            .unwrap()
            .push(registry.decode(&bytes).unwrap());
        Ok(())
    }
    fn send_all(&self, packets: &[CsmPacketBox]) -> CsmSessionResult<()> {
        for packet in packets {
            self.send(packet)?;
        }
        Ok(())
    }
    fn send_file_reserve(&self) -> Option<u8> {
        None
    }
    fn send_file(&self, _: u8, _: u16, _: &[u8], _: Vec<u8>) {
        panic!("GPS must not use FileTransfer");
    }
}
