use std::time::Duration;

use crate::video::AvccConfig;

#[derive(Debug, PartialEq, Clone)]
pub struct AvccConfigExtended {
    pub hevc: bool,
    pub avcc: AvccConfig,
    pub video_latency: Duration,
    pub width: u32,
    pub height: u32,
    pub respect_timestamps: bool,
    /// View origin and size from the sender's VideoConfig header (params 3 and 4), passed on unchanged by a proxied stream.
    /// Zero for streams we encode ourselves.
    pub view: [[u8; 8]; 2],
}
