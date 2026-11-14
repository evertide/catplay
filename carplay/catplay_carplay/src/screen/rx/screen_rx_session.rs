use std::time::{Duration, Instant};

use async_trait::async_trait;
use catplay_tokio::{CItem, TcpSession, TcpSink};
use catplay_util::{AsyncShutdown, EventSleeper};
use log::{debug, info, trace, warn};

use crate::{
    cipher::AirPlayStreamEncryption,
    clock::MediaClockBox,
    rtsp_frame::{RtspError, RtspResult},
    screen::{ScreenFlag, ScreenFrameCodec, ScreenFrameCodecFlags, ScreenOpCode, rx::screen_rx_sink::ScreenReceiverSinkBox},
    video::AvccConfigExtended,
};

pub struct ScreenReceiverSession {
    cipher: AirPlayStreamEncryption,
    stream_connection_id: u64,

    clock: MediaClockBox,
    sink: ScreenReceiverSinkBox,
    video_latency: Duration,
    config: Option<AvccConfigExtended>,
    prev_frame: Option<Instant>,
    start: Instant,
    negative_ahead_frames: u32,
    late_frames: u32,
    display_delta_ms: i64,
}

impl ScreenReceiverSession {
    pub fn new(
        cipher: AirPlayStreamEncryption,
        stream_connection_id: u64,
        video_latency: Duration,
        sink: ScreenReceiverSinkBox,
        clock: MediaClockBox,
    ) -> Self {
        Self {
            cipher,
            stream_connection_id,
            config: None,
            video_latency,
            sink,
            clock,
            prev_frame: None,
            start: Instant::now(),
            negative_ahead_frames: 0,
            late_frames: 0,
            display_delta_ms: 0,
        }
    }
}

#[async_trait]
impl TcpSession for ScreenReceiverSession {
    type Codec = ScreenFrameCodec;
    type Error = RtspError;

    fn init_codec(&mut self) -> RtspResult<ScreenFrameCodec> {
        Ok(ScreenFrameCodec::new_with_flags(
            self.cipher,
            self.stream_connection_id,
            true,
            ScreenFrameCodecFlags::crate_defaults(),
        ))
    }

    async fn on_msg(&mut self, _sink: &mut dyn TcpSink<Self>, msg: CItem<Self::Codec>) -> RtspResult<()> {
        debug!("Received screen frame: {:?} {:?}", msg.header, msg.flags());

        match msg.header.opcode {
            ScreenOpCode::VideoFrame => {
                let Some(config) = self.config.as_ref() else {
                    return Err(RtspError::ProtocolViolation("received VideoFrame before VideoConfig"));
                };

                let _respect_timestamps = config.respect_timestamps;

                if !self.clock.is_synchronized() {
                    warn!("Received video frame before media clock was synchronized! Using pts = Instant::now()");
                }

                let frame = msg
                    .video_decode(config, self.clock.as_ref())
                    .map_err(|e| RtspError::ProtocolViolationString(format!("failed to scan NAL offsets in VideoFrame: {e:?}")))?
                    .ok_or(RtspError::Empty)?;

                let pts_decoded = frame.pts.decode();
                let display_ticks = frame.pts.0;
                let now = Instant::now();
                let video_latency_ms = self.video_latency.as_millis() as i64;

                let display_vs_now_ms = if display_ticks >= now {
                    display_ticks.saturating_duration_since(now).as_millis() as i64
                } else {
                    self.negative_ahead_frames = self.negative_ahead_frames.saturating_add(1);
                    -(now.saturating_duration_since(display_ticks).as_millis() as i64)
                };
                self.display_delta_ms = video_latency_ms - display_vs_now_ms;

                if self.display_delta_ms >= (2 * video_latency_ms) {
                    self.late_frames = self.late_frames.saturating_add(1);

                    warn!(
                        "Late frame ({} ms, {} total late frames, negativeAheadFrames={})",
                        self.display_delta_ms, self.late_frames, self.negative_ahead_frames
                    );
                }

                let recv_delta = match self.prev_frame {
                    None => {
                        let first_frame_took = now.saturating_duration_since(self.start);
                        warn!("Received first video frame after {first_frame_took:?}");
                        None
                    }
                    Some(v) => Some(now.saturating_duration_since(v)),
                };

                debug!("Decoded video frame pts={pts_decoded} recv_delta={recv_delta:?}");

                if frame.is_known_keyframe() {
                    warn!("Received keyframe");
                }

                self.prev_frame.replace(now);
                self.sink.process_frame(frame).await?;
            }

            ScreenOpCode::VideoConfig => {
                let config = msg.config_decode(self.video_latency);
                info!(
                    "Received VideoConfig: flags={:?} {}x{} params={:?}",
                    msg.flags(),
                    config.as_ref().map_or(0, |c| c.width),
                    config.as_ref().map_or(0, |c| c.height),
                    msg.header.params
                );

                if msg.flags().contains(ScreenFlag::ClearScreen) {
                    // A ClearScreen config with the same codec config only updates the header parameters (for example the view
                    // origin and size the iPhone switches to on showUI), so keep those; anything else is still ignored.
                    match clear_screen_update(config, self.config.as_ref()) {
                        Some(config) => {
                            self.sink.set_avcc_config(config.clone()).await?;
                            self.config.replace(config);
                        }
                        None => warn!("Ignoring ScreenFlag::ClearScreen"),
                    }
                    return Ok(());
                }
                let Some(config) = config else {
                    return Err(RtspError::ProtocolViolation("received unparsable VideoConfig frame"));
                };

                self.sink.set_avcc_config(config.clone()).await?;
                self.config.replace(config);
            }

            ScreenOpCode::KeepAlive | ScreenOpCode::KeepAliveWithBody => {
                trace!("Received screen keep alive: opcode {:?}", msg.header.opcode);

                if let Some(stats) = msg.sender_stats_decode() {
                    debug!("Received screen sender stats: {stats:?}");
                }
            }
            _ => {
                debug!("Received unknown video frame: {:?}", msg.header);
            }
        }
        Ok(())
    }

    async fn on_eof(&mut self, status: Option<RtspError>) {
        warn!("Observed EOF on screen socket! {status:?}");
    }
}

/// A ClearScreen VideoConfig is only applied when it changes nothing but the view origin and size; anything else only changes
/// through a regular VideoConfig.
fn clear_screen_update(config: Option<AvccConfigExtended>, current: Option<&AvccConfigExtended>) -> Option<AvccConfigExtended> {
    let (config, current) = (config?, current?);
    let view_only = AvccConfigExtended {
        view: current.view,
        ..config.clone()
    } == *current;
    (view_only && config.view != current.view).then_some(config)
}

impl EventSleeper for ScreenReceiverSession {}

impl AsyncShutdown for ScreenReceiverSession {
    async fn shutdown(&mut self) {
        self.sink.shutdown_pinned().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::AvccConfig;

    fn config(sps_pps: &[u8], view: [u8; 8]) -> AvccConfigExtended {
        AvccConfigExtended {
            hevc: false,
            avcc: AvccConfig {
                nal_size_len: 4,
                sps_pps: sps_pps.to_vec().into(),
            },
            video_latency: Duration::ZERO,
            width: 1920,
            height: 720,
            respect_timestamps: true,
            view: [[0; 8], view],
        }
    }

    #[test]
    fn clear_screen_only_updates_the_view() {
        let current = config(&[1, 2], [0; 8]);
        let new_view = config(&[1, 2], [7; 8]);
        assert_eq!(clear_screen_update(Some(new_view.clone()), Some(&current)), Some(new_view.clone()));
        assert_eq!(clear_screen_update(Some(current.clone()), Some(&current)), None, "nothing changed");
        assert_eq!(
            clear_screen_update(Some(config(&[3], [7; 8])), Some(&current)),
            None,
            "codec config changes are ignored"
        );
        let resized = AvccConfigExtended { width: 800, ..new_view };
        assert_eq!(clear_screen_update(Some(resized), Some(&current)), None, "size changes are ignored");
        assert_eq!(clear_screen_update(None, Some(&current)), None);
        assert_eq!(
            clear_screen_update(Some(current.clone()), None),
            None,
            "no VideoConfig received yet"
        );
    }
}
