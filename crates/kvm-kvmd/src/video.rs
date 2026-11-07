use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use kvm_core::{Error, Result, Snapshot, VideoStream};
use reqwest_websocket::{Message, RequestBuilderExt};
use tokio::{sync::watch, task::JoinHandle};

use crate::Kvmd;

// Limits bound memory consumed by an untrusted device independently of its claims.
const MAX_HEADER: usize = 16 * 1024;
const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_BOUNDARY: usize = 70;
const PING_INTERVAL: Duration = Duration::from_secs(15);

struct Mjpeg {
    response: reqwest::Response,
    boundary: Vec<u8>,
    buffer: Vec<u8>,
    pending: bytes::Bytes,
    lease: JoinHandle<()>,
    lease_state: watch::Receiver<bool>,
    timeout: Duration,
    first: bool,
    ended: bool,
}

impl Drop for Mjpeg {
    fn drop(&mut self) {
        self.lease.abort();
    }
}

pub(crate) async fn open(device: &Kvmd) -> Result<Box<dyn VideoStream>> {
    let session = device.client.read().await;
    let mut ws_url = device.url("ws")?;
    ws_url.query_pairs_mut().append_pair("stream", "true");
    let ws_config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_HEADER))
        .max_frame_size(Some(MAX_HEADER));
    let response = tokio::time::timeout(
        device.config.timeout,
        session
            .video
            .get(ws_url)
            .headers(device.auth_headers.clone())
            .upgrade()
            .web_socket_config(ws_config)
            .send(),
    )
    .await
    .map_err(|_| Error::Transport("stream lease timed out".into()))?
    .map_err(|error| match error {
        reqwest_websocket::Error::Reqwest(error) => crate::request_error("stream lease", &error),
        _ => Error::Transport("stream lease: request failure".into()),
    })?;
    if response.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
        Kvmd::check(response.status())?;
        return Err(Error::InvalidResponse(
            "stream lease was not upgraded".into(),
        ));
    }
    let mut socket = response
        .into_websocket()
        .await
        .map_err(|_| Error::InvalidResponse("invalid stream lease handshake".into()))?;
    let (lease_tx, lease_state) = watch::channel(true);
    let lease = tokio::spawn(async move {
        let mut interval = tokio::time::interval(PING_INTERVAL);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if !send_ping(&mut socket, PING_INTERVAL).await { break; }
                }
                message = socket.next() => match message {
                    Some(Ok(Message::Close { .. })) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
        }
        let _ = lease_tx.send(false);
    });
    // Guard the spawned task on all error and cancellation paths.
    struct Abort(Option<JoinHandle<()>>);
    impl Drop for Abort {
        fn drop(&mut self) {
            if let Some(task) = self.0.take() {
                task.abort();
            }
        }
    }
    let mut guard = Abort(Some(lease));
    let mut url = device.endpoint.clone();
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    let url = url
        .join("streamer/stream")
        .map_err(|_| Error::InvalidResponse("invalid stream URL".into()))?;
    let response = tokio::time::timeout(
        device.config.timeout,
        session
            .video
            .get(url)
            .headers(device.auth_headers.clone())
            .send(),
    )
    .await
    .map_err(|_| Error::Transport("video request timed out".into()))?
    .map_err(|error| crate::request_error("video request", &error))?;
    Kvmd::check(response.status())?;
    let content_type: mime::Mime = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| Error::InvalidResponse("missing multipart content type".into()))?;
    if content_type.type_() != mime::MULTIPART
        || content_type.subtype().as_str() != "x-mixed-replace"
    {
        return Err(Error::Unsupported("device does not expose MJPEG".into()));
    }
    let boundary = content_type
        .get_param("boundary")
        .map(|v| v.as_str())
        .filter(|b| {
            !b.is_empty()
                && b.len() <= MAX_BOUNDARY
                && b.bytes().all(|c| c.is_ascii_graphic() && c != b'"')
        })
        .ok_or_else(|| Error::InvalidResponse("invalid multipart boundary".into()))?;
    let boundary = format!("--{boundary}").into_bytes();
    Ok(Box::new(Mjpeg {
        response,
        boundary,
        buffer: Vec::new(),
        pending: bytes::Bytes::new(),
        lease: guard.0.take().expect("lease exists"),
        lease_state,
        timeout: device.config.timeout,
        first: true,
        ended: false,
    }))
}

async fn send_ping<S: futures_util::Sink<Message> + Unpin>(
    socket: &mut S,
    timeout: Duration,
) -> bool {
    matches!(
        tokio::time::timeout(
            timeout,
            socket.send(Message::Text(r#"{"event_type":"ping","event":{}}"#.into()))
        )
        .await,
        Ok(Ok(()))
    )
}

fn absorb_chunk(
    buffer: &mut Vec<u8>,
    pending: &mut bytes::Bytes,
    chunk: bytes::Bytes,
    limit: usize,
) -> Result<()> {
    let count = chunk.len().min(limit.saturating_sub(buffer.len()));
    if count == 0 {
        return Err(Error::InvalidResponse(
            "multipart data exceeds limit".into(),
        ));
    }
    buffer.extend_from_slice(&chunk[..count]);
    *pending = chunk.slice(count..);
    Ok(())
}

fn next_scan_offset(buffer_len: usize, delimiter_len: usize, body_start: usize) -> usize {
    buffer_len
        .saturating_sub(delimiter_len.saturating_sub(1))
        .max(body_start)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

impl Mjpeg {
    fn absorb(&mut self, chunk: bytes::Bytes, limit: usize) -> Result<()> {
        absorb_chunk(&mut self.buffer, &mut self.pending, chunk, limit)
    }

    async fn fill(&mut self, limit: usize) -> Result<bool> {
        if !*self.lease_state.borrow() {
            return Err(Error::Transport("stream lease ended".into()));
        }
        if !self.pending.is_empty() {
            let count = self
                .pending
                .len()
                .min(limit.saturating_sub(self.buffer.len()));
            if count == 0 {
                return Err(Error::InvalidResponse(
                    "multipart data exceeds limit".into(),
                ));
            }
            self.buffer.extend_from_slice(&self.pending.split_to(count));
            return Ok(true);
        }
        let chunk = tokio::select! {
            changed = self.lease_state.changed() => {
                let _ = changed;
                return Err(Error::Transport("stream lease ended".into()));
            }
            result = tokio::time::timeout(self.timeout, self.response.chunk()) => result
                .map_err(|_| Error::Transport("video read timed out".into()))?
                .map_err(|error| crate::request_error("video read", &error))?,
        };
        match chunk {
            Some(chunk) => {
                self.absorb(chunk, limit)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn until(&mut self, needle: &[u8], limit: usize) -> Result<usize> {
        loop {
            if let Some(pos) = find(&self.buffer, needle) {
                if pos + needle.len() > limit {
                    return Err(Error::InvalidResponse(
                        "multipart data exceeds limit".into(),
                    ));
                }
                return Ok(pos);
            }
            if self.buffer.len() >= limit {
                return Err(Error::InvalidResponse(
                    "multipart data exceeds limit".into(),
                ));
            }
            if !self.fill(MAX_FRAME + MAX_HEADER).await? {
                return Err(Error::InvalidResponse("truncated multipart stream".into()));
            }
        }
    }
}

#[async_trait]
impl VideoStream for Mjpeg {
    async fn next_frame(&mut self) -> Result<Option<Snapshot>> {
        if self.ended {
            return Ok(None);
        }
        if !*self.lease_state.borrow() {
            return Err(Error::Transport("stream lease ended".into()));
        }
        if self.first {
            let marker = [self.boundary.as_slice(), b"\r\n"].concat();
            let pos = self.until(&marker, MAX_HEADER).await?;
            if pos != 0 {
                return Err(Error::InvalidResponse("invalid multipart preamble".into()));
            }
            self.buffer.drain(..marker.len());
            self.first = false;
        }
        let end = self.until(b"\r\n\r\n", MAX_HEADER).await?;
        let headers = std::str::from_utf8(&self.buffer[..end])
            .map_err(|_| Error::InvalidResponse("invalid multipart headers".into()))?;
        let mut length = None;
        let mut jpeg = false;
        for line in headers.split("\r\n") {
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| Error::InvalidResponse("invalid multipart header".into()))?;
            if name.eq_ignore_ascii_case("content-length") {
                if length.is_some() {
                    return Err(Error::InvalidResponse("duplicate frame length".into()));
                }
                length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| Error::InvalidResponse("invalid frame length".into()))?,
                );
            }
            if name.eq_ignore_ascii_case("content-type") {
                jpeg = value.trim().eq_ignore_ascii_case("image/jpeg");
            }
        }
        if !jpeg {
            return Err(Error::InvalidResponse("frame is not JPEG".into()));
        }
        let body_start = end + 4;
        let delimiter = [b"\r\n".as_slice(), self.boundary.as_slice()].concat();
        let frame_limit = body_start + MAX_FRAME + delimiter.len() + 2;
        let size = if let Some(length) = length {
            if length > MAX_FRAME {
                return Err(Error::InvalidResponse("frame exceeds limit".into()));
            }
            while self.buffer.len() < body_start + length + delimiter.len() + 2 {
                if !self.fill(frame_limit).await? {
                    return Err(Error::InvalidResponse("truncated frame".into()));
                }
            }
            if !self.buffer[body_start + length..].starts_with(&delimiter) {
                return Err(Error::InvalidResponse("invalid frame boundary".into()));
            }
            length
        } else {
            let mut offset = body_start;
            loop {
                if let Some(relative) = find(&self.buffer[offset..], &delimiter) {
                    let pos = offset + relative;
                    while self.buffer.len() < pos + delimiter.len() + 2 {
                        if !self.fill(frame_limit).await? {
                            return Err(Error::InvalidResponse("truncated boundary".into()));
                        }
                    }
                    let suffix = &self.buffer[pos + delimiter.len()..pos + delimiter.len() + 2];
                    if suffix == b"\r\n" || suffix == b"--" {
                        break pos - body_start;
                    }
                    offset = pos + 1;
                } else {
                    if self.buffer.len() >= frame_limit {
                        return Err(Error::InvalidResponse("frame exceeds limit".into()));
                    }
                    offset = next_scan_offset(self.buffer.len(), delimiter.len(), body_start);
                    if !self.fill(frame_limit).await? {
                        return Err(Error::InvalidResponse("truncated frame".into()));
                    }
                }
            }
        };
        if size > MAX_FRAME
            || !self.buffer[body_start..body_start + size].starts_with(&[0xff, 0xd8])
            || !self.buffer[body_start..body_start + size].ends_with(&[0xff, 0xd9])
        {
            return Err(Error::InvalidResponse("invalid JPEG frame".into()));
        }
        let frame = self.buffer[body_start..body_start + size].to_vec();
        let suffix_start = body_start + size + delimiter.len();
        let suffix = &self.buffer[suffix_start..suffix_start + 2];
        if suffix == b"--" {
            self.ended = true;
            self.lease.abort();
        } else if suffix != b"\r\n" {
            return Err(Error::InvalidResponse("invalid boundary suffix".into()));
        }
        self.buffer.drain(..suffix_start + 2);
        Ok(Some(Snapshot { jpeg: frame }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };

    struct BlockedSink;
    impl futures_util::Sink<Message> for BlockedSink {
        type Error = ();
        fn poll_ready(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), ()>> {
            Poll::Pending
        }
        fn start_send(self: Pin<&mut Self>, _: Message) -> std::result::Result<(), ()> {
            unreachable!()
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), ()>> {
            Poll::Pending
        }
        fn poll_close(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), ()>> {
            Poll::Pending
        }
    }

    #[tokio::test]
    async fn blocked_ping_is_timed_out() {
        assert!(!send_ping(&mut BlockedSink, Duration::from_millis(10)).await);
    }

    #[tokio::test]
    async fn blocked_network_socket_times_out_ping() {
        use tokio::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connection = TcpStream::connect(listener.local_addr().unwrap());
        let (client, peer) = tokio::join!(connection, listener.accept());
        let client = client.unwrap();
        let (_peer, _) = peer.unwrap();
        let mut filled = 0usize;
        let data = [0u8; 65536];
        loop {
            client.writable().await.unwrap();
            match client.try_write(&data) {
                Ok(n) => {
                    filled += n;
                    if n == 0 {
                        panic!("socket closed");
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("socket write failed: {e}"),
            }
            assert!(filled < 64 * 1024 * 1024, "socket did not backpressure");
        }
        let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
            client,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        let result = tokio::time::timeout(Duration::from_millis(50), async {
            loop {
                ws.send(tokio_tungstenite::tungstenite::Message::Binary(
                    vec![0; 65536].into(),
                ))
                .await
                .unwrap();
            }
        })
        .await;
        assert!(
            result.is_err(),
            "saturated socket unexpectedly accepted unlimited websocket messages"
        );
    }

    #[test]
    fn fragmented_scan_work_is_linear_and_finds_split_delimiters() {
        let delimiter = b"\r\n--boundary";
        let mut data = Vec::new();
        let mut offset = 0;
        let mut work = 0;
        for _ in 0..4096 {
            data.extend_from_slice(b"abcdefgh");
            work += data.len() - offset;
            assert!(find(&data[offset..], delimiter).is_none());
            offset = next_scan_offset(data.len(), delimiter.len(), 0);
        }
        data.extend_from_slice(&delimiter[..delimiter.len() - 1]);
        assert!(find(&data[offset..], delimiter).is_none());
        offset = next_scan_offset(data.len(), delimiter.len(), 0);
        data.push(delimiter[delimiter.len() - 1]);
        assert_eq!(
            offset + find(&data[offset..], delimiter).unwrap(),
            data.len() - delimiter.len()
        );
        assert!(work <= 4096 * (8 + delimiter.len()));
    }

    #[test]
    fn coalesced_chunk_is_split_without_rejecting_later_frames() {
        let mut buffer = Vec::new();
        let mut pending = bytes::Bytes::new();
        absorb_chunk(
            &mut buffer,
            &mut pending,
            bytes::Bytes::from_static(b"frame-oneframe-two"),
            9,
        )
        .unwrap();
        assert_eq!(buffer, b"frame-one");
        assert_eq!(pending, b"frame-two"[..]);
    }
}
