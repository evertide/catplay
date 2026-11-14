use std::{collections::VecDeque, net::TcpStream, time::Duration};

use async_trait::async_trait;
use catplay_plist::CachingSerializer;
use catplay_tokio::{CItem, TcpSession, TcpSink};
use catplay_util::{AsyncShutdown, EventSleeper};
use log::{Level, log};
use serde::Deserialize;

use crate::{
    cipher::AirPlayCipherSaltType,
    msg::Command,
    rtsp_frame::{HttpStatus, RtspDrain, RtspError, RtspFuture, RtspMethod, RtspQueue, RtspRequest, RtspResult},
    rtsp_transport::{AirPlayTransportCodec, RtspFrame},
};

#[derive(EventSleeper)]
pub struct EventsClient {
    shared_secret: Option<[u8; 32]>,
    rtsp: RtspQueue,
    #[sleep]
    rtsp_drain: RtspDrain,
    response_log_levels: VecDeque<Level>,
}

impl EventsClient {
    pub fn new(shared_secret: [u8; 32]) -> Self {
        let (rtsp, rtsp_drain) = RtspQueue::fifo();
        Self {
            rtsp,
            rtsp_drain,
            shared_secret: Some(shared_secret),
            response_log_levels: VecDeque::new(),
        }
    }

    pub fn unencrypted() -> Self {
        let (rtsp, rtsp_drain) = RtspQueue::fifo();
        Self {
            rtsp,
            rtsp_drain,
            shared_secret: None,
            response_log_levels: VecDeque::new(),
        }
    }

    pub fn rtsp_client(&self) -> RtspQueue {
        self.rtsp.clone()
    }

    fn request_log_level(&mut self, request: &RtspRequest) -> Level {
        #[derive(Deserialize)]
        struct CommandType {
            #[serde(rename = "type")]
            name: String,
        }

        // Read only the command type; do not decode or copy the iAP2 data for logging.
        let iap2 = request.url.as_str() == "/command"
            && catplay_plist::from_bytes::<CommandType>(&request.payload).is_ok_and(|command| command.name == "iAPSendMessage");
        let level = if iap2 { Level::Debug } else { Level::Warn };
        self.response_log_levels.push_back(level);
        level
    }

    fn response_log_level(&mut self, status: HttpStatus) -> Level {
        // Event responses have no CSeq. Keep FIFO slots even if the caller drops its future.
        let level = self.response_log_levels.pop_front().unwrap_or(Level::Warn);
        if status == HttpStatus::Ok { level } else { Level::Warn }
    }
}

pub trait CommandsClient {
    fn send_command(&self, cmd: &Command, timeout: Duration) -> RtspResult<RtspFuture>;

    fn send_command_caching(&self, cmd: &Command, timeout: Duration, serializer: &mut CachingSerializer) -> RtspResult<RtspFuture>;
}

impl CommandsClient for RtspQueue {
    fn send_command(&self, cmd: &Command, timeout: Duration) -> RtspResult<RtspFuture> {
        let ev = cmd.serialize().map_err(RtspError::SerializationFailed)?;
        let fut = self.request_timeout(RtspRequest::with_payload(RtspMethod::Post, "/command", &ev[..]), timeout);
        Ok(fut)
    }

    fn send_command_caching(&self, cmd: &Command, timeout: Duration, serializer: &mut CachingSerializer) -> RtspResult<RtspFuture> {
        let ev = cmd
            .serialize_caching(serializer)
            .map_err(RtspError::SerializationFailed)?;
        let fut = self.request_timeout(RtspRequest::with_payload(RtspMethod::Post, "/command", ev), timeout);
        Ok(fut)
    }
}

#[async_trait]
impl TcpSession for EventsClient {
    type Codec = AirPlayTransportCodec;
    type Error = RtspError;

    fn init_stream(&mut self, stream: &mut TcpStream) -> Result<(), Self::Error> {
        stream.set_nodelay(true)?;
        std::os::linux::net::TcpStreamExt::set_quickack(stream, true)?;

        Ok(())
    }

    fn init_codec(&mut self) -> RtspResult<AirPlayTransportCodec> {
        let mut codec = AirPlayTransportCodec::new(false);
        if let Some(shared_secret) = self.shared_secret {
            codec.encrypt(AirPlayCipherSaltType::Events, shared_secret);
        }
        Ok(codec)
    }

    async fn on_msg(&mut self, _sink: &mut dyn TcpSink<Self>, msgs: CItem<Self::Codec>) -> RtspResult<()> {
        for msg in msgs {
            if let RtspFrame::Response(resp) = msg {
                let level = self.response_log_level(resp.status);
                log!(level, "Event socket: <- {resp}");
                self.rtsp_drain.feed(resp);
            }
        }
        Ok(())
    }

    async fn reconcile(&mut self, _sink: &mut dyn TcpSink<Self>) -> RtspResult<()> {
        for req in self.rtsp_drain.drain() {
            let level = self.request_log_level(&req);
            log!(level, "Event socket: -> {}", &req);
            _sink.write(vec![req.into()])?;
        }

        Ok(())
    }

    async fn on_eof(&mut self, _status: Option<RtspError>) {
        self.rtsp_drain.close();
    }
}

impl AsyncShutdown for EventsClient {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::{CommandForceKeyFrame, CommandIApSendMessage};

    #[test]
    fn only_iap2_commands_and_their_successful_fifo_responses_use_debug() {
        let mut client = EventsClient::unencrypted();
        let queue = client.rtsp_client();
        let iap2 = Command::IApSendMessage(CommandIApSendMessage {
            data: vec![1, 2, 3].into(),
        });
        let video = Command::ForceKeyFrame(CommandForceKeyFrame { uuid: None });
        let timeout = Duration::from_secs(1);
        let cancelled = queue.send_command(&iap2, timeout).unwrap();
        drop(cancelled);
        let _video = queue.send_command(&video, timeout).unwrap();
        let _iap2 = queue
            .send_command_caching(&iap2, timeout, &mut CachingSerializer::default())
            .unwrap();
        let requests = client.rtsp_drain.drain();
        assert_eq!(requests.len(), 3);
        for (request, expected) in requests
            .iter()
            .zip([Level::Debug, Level::Warn, Level::Debug])
        {
            assert_eq!(client.request_log_level(request), expected);
        }
        for (status, expected) in [
            (HttpStatus::BadRequest, Level::Warn),
            (HttpStatus::Ok, Level::Warn),
            (HttpStatus::Ok, Level::Debug),
            (HttpStatus::Ok, Level::Warn),
        ] {
            assert_eq!(client.response_log_level(status), expected);
        }
    }

    #[test]
    fn malformed_event_requests_keep_warn_logging() {
        let mut client = EventsClient::unencrypted();
        let request = RtspRequest::with_payload(RtspMethod::Post, "/command", &b"invalid plist"[..]);
        assert_eq!(client.request_log_level(&request), Level::Warn);
        assert_eq!(client.response_log_level(HttpStatus::Ok), Level::Warn);
    }
}
