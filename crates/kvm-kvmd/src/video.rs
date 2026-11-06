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
    let mut ws_url = device.url("ws")?;
    ws_url.query_pairs_mut().append_pair("stream", "true");
    let ws_config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_HEADER))
        .max_frame_size(Some(MAX_HEADER));
    let response = tokio::time::timeout(
        device.timeout,
        device
            .video_client
            .get(ws_url)
            .upgrade()
            .web_socket_config(ws_config)
            .send(),
    )
    .await
    .map_err(|_| Error::Transport("stream lease timed out".into()))?
    .map_err(|_| Error::Transport("stream lease request failed".into()))?;
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
                    if socket.send(Message::Text(r#"{"event_type":"ping","event":{}}"#.into())).await.is_err() { break; }
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
    let response = tokio::time::timeout(device.timeout, device.video_client.get(url).send())
        .await
        .map_err(|_| Error::Transport("video request timed out".into()))?
        .map_err(|_| Error::Transport("video request failed".into()))?;
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
        lease: guard.0.take().expect("lease exists"),
        lease_state,
        timeout: device.timeout,
        first: true,
        ended: false,
    }))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

impl Mjpeg {
    async fn fill(&mut self, limit: usize) -> Result<bool> {
        if !*self.lease_state.borrow() {
            return Err(Error::Transport("stream lease ended".into()));
        }
        let chunk = tokio::select! {
            changed = self.lease_state.changed() => {
                let _ = changed;
                return Err(Error::Transport("stream lease ended".into()));
            }
            result = tokio::time::timeout(self.timeout, self.response.chunk()) => result
                .map_err(|_| Error::Transport("video read timed out".into()))?
                .map_err(|_| Error::Transport("video read failed".into()))?,
        };
        match chunk {
            Some(chunk) => {
                if self.buffer.len().saturating_add(chunk.len()) > limit {
                    return Err(Error::InvalidResponse(
                        "multipart data exceeds limit".into(),
                    ));
                }
                self.buffer.extend_from_slice(&chunk);
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
        self.buffer.drain(..end + 4);
        let delimiter = [b"\r\n".as_slice(), self.boundary.as_slice()].concat();
        let size = if let Some(length) = length {
            if length > MAX_FRAME {
                return Err(Error::InvalidResponse("frame exceeds limit".into()));
            }
            while self.buffer.len() < length + delimiter.len() + 2 {
                if !self.fill(MAX_FRAME + MAX_HEADER).await? {
                    return Err(Error::InvalidResponse("truncated frame".into()));
                }
            }
            if !self.buffer[length..].starts_with(&delimiter) {
                return Err(Error::InvalidResponse("invalid frame boundary".into()));
            }
            length
        } else {
            self.until(&delimiter, MAX_FRAME + delimiter.len() + 2)
                .await?
        };
        if size > MAX_FRAME
            || !self.buffer[..size].starts_with(&[0xff, 0xd8])
            || !self.buffer[..size].ends_with(&[0xff, 0xd9])
        {
            return Err(Error::InvalidResponse("invalid JPEG frame".into()));
        }
        let frame = self.buffer[..size].to_vec();
        self.buffer.drain(..size + delimiter.len());
        while self.buffer.len() < 2 {
            if !self.fill(MAX_HEADER).await? {
                return Err(Error::InvalidResponse("truncated boundary".into()));
            }
        }
        if self.buffer.starts_with(b"--") {
            self.buffer.drain(..2);
            self.ended = true;
            self.lease.abort();
        } else if self.buffer.starts_with(b"\r\n") {
            self.buffer.drain(..2);
        } else {
            return Err(Error::InvalidResponse("invalid boundary suffix".into()));
        }
        Ok(Some(Snapshot { jpeg: frame }))
    }
}
