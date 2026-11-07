use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

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

struct Parser {
    response: reqwest::Response,
    boundary: Vec<u8>,
    buffer: Vec<u8>,
    pending: bytes::Bytes,
    storage: Option<std::sync::Arc<tokio::sync::OwnedSemaphorePermit>>,
    validation: Option<JoinHandle<Result<Vec<u8>>>>,
    scan_offset: usize,
    #[cfg(test)]
    scanned: usize,
    #[cfg(test)]
    scan_progress: Option<tokio::sync::mpsc::UnboundedSender<usize>>,
    lease: JoinHandle<()>,
    lease_state: watch::Receiver<bool>,
    timeout: Duration,
    first: bool,
    ended: bool,
    proven: Arc<AtomicBool>,
}

impl Drop for Parser {
    fn drop(&mut self) {
        self.lease.abort();
    }
}

fn storage_slot(
    slots: std::sync::Arc<tokio::sync::Semaphore>,
) -> Result<tokio::sync::OwnedSemaphorePermit> {
    slots
        .try_acquire_owned()
        .map_err(|_| Error::Transport("video stream capacity exhausted".into()))
}

pub(crate) async fn open(device: &Kvmd) -> Result<Box<dyn VideoStream>> {
    #[cfg(not(test))]
    let slots = {
        static SLOTS: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
            std::sync::OnceLock::new();
        SLOTS
            .get_or_init(|| {
                std::sync::Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS))
            })
            .clone()
    };
    #[cfg(test)]
    let slots = device.stream_storage.clone();
    let storage = storage_slot(slots)?;
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
    let parser = Parser {
        response,
        boundary,
        buffer: Vec::new(),
        pending: bytes::Bytes::new(),
        storage: Some(std::sync::Arc::new(storage)),
        validation: None,
        scan_offset: 0,
        #[cfg(test)]
        scanned: 0,
        #[cfg(test)]
        scan_progress: None,
        lease: guard.0.take().expect("lease exists"),
        lease_state,
        timeout: device.config.timeout,
        first: true,
        ended: false,
        proven: device.video_proven.clone(),
    };
    Ok(Box::new(Mjpeg::start(parser, device.config.timeout)))
}

struct Delivery {
    result: std::sync::Arc<std::sync::Mutex<Option<Result<Option<Snapshot>>>>>,
    acknowledged: tokio::sync::oneshot::Sender<bool>,
    finished: bool,
    claimed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    storage: std::sync::Weak<tokio::sync::OwnedSemaphorePermit>,
}

struct Mjpeg {
    requests: tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<Delivery>>,
    terminal: watch::Receiver<bool>,
    actor: JoinHandle<()>,
    final_result: Option<std::sync::Arc<std::sync::Mutex<Option<Result<Option<Snapshot>>>>>>,
    final_storage: Option<
        std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<tokio::sync::OwnedSemaphorePermit>>>>,
    >,
    idle: Duration,
    expiry: Option<JoinHandle<()>>,
}

impl Drop for Mjpeg {
    fn drop(&mut self) {
        self.actor.abort();
        if let Some(expiry) = self.expiry.take() {
            expiry.abort();
        }
    }
}

impl Mjpeg {
    fn start(mut parser: Parser, idle: Duration) -> Self {
        let (requests, mut incoming) =
            tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<Delivery>>(1);
        let (terminal_tx, terminal) = watch::channel(false);
        let actor = tokio::spawn(async move {
            let mut pending = None;
            let storage = parser.storage.clone().expect("stream storage exists");
            let deadline = tokio::time::sleep(idle);
            tokio::pin!(deadline);
            loop {
                let request = tokio::select! {
                    _ = &mut deadline => { let _ = terminal_tx.send(true); break; }
                    request = incoming.recv() => match request { Some(request) => request, None => break },
                };
                if request.is_closed() {
                    continue;
                }
                let result = match pending.take() {
                    Some(result) => result,
                    None => tokio::select! {
                        _ = &mut deadline => { let _ = terminal_tx.send(true); break; }
                        result = parser.next_frame() => result,
                    },
                };
                let finished = !matches!(&result, Ok(Some(_))) || parser.ended;
                let result = std::sync::Arc::new(std::sync::Mutex::new(Some(result)));
                let (acknowledged, ack) = tokio::sync::oneshot::channel();
                let claimed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let mut claimed_final = false;
                if request
                    .send(Delivery {
                        result: result.clone(),
                        acknowledged,
                        finished,
                        claimed: claimed.clone(),
                        storage: std::sync::Arc::downgrade(&storage),
                    })
                    .is_ok()
                {
                    tokio::select! {
                        _ = &mut deadline => {
                            let mut result = result.lock().expect("delivery lock poisoned");
                            if !claimed.load(std::sync::atomic::Ordering::Acquire) {
                                result.take();
                                let _ = terminal_tx.send(true);
                            }
                            break;
                        }
                        acknowledged = ack => { claimed_final = matches!(acknowledged, Ok(true)); }
                    }
                }
                if finished && claimed_final {
                    break;
                }
                pending = result.lock().expect("delivery lock poisoned").take();
                if pending.is_none() {
                    if finished {
                        break;
                    }
                    deadline.as_mut().reset(tokio::time::Instant::now() + idle);
                }
            }
        });
        Self {
            requests,
            terminal,
            actor,
            final_result: None,
            final_storage: None,
            idle,
            expiry: None,
        }
    }
}

#[async_trait]
impl VideoStream for Mjpeg {
    async fn next_frame(&mut self) -> Result<Option<Snapshot>> {
        if self.final_result.is_some() {
            let _ = (&mut self.actor).await;
            let result = self
                .final_result
                .take()
                .expect("final result exists")
                .lock()
                .expect("delivery lock poisoned")
                .take();
            if let Some(expiry) = self.expiry.take() {
                expiry.abort();
            }
            if let Some(storage) = self.final_storage.take() {
                storage.lock().expect("storage lock poisoned").take();
            }
            return result
                .unwrap_or_else(|| Err(Error::Transport("video consumer idle timeout".into())));
        }
        let (send, receive) = tokio::sync::oneshot::channel();
        if self.requests.send(send).await.is_err() {
            return if *self.terminal.borrow() {
                Err(Error::Transport("video consumer idle timeout".into()))
            } else {
                Ok(None)
            };
        }
        match receive.await {
            Ok(delivery) => {
                if delivery.finished {
                    {
                        let result = delivery.result.lock().expect("delivery lock poisoned");
                        if result.is_none() {
                            return Err(Error::Transport("video consumer idle timeout".into()));
                        }
                        let storage = std::sync::Arc::new(std::sync::Mutex::new(Some(
                            delivery
                                .storage
                                .upgrade()
                                .expect("live delivery owns storage"),
                        )));
                        self.final_storage = Some(storage.clone());
                        let result = std::sync::Arc::downgrade(&delivery.result);
                        let storage = std::sync::Arc::downgrade(&storage);
                        let idle = self.idle;
                        self.expiry = Some(tokio::spawn(async move {
                            tokio::time::sleep(idle).await;
                            if let Some(result) = result.upgrade() {
                                let mut result = result.lock().expect("delivery lock poisoned");
                                result.take();
                                if let Some(storage) = storage.upgrade() {
                                    storage.lock().expect("storage lock poisoned").take();
                                }
                            }
                        }));
                        delivery
                            .claimed
                            .store(true, std::sync::atomic::Ordering::Release);
                    }
                    self.final_result = Some(delivery.result);
                    let _ = delivery.acknowledged.send(true);
                    let _ = (&mut self.actor).await;
                    let result = self
                        .final_result
                        .take()
                        .expect("final result exists")
                        .lock()
                        .expect("delivery lock poisoned")
                        .take();
                    if let Some(expiry) = self.expiry.take() {
                        expiry.abort();
                    }
                    if let Some(storage) = self.final_storage.take() {
                        storage.lock().expect("storage lock poisoned").take();
                    }
                    return result.unwrap_or_else(|| {
                        Err(Error::Transport("video consumer idle timeout".into()))
                    });
                }
                let result = delivery
                    .result
                    .lock()
                    .expect("delivery lock poisoned")
                    .take();
                let _ = delivery.acknowledged.send(false);
                match result {
                    Some(result) => result,
                    None => Err(Error::Transport("video consumer idle timeout".into())),
                }
            }
            Err(_) if *self.terminal.borrow() => {
                Err(Error::Transport("video consumer idle timeout".into()))
            }
            Err(_) => Ok(None),
        }
    }
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

impl Parser {
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
impl VideoStream for Parser {
    async fn next_frame(&mut self) -> Result<Option<Snapshot>> {
        let result = self.read_frame().await;
        if result.is_err() || self.ended {
            self.ended = true;
            self.lease.abort();
            self.storage = None;
            self.buffer = Vec::new();
            self.pending = bytes::Bytes::new();
        }
        result
    }
}

impl Parser {
    async fn read_frame(&mut self) -> Result<Option<Snapshot>> {
        if self.validation.is_some() {
            let result = self.validation.as_mut().expect("validation exists").await;
            self.validation = None;
            let jpeg =
                result.map_err(|_| Error::Transport("JPEG validation task failed".into()))??;
            self.proven.store(true, Ordering::Release);
            return Ok(Some(Snapshot { jpeg }));
        }
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
            self.scan_offset = self.scan_offset.max(body_start);
            loop {
                #[cfg(test)]
                {
                    self.scanned += self.buffer.len() - self.scan_offset;
                }
                if let Some(relative) = find(&self.buffer[self.scan_offset..], &delimiter) {
                    let pos = self.scan_offset + relative;
                    self.scan_offset = pos;
                    while self.buffer.len() < pos + delimiter.len() + 2 {
                        if !self.fill(frame_limit).await? {
                            return Err(Error::InvalidResponse("truncated boundary".into()));
                        }
                    }
                    let suffix = &self.buffer[pos + delimiter.len()..pos + delimiter.len() + 2];
                    if suffix == b"\r\n" || suffix == b"--" {
                        break pos - body_start;
                    }
                    self.scan_offset = pos + 1;
                } else {
                    if self.buffer.len() >= frame_limit {
                        return Err(Error::InvalidResponse("frame exceeds limit".into()));
                    }
                    self.scan_offset =
                        next_scan_offset(self.buffer.len(), delimiter.len(), body_start);
                    #[cfg(test)]
                    if let Some(progress) = &self.scan_progress {
                        let _ = progress.send(self.buffer.len());
                    }
                    if !self.fill(frame_limit).await? {
                        return Err(Error::InvalidResponse("truncated frame".into()));
                    }
                }
            }
        };
        if size > MAX_FRAME {
            return Err(Error::InvalidResponse("frame exceeds limit".into()));
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
        self.scan_offset = 0;
        let storage = self.storage.clone();
        self.validation = Some(tokio::spawn(async move {
            let _storage = storage;
            let permit = crate::jpeg_permit().await?;
            crate::validate_jpeg_async(frame, permit).await
        }));
        let result = self.validation.as_mut().expect("validation exists").await;
        self.validation = None;
        let jpeg = result.map_err(|_| Error::Transport("JPEG validation task failed".into()))??;
        self.proven.store(true, Ordering::Release);
        Ok(Some(Snapshot { jpeg }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };

    #[test]
    fn stream_capacity_is_explicit_and_released_on_drop() {
        let capacity = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let permit = storage_slot(capacity.clone()).unwrap();
        assert!(storage_slot(capacity.clone()).is_err());
        drop(permit);
        assert!(storage_slot(capacity).is_ok());
    }

    #[tokio::test]
    async fn repeated_cancellation_keeps_fragment_scan_linear() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
            sync::mpsc,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (send, mut receive) = mpsc::channel::<bool>(1);
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode(&[64, 128, 192], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n--frame\r\nContent-Type: image/jpeg\r\n\r\n\xff\xd8").await.unwrap();
            while let Some(last) = receive.recv().await {
                if last {
                    socket.write_all(&jpeg[2..]).await.unwrap();
                    socket.write_all(b"\r\n--frame--\r\n").await.unwrap();
                    break;
                }
                socket.write_all(&[0xff, 0xfe, 0x01, 0x00]).await.unwrap();
                socket.write_all(&[b'a'; 254]).await.unwrap();
            }
        });
        let response = reqwest::get(format!("http://{address}/")).await.unwrap();
        let (_tx, lease_state) = watch::channel(true);
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        let lease = tokio::spawn(std::future::pending());
        let mut video = Parser {
            response,
            boundary: b"--frame".to_vec(),
            buffer: Vec::new(),
            pending: bytes::Bytes::new(),
            storage: Some(std::sync::Arc::new(
                storage_slot(std::sync::Arc::new(tokio::sync::Semaphore::new(1))).unwrap(),
            )),
            validation: None,
            scan_offset: 0,
            scanned: 0,
            scan_progress: Some(progress_tx),
            lease,
            lease_state,
            timeout: Duration::from_secs(3),
            first: true,
            ended: false,
            proven: Arc::new(AtomicBool::new(false)),
        };
        assert!(!video.proven.load(Ordering::Acquire));
        for index in 1..=64 {
            send.send(false).await.unwrap();
            let frame = video.next_frame();
            tokio::pin!(frame);
            loop {
                tokio::select! {
                    result = &mut frame => panic!("frame ended before cancellation: {result:?}"),
                    progress = progress_rx.recv() => {
                        if progress.unwrap() >= 2 + index * 258 { break; }
                    }
                }
            }
        }
        send.send(true).await.unwrap();
        let frame = video.next_frame().await.unwrap().unwrap().jpeg;
        assert!(frame.len() > 64 * 258);
        assert!(video.proven.load(Ordering::Acquire));
        assert!(video.scanned < frame.len() + 64 * b"\r\n--frame".len() * 4);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn actor_idle_releases_storage_and_reports_timeout() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            socket.read(&mut request).await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let response = reqwest::get(format!("http://{address}/")).await.unwrap();
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let storage = slots.clone().try_acquire_owned().unwrap();
        let (_tx, lease_state) = watch::channel(true);
        let parser = Parser {
            response,
            boundary: b"--frame".to_vec(),
            buffer: Vec::new(),
            pending: bytes::Bytes::new(),
            storage: Some(std::sync::Arc::new(storage)),
            validation: None,
            scan_offset: 0,
            scanned: 0,
            scan_progress: None,
            lease: tokio::spawn(std::future::pending()),
            lease_state,
            timeout: Duration::from_secs(1),
            first: true,
            ended: false,
            proven: Arc::new(AtomicBool::new(false)),
        };
        let mut stream = Mjpeg::start(parser, Duration::from_millis(20));
        tokio::time::timeout(Duration::from_secs(1), async {
            while slots.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(stream.next_frame().await.is_err());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn actor_times_out_during_trickled_frame() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            socket.read(&mut request).await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n--frame\r\n").await.unwrap();
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
                if socket.write_all(b"a").await.is_err() {
                    break;
                }
            }
        });
        let response = reqwest::get(format!("http://{address}/")).await.unwrap();
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let (_tx, lease_state) = watch::channel(true);
        let parser = Parser {
            response,
            boundary: b"--frame".to_vec(),
            buffer: Vec::new(),
            pending: bytes::Bytes::new(),
            storage: Some(std::sync::Arc::new(
                slots.clone().try_acquire_owned().unwrap(),
            )),
            validation: None,
            scan_offset: 0,
            scanned: 0,
            scan_progress: None,
            lease: tokio::spawn(std::future::pending()),
            lease_state,
            timeout: Duration::from_secs(1),
            first: true,
            ended: false,
            proven: Arc::new(AtomicBool::new(false)),
        };
        let mut stream = Mjpeg::start(parser, Duration::from_millis(40));
        assert!(
            tokio::time::timeout(Duration::from_secs(1), stream.next_frame())
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(slots.available_permits(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn cancelled_terminal_join_preserves_frame_until_cleanup() {
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let permit = std::sync::Arc::new(slots.clone().try_acquire_owned().unwrap());
        let (requests, mut incoming) =
            tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<Delivery>>(1);
        let (_terminal_tx, terminal) = watch::channel(false);
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let actor = tokio::spawn(async move {
            let request = incoming.recv().await.unwrap();
            let (acknowledged, ack) = tokio::sync::oneshot::channel();
            request
                .send(Delivery {
                    result: std::sync::Arc::new(std::sync::Mutex::new(Some(Ok(Some(Snapshot {
                        jpeg: vec![1, 2, 3],
                    }))))),
                    acknowledged,
                    finished: true,
                    claimed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    storage: std::sync::Arc::downgrade(&permit),
                })
                .unwrap_or_else(|_| panic!("request closed"));
            assert!(ack.await.unwrap());
            ack_tx.send(()).unwrap();
            release_rx.await.unwrap();
            drop(permit);
        });
        let mut stream = Mjpeg {
            requests,
            terminal,
            actor,
            final_result: None,
            final_storage: None,
            expiry: None,
            idle: Duration::from_secs(3),
        };
        let mut first = Box::pin(stream.next_frame());
        tokio::select! {
            result = &mut first => panic!("completed before cleanup: {result:?}"),
            result = ack_rx => result.unwrap(),
        }
        drop(first);
        assert_eq!(slots.available_permits(), 0);
        release_tx.send(()).unwrap();
        while !stream.actor.is_finished() {
            tokio::task::yield_now().await;
        }
        assert_eq!(slots.available_permits(), 0);
        let expiry = stream.expiry.as_ref().unwrap().abort_handle();
        assert_eq!(
            stream.next_frame().await.unwrap().unwrap().jpeg,
            vec![1, 2, 3]
        );
        assert!(stream.expiry.is_none());
        tokio::task::yield_now().await;
        assert!(expiry.is_finished());
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn cancelled_terminal_result_drop_releases_capacity() {
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let permit = std::sync::Arc::new(slots.clone().try_acquire_owned().unwrap());
        let (requests, mut incoming) =
            tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<Delivery>>(1);
        let (_terminal_tx, terminal) = watch::channel(false);
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let actor = tokio::spawn(async move {
            let request = incoming.recv().await.unwrap();
            let (acknowledged, ack) = tokio::sync::oneshot::channel();
            request
                .send(Delivery {
                    result: std::sync::Arc::new(std::sync::Mutex::new(Some(Ok(Some(Snapshot {
                        jpeg: vec![1, 2, 3],
                    }))))),
                    acknowledged,
                    finished: true,
                    claimed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    storage: std::sync::Arc::downgrade(&permit),
                })
                .unwrap_or_else(|_| panic!("request closed"));
            assert!(ack.await.unwrap());
            ack_tx.send(()).unwrap();
            release_rx.await.unwrap();
            drop(permit);
        });
        let mut stream = Mjpeg {
            requests,
            terminal,
            actor,
            final_result: None,
            final_storage: None,
            expiry: None,
            idle: Duration::from_secs(60),
        };
        let mut first = Box::pin(stream.next_frame());
        tokio::select! { result = &mut first => panic!("completed before cleanup: {result:?}"), result = ack_rx => result.unwrap() }
        drop(first);
        release_tx.send(()).unwrap();
        while !stream.actor.is_finished() {
            tokio::task::yield_now().await;
        }
        assert_eq!(slots.available_permits(), 0);
        let expiry = stream.expiry.as_ref().unwrap().abort_handle();
        drop(stream);
        assert_eq!(slots.available_permits(), 1);
        tokio::task::yield_now().await;
        assert!(expiry.is_finished());
    }

    #[tokio::test]
    async fn cancelled_terminal_result_expires_without_repoll() {
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let permit = std::sync::Arc::new(slots.clone().try_acquire_owned().unwrap());
        let (requests, mut incoming) =
            tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<Delivery>>(1);
        let (_terminal_tx, terminal) = watch::channel(false);
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let actor = tokio::spawn(async move {
            let request = incoming.recv().await.unwrap();
            let (acknowledged, ack) = tokio::sync::oneshot::channel();
            request
                .send(Delivery {
                    result: std::sync::Arc::new(std::sync::Mutex::new(Some(Ok(Some(Snapshot {
                        jpeg: vec![1, 2, 3],
                    }))))),
                    acknowledged,
                    finished: true,
                    claimed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    storage: std::sync::Arc::downgrade(&permit),
                })
                .unwrap_or_else(|_| panic!("request closed"));
            assert!(ack.await.unwrap());
            ack_tx.send(()).unwrap();
            release_rx.await.unwrap();
            drop(permit);
        });
        let mut stream = Mjpeg {
            requests,
            terminal,
            actor,
            final_result: None,
            final_storage: None,
            expiry: None,
            idle: Duration::from_millis(30),
        };
        let mut first = Box::pin(stream.next_frame());
        tokio::select! { result = &mut first => panic!("completed before cleanup: {result:?}"), result = ack_rx => result.unwrap() }
        drop(first);
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while slots.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(stream.next_frame().await.is_err());
    }

    #[tokio::test]
    async fn unpolled_delivery_expires_and_releases_capacity() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode(&[64, 128, 192], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            socket.read(&mut request).await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n--frame\r\nContent-Type: image/jpeg\r\n\r\n").await.unwrap();
            socket.write_all(&jpeg).await.unwrap();
            socket.write_all(b"\r\n--frame--\r\n").await.unwrap();
        });
        let response = reqwest::get(format!("http://{address}/")).await.unwrap();
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let (_tx, lease_state) = watch::channel(true);
        let parser = Parser {
            response,
            boundary: b"--frame".to_vec(),
            buffer: Vec::new(),
            pending: bytes::Bytes::new(),
            storage: Some(std::sync::Arc::new(
                slots.clone().try_acquire_owned().unwrap(),
            )),
            validation: None,
            scan_offset: 0,
            scanned: 0,
            scan_progress: None,
            lease: tokio::spawn(std::future::pending()),
            lease_state,
            timeout: Duration::from_secs(1),
            first: true,
            ended: false,
            proven: Arc::new(AtomicBool::new(false)),
        };
        let stream = Mjpeg::start(parser, Duration::from_secs(2));
        let (send, mut receive) = tokio::sync::oneshot::channel();
        stream.requests.send(send).await.unwrap();
        let delivery = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match receive.try_recv() {
                    Ok(delivery) => break delivery,
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                        tokio::task::yield_now().await
                    }
                    Err(error) => panic!("delivery failed: {error}"),
                }
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while slots.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(delivery.result.lock().unwrap().is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn actor_delivers_final_frame_after_cancelled_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode(&[64, 128, 192], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        let expected = jpeg.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            socket.read(&mut request).await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n").await.unwrap();
            wait.await.unwrap();
            socket
                .write_all(b"--frame\r\nContent-Type: image/jpeg\r\n\r\n")
                .await
                .unwrap();
            socket.write_all(&jpeg).await.unwrap();
            socket.write_all(b"\r\n--frame--\r\n").await.unwrap();
        });
        let response = reqwest::get(format!("http://{address}/")).await.unwrap();
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let storage = slots.clone().try_acquire_owned().unwrap();
        let (_tx, lease_state) = watch::channel(true);
        let parser = Parser {
            response,
            boundary: b"--frame".to_vec(),
            buffer: Vec::new(),
            pending: bytes::Bytes::new(),
            storage: Some(std::sync::Arc::new(storage)),
            validation: None,
            scan_offset: 0,
            scanned: 0,
            scan_progress: None,
            lease: tokio::spawn(std::future::pending()),
            lease_state,
            timeout: Duration::from_secs(3),
            first: true,
            ended: false,
            proven: Arc::new(AtomicBool::new(false)),
        };
        let mut stream = Mjpeg::start(parser, Duration::from_secs(3));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), stream.next_frame())
                .await
                .is_err()
        );
        release.send(()).unwrap();
        assert_eq!(stream.next_frame().await.unwrap().unwrap().jpeg, expected);
        assert!(stream.next_frame().await.unwrap().is_none());
        assert_eq!(slots.available_permits(), 1);
        server.await.unwrap();
    }

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
