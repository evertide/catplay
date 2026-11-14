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
    /// The sender's VideoConfig header parameters (for example the view origin and size), so a proxied stream tells the
    /// receiver exactly what the iPhone told us. All zero for streams we encode ourselves.
    pub header_params: [[u8; 8]; 15],
}
