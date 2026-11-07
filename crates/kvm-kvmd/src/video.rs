use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use kvm_core::{Error, Result, Snapshot, VideoStream};
use reqwest_websocket::{Message, RequestBuilderExt};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    task::JoinHandle,
};

use crate::Kvmd;

// Limits bound memory consumed by an untrusted device independently of its claims.
const MAX_HEADER: usize = 16 * 1024;
const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_BOUNDARY: usize = 70;
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// The most one multipart part occupies in a stream's buffer: its headers and blank line (`until` bounds both by `MAX_HEADER`), the largest frame, the delimiter (`\r\n--` and the longest boundary) and the two-byte suffix after it.
const MAX_PART: usize = MAX_HEADER + MAX_FRAME + 4 + MAX_BOUNDARY + 2;
/// The compressed bytes that every MJPEG stream in the process may hold from devices at once. At most `MAX_JPEG_VALIDATIONS` frames are decoded at a time, so storage for that many of the largest parts keeps every decoder slot supplied with a frame; more would only queue frames behind the decoders. Streams with smaller parts share the same bytes, so the number of open streams is not limited. Decoder working memory is bounded separately, by `MAX_JPEG_VALIDATIONS` validations of at most `MAX_JPEG_WORKING_BYTES` each, while the compressed frame being decoded stays charged here.
pub(crate) const STREAM_STORAGE_BYTES: usize = crate::MAX_JPEG_VALIDATIONS * MAX_PART;

/// A budget of compressed bytes shared by MJPEG streams, and the streams waiting for it.
///
/// A stream charges the bytes it holds from the device: its buffer's allocation, the network chunk it is consuming, and each frame it has parsed until a consumer takes it or the frame is dropped. A stream that cannot charge what it needs registers as waiting, and other streams yield to waiters so that neither idle nor partly read streams can starve it:
///
/// - a stream with no consumer request outstanding releases everything it holds while any stream waits, dropping an unclaimed frame and any partial part, and resynchronises on the next delimiter when it is read again;
/// - only the oldest waiting stream acquires storage; a younger waiting stream releases its partial part and waits until no older stream is waiting, so waiting streams never hold storage each other need.
///
/// The oldest waiter therefore waits only for streams that are reading or validating, which release their storage when their frame is delivered, ends or times out.
pub(crate) struct StreamStorage {
    bytes: Arc<Semaphore>,
    capacity: usize,
    waiting: watch::Sender<BTreeSet<u64>>,
    next_stream: AtomicU64,
}

impl StreamStorage {
    /// Creates a budget of `capacity` bytes with no streams.
    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            bytes: Arc::new(Semaphore::new(capacity)),
            capacity,
            waiting: watch::Sender::new(BTreeSet::new()),
            next_stream: AtomicU64::new(0),
        })
    }

    /// The bytes no stream has charged.
    #[cfg(test)]
    pub(crate) fn available(&self) -> usize {
        self.bytes.available_permits()
    }

    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }
}

fn storage_exceeded() -> Error {
    Error::InvalidResponse("video part exceeds stream storage".into())
}

fn storage_reclaimed() -> Error {
    Error::Transport("video frame storage reclaimed for a waiting stream".into())
}

/// A stream's charge against its `StreamStorage`, released when dropped.
struct Ledger {
    storage: Arc<StreamStorage>,
    /// Opening order: a lower id was opened earlier and takes precedence while streams wait.
    id: u64,
    held: OwnedSemaphorePermit,
}

impl Ledger {
    fn new(storage: Arc<StreamStorage>) -> Self {
        let id = storage.next_stream.fetch_add(1, Ordering::Relaxed);
        let held = storage
            .bytes
            .clone()
            .try_acquire_many_owned(0)
            .expect("stream storage is never closed");
        Self { storage, id, held }
    }
}

/// Registers a stream as waiting for storage until dropped.
struct Waiting {
    storage: Arc<StreamStorage>,
    id: u64,
}

impl Waiting {
    fn new(storage: Arc<StreamStorage>, id: u64) -> Self {
        storage.waiting.send_modify(|waiting| {
            waiting.insert(id);
        });
        Self { storage, id }
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.storage.waiting.send_modify(|waiting| {
            waiting.remove(&self.id);
        });
    }
}

/// A validated frame and the storage charged for it, which is released when the frame is handed to a consumer or dropped.
struct Frame {
    jpeg: Vec<u8>,
    _charge: OwnedSemaphorePermit,
}

impl Frame {
    fn into_snapshot(self) -> Snapshot {
        Snapshot { jpeg: self.jpeg }
    }
}

/// Why reading a part stopped: storage was reclaimed, discarding what the stream held, so the read restarts; or the stream failed.
enum ReadError {
    Reclaimed,
    Failed(Error),
}

impl From<Error> for ReadError {
    fn from(error: Error) -> Self {
        Self::Failed(error)
    }
}

type Read<T> = std::result::Result<T, ReadError>;

struct Parser {
    response: reqwest::Response,
    boundary: Vec<u8>,
    buffer: Vec<u8>,
    pending: bytes::Bytes,
    /// The length of the network chunk `pending` was cut from, whose allocation stays alive while any of it is unconsumed; zero when `pending` is empty.
    pending_alloc: usize,
    ledger: Ledger,
    /// Set when discarded bytes left the parser inside a part, so it must skip to the next delimiter.
    resync: bool,
    validation: Option<JoinHandle<Result<Frame>>>,
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

#[cfg(not(test))]
fn shared_storage() -> Arc<StreamStorage> {
    static STORAGE: std::sync::OnceLock<Arc<StreamStorage>> = std::sync::OnceLock::new();
    STORAGE
        .get_or_init(|| StreamStorage::new(STREAM_STORAGE_BYTES))
        .clone()
}

pub(crate) async fn open(device: &Kvmd) -> Result<Box<dyn VideoStream>> {
    #[cfg(not(test))]
    let storage = shared_storage();
    #[cfg(test)]
    let storage = device.stream_storage.clone();
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
    let parser = Parser::new(
        response,
        boundary,
        guard.0.take().expect("lease exists"),
        lease_state,
        device.config.timeout,
        device.video_proven.clone(),
        storage,
    );
    Ok(Box::new(Mjpeg::start(parser, device.config.timeout)))
}

type SharedResult = Arc<std::sync::Mutex<Option<Result<Option<Frame>>>>>;

struct Delivery {
    result: SharedResult,
    acknowledged: tokio::sync::oneshot::Sender<bool>,
    finished: bool,
    claimed: Arc<AtomicBool>,
}

struct Mjpeg {
    requests: tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<Delivery>>,
    terminal: watch::Receiver<bool>,
    actor: JoinHandle<()>,
    final_result: Option<SharedResult>,
    storage: Arc<StreamStorage>,
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

fn idle_timeout() -> Error {
    Error::Transport("video consumer idle timeout".into())
}

fn to_snapshot(result: Result<Option<Frame>>) -> Result<Option<Snapshot>> {
    result.map(|frame| frame.map(Frame::into_snapshot))
}

impl Mjpeg {
    fn start(mut parser: Parser, idle: Duration) -> Self {
        let storage = parser.ledger.storage.clone();
        let mut pressure = storage.waiting.subscribe();
        let (requests, mut incoming) =
            tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<Delivery>>(1);
        let (terminal_tx, terminal) = watch::channel(false);
        let actor = tokio::spawn(async move {
            let mut pending: Option<Result<Option<Frame>>> = None;
            let deadline = tokio::time::sleep(idle);
            tokio::pin!(deadline);
            loop {
                let holds_storage =
                    parser.ledger.held.num_permits() > 0 || matches!(pending, Some(Ok(Some(_))));
                let request = tokio::select! {
                    _ = &mut deadline => { let _ = terminal_tx.send(true); break; }
                    request = incoming.recv() => match request { Some(request) => request, None => break },
                    // No consumer request is outstanding, so this stream is idle and yields what it holds to the waiting stream.
                    _ = pressure.wait_for(|waiting| !waiting.is_empty()), if holds_storage => {
                        parser.discard();
                        if matches!(pending, Some(Ok(Some(_)))) {
                            // A later frame supersedes an unclaimed one, but the final frame has no successor, so its loss is reported.
                            pending = parser.ended.then(|| Err(storage_reclaimed()));
                        }
                        continue;
                    }
                };
                if request.is_closed() {
                    continue;
                }
                let result = match pending.take() {
                    Some(result) => result,
                    None => tokio::select! {
                        _ = &mut deadline => { let _ = terminal_tx.send(true); break; }
                        result = parser.next_charged() => result,
                    },
                };
                let finished = !matches!(&result, Ok(Some(_))) || parser.ended;
                let result = Arc::new(std::sync::Mutex::new(Some(result)));
                let (acknowledged, ack) = tokio::sync::oneshot::channel();
                let claimed = Arc::new(AtomicBool::new(false));
                let mut claimed_final = false;
                if request
                    .send(Delivery {
                        result: result.clone(),
                        acknowledged,
                        finished,
                        claimed: claimed.clone(),
                    })
                    .is_ok()
                {
                    tokio::select! {
                        _ = &mut deadline => {
                            let mut result = result.lock().expect("delivery lock poisoned");
                            if !claimed.load(Ordering::Acquire) {
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
            storage,
            idle,
            expiry: None,
        }
    }

    /// Joins the finished actor and takes the final result, which the expiry task may have reclaimed or expired meanwhile.
    async fn take_final(&mut self) -> Result<Option<Snapshot>> {
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
        result.map_or_else(|| Err(idle_timeout()), to_snapshot)
    }
}

#[async_trait]
impl VideoStream for Mjpeg {
    async fn next_frame(&mut self) -> Result<Option<Snapshot>> {
        if self.final_result.is_some() {
            return self.take_final().await;
        }
        let (send, receive) = tokio::sync::oneshot::channel();
        if self.requests.send(send).await.is_err() {
            return if *self.terminal.borrow() {
                Err(idle_timeout())
            } else {
                Ok(None)
            };
        }
        match receive.await {
            Ok(delivery) => {
                if delivery.finished {
                    {
                        if delivery
                            .result
                            .lock()
                            .expect("delivery lock poisoned")
                            .is_none()
                        {
                            return Err(idle_timeout());
                        }
                        let result = Arc::downgrade(&delivery.result);
                        let idle = self.idle;
                        let mut pressure = self.storage.waiting.subscribe();
                        // The final result is kept for this consumer across cancellation, but no longer than the idle timeout, and a final frame's storage goes to any stream that waits for it.
                        self.expiry = Some(tokio::spawn(async move {
                            let expired = tokio::time::sleep(idle);
                            tokio::pin!(expired);
                            let reclaim = tokio::select! {
                                () = &mut expired => false,
                                _ = pressure.wait_for(|waiting| !waiting.is_empty()) => true,
                            };
                            if reclaim {
                                if let Some(result) = result.upgrade() {
                                    let mut result = result.lock().expect("delivery lock poisoned");
                                    if matches!(*result, Some(Ok(Some(_)))) {
                                        *result = Some(Err(storage_reclaimed()));
                                    }
                                }
                                expired.await;
                            }
                            if let Some(result) = result.upgrade() {
                                result.lock().expect("delivery lock poisoned").take();
                            }
                        }));
                        delivery.claimed.store(true, Ordering::Release);
                    }
                    self.final_result = Some(delivery.result);
                    let _ = delivery.acknowledged.send(true);
                    return self.take_final().await;
                }
                let result = delivery
                    .result
                    .lock()
                    .expect("delivery lock poisoned")
                    .take();
                let _ = delivery.acknowledged.send(false);
                result.map_or_else(|| Err(idle_timeout()), to_snapshot)
            }
            Err(_) if *self.terminal.borrow() => Err(idle_timeout()),
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
    fn new(
        response: reqwest::Response,
        boundary: Vec<u8>,
        lease: JoinHandle<()>,
        lease_state: watch::Receiver<bool>,
        timeout: Duration,
        proven: Arc<AtomicBool>,
        storage: Arc<StreamStorage>,
    ) -> Self {
        Self {
            response,
            boundary,
            buffer: Vec::new(),
            pending: bytes::Bytes::new(),
            pending_alloc: 0,
            ledger: Ledger::new(storage),
            resync: false,
            validation: None,
            scan_offset: 0,
            #[cfg(test)]
            scanned: 0,
            #[cfg(test)]
            scan_progress: None,
            lease,
            lease_state,
            timeout,
            first: true,
            ended: false,
            proven,
        }
    }

    /// The device bytes this stream holds outside frames it has parsed: the buffer's allocation and the chunk `pending` keeps alive.
    fn footprint(&self) -> usize {
        self.buffer.capacity() + self.pending_alloc
    }

    /// Releases the charge beyond the footprint. Every allocation is charged before it is made, so the charge never falls short of the footprint.
    fn settle(&mut self) {
        let footprint = self.footprint();
        debug_assert!(
            footprint <= self.ledger.held.num_permits(),
            "stream holds uncharged bytes"
        );
        let excess = self.ledger.held.num_permits().saturating_sub(footprint);
        drop(self.ledger.held.split(excess));
    }

    /// Drops every device byte this stream holds outside parsed frames and releases their charge. Dropping bytes leaves the parser inside a part, so it then skips to the next delimiter.
    fn discard(&mut self) {
        if !self.buffer.is_empty() || !self.pending.is_empty() {
            self.resync = true;
            self.first = false;
        }
        self.buffer = Vec::new();
        self.pending = bytes::Bytes::new();
        self.pending_alloc = 0;
        self.scan_offset = 0;
        self.settle();
    }

    /// Charges the stream for holding `need` bytes in all, or `prefer` bytes when that many are free at once, waiting at most the read timeout. Only the oldest waiting stream acquires: while an older stream waits, this one releases what it holds, returning `Reclaimed`, or defers.
    async fn reserve(&mut self, need: usize, prefer: usize) -> Read<()> {
        let storage = self.ledger.storage.clone();
        if need > storage.capacity {
            return Err(storage_exceeded().into());
        }
        let permits = |count: usize| u32::try_from(count).map_err(|_| storage_exceeded());
        let id = self.ledger.id;
        let older_waiting =
            move |waiting: &BTreeSet<u64>| waiting.first().is_some_and(|first| *first < id);
        let deadline = tokio::time::Instant::now() + self.timeout;
        let exhausted =
            || ReadError::Failed(Error::Transport("video stream storage exhausted".into()));
        let mut pressure = storage.waiting.subscribe();
        let mut waiting = None;
        loop {
            let held = self.ledger.held.num_permits();
            if need <= held {
                return Ok(());
            }
            for total in [
                if prefer <= storage.capacity {
                    prefer
                } else {
                    need
                },
                need,
            ] {
                if let Ok(permit) = storage
                    .bytes
                    .clone()
                    .try_acquire_many_owned(permits(total - held)?)
                {
                    self.ledger.held.merge(permit);
                    return Ok(());
                }
            }
            if waiting.is_none() {
                waiting = Some(Waiting::new(storage.clone(), id));
            }
            let deferred = older_waiting(&pressure.borrow());
            if deferred {
                if held > 0 {
                    self.discard();
                    return Err(ReadError::Reclaimed);
                }
                tokio::select! {
                    _ = pressure.wait_for(|waiting| !older_waiting(waiting)) => continue,
                    () = tokio::time::sleep_until(deadline) => return Err(exhausted()),
                }
            }
            tokio::select! {
                permit = storage.bytes.clone().acquire_many_owned(permits(need - held)?) => {
                    self.ledger.held.merge(permit.expect("stream storage is never closed"));
                    return Ok(());
                }
                _ = pressure.wait_for(older_waiting) => {}
                () = tokio::time::sleep_until(deadline) => return Err(exhausted()),
            }
        }
    }

    /// Moves device bytes into the buffer, which never exceeds `limit` bytes. With `exact`, `limit` is the known end of the part and the buffer grows straight to it; otherwise it doubles while that storage is free, so the copying stays linear in the part's length, and grows only as far as needed when it is not.
    async fn fill(&mut self, limit: usize, exact: bool) -> Read<bool> {
        if !*self.lease_state.borrow() {
            return Err(Error::Transport("stream lease ended".into()).into());
        }
        if self.pending.is_empty() {
            let chunk = tokio::select! {
                changed = self.lease_state.changed() => {
                    let _ = changed;
                    return Err(Error::Transport("stream lease ended".into()).into());
                }
                result = tokio::time::timeout(self.timeout, self.response.chunk()) => result
                    .map_err(|_| Error::Transport("video read timed out".into()))?
                    .map_err(|error| crate::request_error("video read", &error))?,
            };
            match chunk {
                Some(chunk) if chunk.is_empty() => return Ok(true),
                Some(chunk) => {
                    // The transport has already read the chunk, so it is charged before it is absorbed rather than before it is received.
                    self.pending_alloc = chunk.len();
                    self.pending = chunk;
                }
                None => return Ok(false),
            }
        }
        let count = self
            .pending
            .len()
            .min(limit.saturating_sub(self.buffer.len()));
        if count == 0 {
            return Err(Error::InvalidResponse("multipart data exceeds limit".into()).into());
        }
        let len = self.buffer.len() + count;
        let capacity = self.buffer.capacity();
        let (need, prefer) = if len <= capacity {
            (capacity, capacity)
        } else if exact {
            (limit, limit)
        } else {
            (len, len.max(capacity * 2).min(limit))
        };
        self.reserve(need + self.pending_alloc, prefer + self.pending_alloc)
            .await?;
        let target = if self.ledger.held.num_permits() >= prefer + self.pending_alloc {
            prefer
        } else {
            need
        };
        self.buffer.reserve_exact(target - self.buffer.len());
        let chunk = std::mem::take(&mut self.pending);
        absorb_chunk(&mut self.buffer, &mut self.pending, chunk, limit)?;
        if self.pending.is_empty() {
            self.pending_alloc = 0;
        }
        self.settle();
        Ok(true)
    }

    async fn until(&mut self, needle: &[u8], limit: usize) -> Read<usize> {
        loop {
            if let Some(pos) = find(&self.buffer, needle) {
                if pos + needle.len() > limit {
                    return Err(
                        Error::InvalidResponse("multipart data exceeds limit".into()).into(),
                    );
                }
                return Ok(pos);
            }
            if self.buffer.len() >= limit {
                return Err(Error::InvalidResponse("multipart data exceeds limit".into()).into());
            }
            if !self.fill(limit, false).await? {
                return Err(Error::InvalidResponse("truncated multipart stream".into()).into());
            }
        }
    }

    /// Reads the next frame, carrying its storage charge. A final boundary or failure ends the stream and releases everything it holds from the device.
    async fn next_charged(&mut self) -> Result<Option<Frame>> {
        let result = self.read_frame().await;
        if result.is_err() || self.ended {
            self.ended = true;
            self.lease.abort();
            self.buffer = Vec::new();
            self.pending = bytes::Bytes::new();
            self.pending_alloc = 0;
            self.settle();
        }
        result
    }

    async fn read_frame(&mut self) -> Result<Option<Frame>> {
        loop {
            match self.read_part().await {
                Ok(frame) => return Ok(frame),
                Err(ReadError::Failed(error)) => return Err(error),
                Err(ReadError::Reclaimed) => {}
            }
        }
    }
}

#[cfg(test)]
#[async_trait]
impl VideoStream for Parser {
    async fn next_frame(&mut self) -> Result<Option<Snapshot>> {
        to_snapshot(self.next_charged().await)
    }
}

impl Parser {
    /// Skips to the start of the next part's headers after discarded bytes, keeping only enough of the buffer to find a delimiter split across reads.
    async fn resynchronise(&mut self) -> Read<Option<()>> {
        let delimiter = [b"\r\n".as_slice(), self.boundary.as_slice()].concat();
        loop {
            if let Some(relative) = find(&self.buffer[self.scan_offset..], &delimiter) {
                let pos = self.scan_offset + relative;
                let end = pos + delimiter.len() + 2;
                while self.buffer.len() < end {
                    if !self.fill(end, false).await? {
                        return Err(Error::InvalidResponse("truncated boundary".into()).into());
                    }
                }
                let suffix = [self.buffer[end - 2], self.buffer[end - 1]];
                match &suffix {
                    b"\r\n" => {
                        self.buffer.drain(..end);
                        self.scan_offset = 0;
                        self.resync = false;
                        return Ok(Some(()));
                    }
                    b"--" => {
                        self.ended = true;
                        self.lease.abort();
                        return Ok(None);
                    }
                    _ => self.scan_offset = pos + 1,
                }
            } else {
                let keep = delimiter.len() - 1;
                if self.buffer.len() > keep {
                    self.buffer.drain(..self.buffer.len() - keep);
                }
                self.scan_offset = 0;
                if !self.fill(MAX_HEADER, false).await? {
                    return Err(Error::InvalidResponse("truncated multipart stream".into()).into());
                }
            }
        }
    }

    async fn read_part(&mut self) -> Read<Option<Frame>> {
        if let Some(validation) = self.validation.as_mut() {
            let result = validation.await;
            self.validation = None;
            let frame =
                result.map_err(|_| Error::Transport("JPEG validation task failed".into()))??;
            self.proven.store(true, Ordering::Release);
            return Ok(Some(frame));
        }
        if self.ended {
            return Ok(None);
        }
        if !*self.lease_state.borrow() {
            return Err(Error::Transport("stream lease ended".into()).into());
        }
        if self.resync && self.resynchronise().await?.is_none() {
            return Ok(None);
        }
        if self.first {
            let marker = [self.boundary.as_slice(), b"\r\n"].concat();
            let pos = self.until(&marker, MAX_HEADER).await?;
            if pos != 0 {
                return Err(Error::InvalidResponse("invalid multipart preamble".into()).into());
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
                    return Err(Error::InvalidResponse("duplicate frame length".into()).into());
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
            return Err(Error::InvalidResponse("frame is not JPEG".into()).into());
        }
        let body_start = end + 4;
        let delimiter = [b"\r\n".as_slice(), self.boundary.as_slice()].concat();
        let frame_limit = body_start + MAX_FRAME + delimiter.len() + 2;
        let size = if let Some(length) = length {
            if length > MAX_FRAME {
                return Err(Error::InvalidResponse("frame exceeds limit".into()).into());
            }
            let part_end = body_start + length + delimiter.len() + 2;
            while self.buffer.len() < part_end {
                if !self.fill(part_end, true).await? {
                    return Err(Error::InvalidResponse("truncated frame".into()).into());
                }
            }
            if !self.buffer[body_start + length..].starts_with(&delimiter) {
                return Err(Error::InvalidResponse("invalid frame boundary".into()).into());
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
                        if !self.fill(frame_limit, false).await? {
                            return Err(Error::InvalidResponse("truncated boundary".into()).into());
                        }
                    }
                    let suffix = &self.buffer[pos + delimiter.len()..pos + delimiter.len() + 2];
                    if suffix == b"\r\n" || suffix == b"--" {
                        break pos - body_start;
                    }
                    self.scan_offset = pos + 1;
                } else {
                    if self.buffer.len() >= frame_limit {
                        return Err(Error::InvalidResponse("frame exceeds limit".into()).into());
                    }
                    self.scan_offset =
                        next_scan_offset(self.buffer.len(), delimiter.len(), body_start);
                    #[cfg(test)]
                    if let Some(progress) = &self.scan_progress {
                        let _ = progress.send(self.buffer.len());
                    }
                    if !self.fill(frame_limit, false).await? {
                        return Err(Error::InvalidResponse("truncated frame".into()).into());
                    }
                }
            }
        };
        if size > MAX_FRAME {
            return Err(Error::InvalidResponse("frame exceeds limit".into()).into());
        }
        let suffix_start = body_start + size + delimiter.len();
        let rest_start = suffix_start + 2;
        let suffix = &self.buffer[suffix_start..rest_start];
        if suffix == b"--" {
            self.ended = true;
            self.lease.abort();
            self.buffer.truncate(rest_start);
        } else if suffix != b"\r\n" {
            return Err(Error::InvalidResponse("invalid boundary suffix".into()).into());
        } else if self.buffer.len() > rest_start {
            // The bytes after this part move to a buffer of their own, so the frame keeps the old allocation without a copy; both are charged while the move is made.
            let total = self.footprint() + self.buffer.len() - rest_start;
            self.reserve(total, total).await?;
        }
        let rest = self.buffer.split_off(rest_start);
        let mut frame = std::mem::replace(&mut self.buffer, rest);
        frame.truncate(body_start + size);
        frame.drain(..body_start);
        let charge = self
            .ledger
            .held
            .split(frame.capacity())
            .expect("the buffer's allocation is charged");
        self.scan_offset = 0;
        self.settle();
        self.validation = Some(tokio::spawn(async move {
            let permit = crate::jpeg_permit().await?;
            let jpeg = crate::validate_jpeg_async(frame, permit).await?;
            Ok(Frame {
                jpeg,
                _charge: charge,
            })
        }));
        let result = self.validation.as_mut().expect("validation exists").await;
        self.validation = None;
        let frame =
            result.map_err(|_| Error::Transport("JPEG validation task failed".into()))??;
        self.proven.store(true, Ordering::Release);
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video_tests::{jpeg_colour, padded_jpeg, part};
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    /// The bound on every wait in these tests, and the parsers' read and storage timeout: each awaited event follows from data the test has already sent or released, so exceeding it means the event did not happen.
    const BOUND: Duration = Duration::from_secs(5);
    /// The most one read from hyper's HTTP/1 transport hands over: `DEFAULT_MAX_BUFFER_SIZE` in hyper's `proto/h1/io.rs`, the cap on its read buffer.
    const TRANSPORT_CHUNK: usize = 8192 + 4096 * 100;
    /// The first boundary line, which the parser drains before it charges a part's exact length.
    const MARKER: &[u8] = b"--frame\r\n";

    fn jpeg() -> Vec<u8> {
        jpeg_colour([64, 128, 192])
    }

    /// A parser for `response` charging `storage`, and the sender keeping its lease open.
    fn parser(
        response: reqwest::Response,
        storage: Arc<StreamStorage>,
    ) -> (Parser, watch::Sender<bool>) {
        let (lease_tx, lease_state) = watch::channel(true);
        let parser = Parser::new(
            response,
            b"--frame".to_vec(),
            tokio::spawn(std::future::pending()),
            lease_state,
            BOUND,
            Arc::new(AtomicBool::new(false)),
            storage,
        );
        (parser, lease_tx)
    }

    /// Answers one HTTP request with the head of a multipart stream and returns the response and the server's socket, through which the test writes the body.
    async fn multipart() -> (reqwest::Response, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n").await.unwrap();
            socket
        });
        let response = reqwest::get(format!("http://{address}/")).await.unwrap();
        (response, server.await.unwrap())
    }

    async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(BOUND, async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what}"));
    }

    fn charged(storage: &Arc<StreamStorage>, jpeg: Vec<u8>) -> Frame {
        Frame {
            jpeg,
            _charge: storage.bytes.clone().try_acquire_many_owned(1).unwrap(),
        }
    }

    #[tokio::test]
    async fn stream_storage_counts_compressed_bytes() {
        let frame = padded_jpeg([64, 128, 192], 64 * 1024);
        let body = part(&frame, true, true);
        // Room for the part and a transport chunk.
        let storage = StreamStorage::new(body.len() + TRANSPORT_CHUNK);
        let (response, mut socket) = multipart().await;
        socket.write_all(&body).await.unwrap();
        let (mut video, _lease) = parser(response, storage.clone());
        let parsed = video.next_charged().await.unwrap().unwrap();
        assert_eq!(parsed.jpeg, frame);
        let used = storage.capacity() - storage.available();
        assert!(
            (frame.len()..body.len()).contains(&used),
            "a held frame charges its compressed bytes, not a slot: {used}"
        );
        assert!(video.next_charged().await.unwrap().is_none());
        drop(parsed);
        assert_eq!(storage.available(), storage.capacity());

        // A budget smaller than one part refuses it, however few streams there are.
        let storage = StreamStorage::new(frame.len());
        let (response, mut socket) = multipart().await;
        socket.write_all(&body).await.unwrap();
        let (mut video, _lease) = parser(response, storage.clone());
        assert!(matches!(
            video.next_charged().await,
            Err(Error::InvalidResponse(message)) if message == "video part exceeds stream storage"
        ));
        assert_eq!(storage.available(), storage.capacity());
    }

    #[tokio::test]
    async fn repeated_cancellation_keeps_fragment_scan_linear() {
        use tokio::sync::mpsc;
        let (send, mut receive) = mpsc::channel::<bool>(1);
        let jpeg = jpeg();
        let (response, mut socket) = multipart().await;
        let server = tokio::spawn(async move {
            socket
                .write_all(b"--frame\r\nContent-Type: image/jpeg\r\n\r\n\xff\xd8")
                .await
                .unwrap();
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
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let (mut video, _lease) = parser(response, storage);
        video.scan_progress = Some(progress_tx);
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
    async fn actor_idle_reports_timeout_and_releases_storage() {
        let (response, _socket) = multipart().await;
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let (parser, _lease) = parser(response, storage.clone());
        let mut stream = Mjpeg::start(parser, Duration::from_millis(20));
        assert!(stream.next_frame().await.is_err());
        eventually("actor did not finish", || stream.actor.is_finished()).await;
        assert_eq!(storage.available(), storage.capacity());
    }

    #[tokio::test]
    async fn actor_times_out_during_trickled_frame() {
        let (response, mut socket) = multipart().await;
        let server = tokio::spawn(async move {
            socket.write_all(b"--frame\r\n").await.unwrap();
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
                if socket.write_all(b"a").await.is_err() {
                    break;
                }
            }
        });
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let (parser, _lease) = parser(response, storage.clone());
        let mut stream = Mjpeg::start(parser, Duration::from_millis(40));
        assert!(
            tokio::time::timeout(BOUND, stream.next_frame())
                .await
                .unwrap()
                .is_err()
        );
        eventually("actor did not finish", || stream.actor.is_finished()).await;
        assert_eq!(storage.available(), storage.capacity());
        server.abort();
    }

    /// An actor that delivers one final frame of three bytes, charged to `storage`, waits for its acknowledgement, reports it on `ack`, then waits for `release` before it finishes.
    fn final_frame_actor(
        storage: &Arc<StreamStorage>,
        idle: Duration,
    ) -> (
        Mjpeg,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (requests, mut incoming) =
            tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<Delivery>>(1);
        let (_terminal_tx, terminal) = watch::channel(false);
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let frame = charged(storage, vec![1, 2, 3]);
        let actor = tokio::spawn(async move {
            let request = incoming.recv().await.unwrap();
            let (acknowledged, ack) = tokio::sync::oneshot::channel();
            request
                .send(Delivery {
                    result: Arc::new(std::sync::Mutex::new(Some(Ok(Some(frame))))),
                    acknowledged,
                    finished: true,
                    claimed: Arc::new(AtomicBool::new(false)),
                })
                .unwrap_or_else(|_| panic!("request closed"));
            assert!(ack.await.unwrap());
            ack_tx.send(()).unwrap();
            release_rx.await.unwrap();
        });
        let stream = Mjpeg {
            requests,
            terminal,
            actor,
            final_result: None,
            storage: storage.clone(),
            expiry: None,
            idle,
        };
        (stream, ack_rx, release_tx)
    }

    /// Cancels the stream's first read once the actor has had the final frame acknowledged, then lets the actor finish.
    async fn cancel_after_acknowledgement(
        stream: &mut Mjpeg,
        ack: tokio::sync::oneshot::Receiver<()>,
        release: tokio::sync::oneshot::Sender<()>,
    ) {
        let mut first = Box::pin(stream.next_frame());
        tokio::select! {
            result = &mut first => panic!("completed before cleanup: {result:?}"),
            result = ack => result.unwrap(),
        }
        drop(first);
        release.send(()).unwrap();
        eventually("actor did not finish", || stream.actor.is_finished()).await;
    }

    #[tokio::test]
    async fn cancelled_terminal_join_preserves_frame_until_cleanup() {
        let storage = StreamStorage::new(1);
        let (mut stream, ack, release) = final_frame_actor(&storage, Duration::from_secs(3));
        cancel_after_acknowledgement(&mut stream, ack, release).await;
        assert_eq!(storage.available(), 0);
        let expiry = stream.expiry.as_ref().unwrap().abort_handle();
        assert_eq!(
            stream.next_frame().await.unwrap().unwrap().jpeg,
            vec![1, 2, 3]
        );
        assert!(stream.expiry.is_none());
        tokio::task::yield_now().await;
        assert!(expiry.is_finished());
        assert_eq!(storage.available(), 1);
    }

    #[tokio::test]
    async fn cancelled_terminal_result_drop_releases_capacity() {
        let storage = StreamStorage::new(1);
        let (mut stream, ack, release) = final_frame_actor(&storage, Duration::from_secs(60));
        cancel_after_acknowledgement(&mut stream, ack, release).await;
        assert_eq!(storage.available(), 0);
        let expiry = stream.expiry.as_ref().unwrap().abort_handle();
        drop(stream);
        assert_eq!(storage.available(), 1);
        tokio::task::yield_now().await;
        assert!(expiry.is_finished());
    }

    #[tokio::test]
    async fn cancelled_terminal_result_expires_without_repoll() {
        let storage = StreamStorage::new(1);
        let (mut stream, ack, release) = final_frame_actor(&storage, Duration::from_millis(30));
        cancel_after_acknowledgement(&mut stream, ack, release).await;
        eventually("retained frame did not expire", || storage.available() == 1).await;
        assert!(stream.next_frame().await.is_err());
    }

    #[tokio::test]
    async fn retained_final_frame_is_reclaimed_for_a_waiting_stream() {
        let storage = StreamStorage::new(1);
        let (mut stream, ack, release) = final_frame_actor(&storage, Duration::from_secs(60));
        cancel_after_acknowledgement(&mut stream, ack, release).await;
        assert_eq!(storage.available(), 0);
        // Another stream waits for storage; the idle timeout is far beyond the bound, so only the waiter can release the frame in time.
        let waiter = Waiting::new(storage.clone(), u64::MAX);
        eventually("retained frame was not reclaimed", || {
            storage.available() == 1
        })
        .await;
        drop(waiter);
        assert!(matches!(
            stream.next_frame().await,
            Err(Error::Transport(message)) if message == "video frame storage reclaimed for a waiting stream"
        ));
    }

    #[tokio::test]
    async fn unpolled_delivery_expires_and_releases_capacity() {
        let jpeg = jpeg();
        let (response, mut socket) = multipart().await;
        socket.write_all(&part(&jpeg, false, true)).await.unwrap();
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let (parser, _lease) = parser(response, storage.clone());
        let stream = Mjpeg::start(parser, Duration::from_secs(2));
        let (send, mut receive) = tokio::sync::oneshot::channel();
        stream.requests.send(send).await.unwrap();
        let delivery = tokio::time::timeout(BOUND, async {
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
        assert!(storage.available() < storage.capacity());
        eventually("delivery did not expire", || {
            storage.available() == storage.capacity()
        })
        .await;
        assert!(delivery.result.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn actor_delivers_final_frame_after_cancelled_request() {
        let jpeg = jpeg();
        let (response, mut socket) = multipart().await;
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let expected = jpeg.clone();
        let server = tokio::spawn(async move {
            wait.await.unwrap();
            socket.write_all(&part(&jpeg, false, true)).await.unwrap();
        });
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let (parser, _lease) = parser(response, storage.clone());
        let mut stream = Mjpeg::start(parser, BOUND);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), stream.next_frame())
                .await
                .is_err()
        );
        release.send(()).unwrap();
        assert_eq!(stream.next_frame().await.unwrap().unwrap().jpeg, expected);
        assert!(stream.next_frame().await.unwrap().is_none());
        assert_eq!(storage.available(), storage.capacity());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn idle_stream_yields_retained_frame_to_active_stream() {
        const FRAME: usize = 1024 * 1024;
        let retained = padded_jpeg([64, 128, 192], FRAME);
        let later = jpeg_colour([200, 20, 50]);
        let active = padded_jpeg([20, 200, 50], FRAME);
        let mut idle_body = part(&retained, true, false);
        let first_end = idle_body.len() + MARKER.len();
        idle_body.extend_from_slice(&part(&later, true, true));
        let mut active_body = part(&active, true, false);
        active_body.extend_from_slice(MARKER);
        // The active part and a transport chunk fit, but not beside the idle stream's retained part: the active part is longer than the transport chunk left over.
        let storage = StreamStorage::new(active_body.len() + TRANSPORT_CHUNK);
        assert!(active_body.len() > TRANSPORT_CHUNK + MARKER.len());

        let (response, mut socket) = multipart().await;
        let (complete_tx, complete) = tokio::sync::oneshot::channel::<()>();
        let (resume_tx, resume) = tokio::sync::oneshot::channel::<()>();
        let idle_server = tokio::spawn(async move {
            // The last byte of the first part's closing delimiter is withheld until its read has been cancelled.
            socket.write_all(&idle_body[..first_end - 1]).await.unwrap();
            complete.await.unwrap();
            socket
                .write_all(&idle_body[first_end - 1..first_end])
                .await
                .unwrap();
            resume.await.unwrap();
            socket.write_all(&idle_body[first_end..]).await.unwrap();
        });
        let (idle_parser, _idle_lease) = parser(response, storage.clone());
        let mut idle = Mjpeg::start(idle_parser, BOUND);
        {
            let mut read = Box::pin(idle.next_frame());
            let charged_part = async {
                while storage.capacity() - storage.available() < FRAME {
                    tokio::task::yield_now().await;
                }
            };
            tokio::select! {
                result = &mut read => panic!("first frame completed before its delimiter: {result:?}"),
                result = tokio::time::timeout(BOUND, charged_part) => result.expect("first part was not charged"),
            }
        }
        // The cancelled read leaves its frame retained by the idle stream once the delimiter completes.
        complete_tx.send(()).unwrap();

        let (response, mut socket) = multipart().await;
        let active_server = tokio::spawn(async move {
            socket.write_all(&active_body).await.unwrap();
            socket
        });
        let (active_parser, _active_lease) = parser(response, storage.clone());
        let mut stream = Mjpeg::start(active_parser, BOUND);
        assert_eq!(
            tokio::time::timeout(BOUND, stream.next_frame())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .jpeg,
            active
        );
        let _active_socket = active_server.await.unwrap();

        // The idle stream dropped its superseded frame and continues with the next one.
        resume_tx.send(()).unwrap();
        assert_eq!(idle.next_frame().await.unwrap().unwrap().jpeg, later);
        assert!(idle.next_frame().await.unwrap().is_none());
        idle_server.await.unwrap();
    }

    #[tokio::test]
    async fn younger_waiting_stream_yields_partial_part_to_older() {
        use tokio::sync::mpsc;
        // Each stream first receives this much of a part with no length, then the rest.
        const PHASE: usize = 1024 * 1024;
        // A stream's buffer is at most twice what it holds, plus the transport chunk being consumed, and while storage is short it grows only to what it holds. After the first phase one stream therefore holds at most 2 * (PHASE + MAX_HEADER) + TRANSPORT_CHUNK, leaving the other its exact need, so both reach the end of the phase without waiting.
        let capacity = 3 * (PHASE + MAX_HEADER) + 2 * TRANSPORT_CHUNK;
        // Each part is longer than the storage the other stream leaves once both hold a phase, so neither can finish while the other keeps what it holds; one part alone still fits beside a transport chunk.
        let length = capacity - PHASE + MAX_HEADER;
        let storage = StreamStorage::new(capacity);
        let older_frame = padded_jpeg([64, 128, 192], length);
        let younger_frame = padded_jpeg([20, 200, 50], length);
        let next = jpeg_colour([200, 20, 50]);
        let head = b"--frame\r\nContent-Type: image/jpeg\r\n\r\n";

        let mut readers = Vec::new();
        let mut gates = Vec::new();
        let mut servers = Vec::new();
        let mut leases = Vec::new();
        for (frame, tail) in [
            (older_frame.clone(), b"\r\n--frame--\r\n".to_vec()),
            (
                younger_frame,
                [b"\r\n".as_slice(), &part(&next, true, true)].concat(),
            ),
        ] {
            let (response, mut socket) = multipart().await;
            let (rest_tx, rest) = tokio::sync::oneshot::channel::<()>();
            let (tail_tx, tail_gate) = tokio::sync::oneshot::channel::<()>();
            servers.push(tokio::spawn(async move {
                socket.write_all(head).await.unwrap();
                socket.write_all(&frame[..PHASE]).await.unwrap();
                rest.await.unwrap();
                socket.write_all(&frame[PHASE..]).await.unwrap();
                tail_gate.await.unwrap();
                socket.write_all(&tail).await.unwrap();
            }));
            let (progress_tx, progress) = mpsc::unbounded_channel();
            let (mut stream_parser, lease) = parser(response, storage.clone());
            stream_parser.scan_progress = Some(progress_tx);
            let mut stream = Mjpeg::start(stream_parser, BOUND);
            readers.push((
                tokio::spawn(async move {
                    let frame = stream.next_frame().await;
                    (stream, frame)
                }),
                progress,
            ));
            gates.push((rest_tx, tail_tx));
            leases.push(lease);
        }
        for (_, progress) in &mut readers {
            tokio::time::timeout(BOUND, async {
                while progress.recv().await.unwrap() < PHASE {}
            })
            .await
            .expect("stream did not hold its first phase");
        }
        let mut tails = Vec::new();
        for (rest, tail) in gates {
            rest.send(()).unwrap();
            tails.push(tail);
        }
        let mut readers = readers.into_iter();
        let (older, _) = readers.next().unwrap();
        let (younger, _) = readers.next().unwrap();
        let mut tails = tails.into_iter();
        tails.next().unwrap().send(()).unwrap();
        let (mut older, frame) = tokio::time::timeout(BOUND, older).await.unwrap().unwrap();
        assert_eq!(frame.unwrap().unwrap().jpeg, older_frame);
        assert!(older.next_frame().await.unwrap().is_none());
        // The younger stream released its partial part to the older one and resynchronised on the following delimiter.
        tails.next().unwrap().send(()).unwrap();
        let (mut younger, frame) = tokio::time::timeout(BOUND, younger).await.unwrap().unwrap();
        assert_eq!(frame.unwrap().unwrap().jpeg, next);
        assert!(younger.next_frame().await.unwrap().is_none());
        for server in servers {
            server.await.unwrap();
        }
        drop((older, younger, leases));
        assert_eq!(storage.available(), storage.capacity());
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
