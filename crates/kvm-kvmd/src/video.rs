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
/// The largest buffer one multipart part occupies: its headers and blank line (`until` bounds both by `MAX_HEADER`), the largest frame, the delimiter (`\r\n--` and the longest boundary) and the two-byte suffix after it. A stream copies received bytes into its buffer only up to the limit of what it is reading, so neither a part's buffer nor the frame cut from it exceeds this.
const MAX_PART: usize = MAX_HEADER + MAX_FRAME + 4 + MAX_BOUNDARY + 2;
/// The compressed bytes that every MJPEG stream in the process may hold from devices at once.
///
/// A frame keeps its part's buffer, at most `MAX_PART` bytes, from parsing until a consumer takes it, through its wait for a decoder slot and its validation. A stream assembling a part needs at most twice that at once: a buffer grows by copying into a new allocation, so the old and the new allocation are both charged during the copy, and a buffer never exceeds `MAX_PART`. Room for one more part than there are decoder slots (`MAX_JPEG_VALIDATIONS`) therefore lets a stream assemble the largest part while the other slots hold the largest frames, so every slot can be validating a maximum frame at once. Streams with smaller parts share the same bytes, so the number of open streams is not limited by the budget. Decoder working memory is bounded separately, by `MAX_JPEG_VALIDATIONS` validations of at most `MAX_JPEG_WORKING_BYTES` each.
pub(crate) const STREAM_STORAGE_BYTES: usize = (crate::MAX_JPEG_VALIDATIONS + 1) * MAX_PART;

/// A budget of compressed bytes shared by MJPEG streams, the streams waiting for it, and the decoder slots their frames are validated in.
///
/// A stream charges every byte it holds from the device in a buffer of its own: its parse buffer's allocation, and each frame it has parsed until a consumer takes it, the frame is dropped, or its validation ends (a validation that has started decoding cannot be interrupted and releases the frame when it finishes). A chunk the HTTP client has received and the stream has not yet copied is a slice of the connection's own read buffer, which every open connection has whether or not a stream holds a slice of it, so it is not charged. A stream keeps the rest of such a chunk beyond what it is reading: while it copies the chunk, while it waits for storage, and while it is idle after a read stopped part way through the chunk, most often after an exact-length part, when the rest of the chunk that completed the part holds the start of the next one, or after a cancelled read. It drops the slice only together with charged bytes it gives up, since dropping it alone would free nothing a waiting stream can use.
///
/// A stream that cannot charge what it needs registers as waiting, and other streams yield to waiters so that neither idle nor partly read streams can starve it:
///
/// - a stream with no consumer read outstanding that holds charged storage releases it while any stream waits: its buffered bytes with any received chunk, an unclaimed frame, and a frame waiting for or undergoing validation. It resynchronises on the next delimiter when it is read again, and a final frame given up this way, or a final part it was reading, is reported as an error;
/// - only the oldest waiting stream acquires storage; a younger waiting stream stays registered, releases any charged storage it holds (its partial part and any received chunk) and waits, reading nothing further from the device, until no older stream is waiting or its timeout passes. Waiting streams therefore never hold storage each other need.
///
/// The oldest waiter therefore waits only for streams that are reading or validating, which release their storage when their frame is delivered or dropped, the stream ends, or it times out.
pub(crate) struct StreamStorage {
    bytes: Arc<Semaphore>,
    capacity: usize,
    waiting: watch::Sender<BTreeSet<u64>>,
    next_stream: AtomicU64,
    decoders: Arc<Semaphore>,
    /// How many times a stream has given up its storage to an older waiting stream.
    #[cfg(test)]
    yielded: std::sync::atomic::AtomicUsize,
}

impl StreamStorage {
    /// Creates a budget of `capacity` bytes with no streams, validating frames in the process's shared decoder slots.
    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        Self::with_decoders(capacity, crate::jpeg_slots())
    }

    /// Creates a budget of `capacity` bytes with no streams, validating frames in `decoders`.
    pub(crate) fn with_decoders(capacity: usize, decoders: Arc<Semaphore>) -> Arc<Self> {
        Arc::new(Self {
            bytes: Arc::new(Semaphore::new(capacity)),
            capacity,
            waiting: watch::Sender::new(BTreeSet::new()),
            next_stream: AtomicU64::new(0),
            decoders,
            #[cfg(test)]
            yielded: std::sync::atomic::AtomicUsize::new(0),
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
    /// The part of the last received chunk not yet copied into `buffer`: a slice of the HTTP connection's read buffer, not charged (see `StreamStorage`).
    incoming: bytes::Bytes,
    ledger: Ledger,
    /// Set when discarded bytes left the parser inside a part, so it must skip to the next delimiter.
    resync: bool,
    /// Set when the final frame was given up to a waiting stream, which the next read reports.
    final_reclaimed: bool,
    validation: Option<JoinHandle<Result<Frame>>>,
    scan_offset: usize,
    #[cfg(test)]
    scanned: usize,
    #[cfg(test)]
    scan_progress: Option<tokio::sync::mpsc::UnboundedSender<usize>>,
    /// The capacity of the buffer after each growth.
    #[cfg(test)]
    growths: Vec<usize>,
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
        // A validation still waiting for a decoder slot drops its frame and charge now; one already decoding releases them when it finishes.
        if let Some(validation) = self.validation.take() {
            validation.abort();
        }
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
    /// The final result after the consumer claimed it, kept here only while no read is outstanding; a read takes it out, so neither expiry nor a waiting stream can reach it while the consumer is reading.
    final_result: Option<SharedResult>,
    storage: Arc<StreamStorage>,
    idle: Duration,
    /// Expires or reclaims the kept final result; it exists only while no read is outstanding.
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

/// Keeps a kept final result for its consumer no longer than `idle`, and gives a final frame's storage to the first stream that waits for it, reporting the loss in its place.
fn spawn_expiry(
    result: std::sync::Weak<std::sync::Mutex<Option<Result<Option<Frame>>>>>,
    storage: &StreamStorage,
    idle: Duration,
) -> JoinHandle<()> {
    let mut pressure = storage.waiting.subscribe();
    tokio::spawn(async move {
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
    })
}

/// A final result taken out for a read in progress. If the read is cancelled, it puts the result back and starts its expiry.
struct Lent<'a> {
    /// Cleared once the read completes, so the result is neither put back nor expired.
    armed: bool,
    result: Option<Result<Option<Frame>>>,
    slot: SharedResult,
    kept: &'a mut Option<SharedResult>,
    expiry: &'a mut Option<JoinHandle<()>>,
    storage: &'a StreamStorage,
    idle: Duration,
}

impl Drop for Lent<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(result) = self.result.take() {
            *self.slot.lock().expect("delivery lock poisoned") = Some(result);
        }
        // A read dropped outside a runtime leaves the result to be taken by the next read or dropped with the stream.
        if tokio::runtime::Handle::try_current().is_ok() {
            *self.expiry = Some(spawn_expiry(
                Arc::downgrade(&self.slot),
                self.storage,
                self.idle,
            ));
        }
        *self.kept = Some(self.slot.clone());
    }
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
                let holds_storage = parser.holds_storage() || matches!(pending, Some(Ok(Some(_))));
                let mut request = tokio::select! {
                    _ = &mut deadline => { let _ = terminal_tx.send(true); break; }
                    request = incoming.recv() => match request { Some(request) => request, None => break },
                    // No consumer read is outstanding, so this stream is idle and yields what it holds to the waiting stream.
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
                        // The consumer cancelled its read, so the stream stops reading and is idle until the next one; what it holds stays for that read unless a stream waits.
                        () = request.closed() => continue,
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

    /// Joins the finished actor and returns the kept final result. The result is taken out for the read and its expiry stopped, so it cannot be expired or reclaimed while the consumer reads; a cancelled read puts it back.
    async fn take_final(&mut self) -> Result<Option<Snapshot>> {
        let Self {
            actor,
            final_result,
            storage,
            idle,
            expiry,
            ..
        } = self;
        if let Some(expiry) = expiry.take() {
            expiry.abort();
        }
        let slot = final_result.take().expect("final result exists");
        let result = slot.lock().expect("delivery lock poisoned").take();
        let mut lent = Lent {
            armed: true,
            result,
            slot,
            kept: final_result,
            expiry,
            storage: storage.as_ref(),
            idle: *idle,
        };
        let _ = actor.await;
        lent.armed = false;
        let result = lent.result.take();
        drop(lent);
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
                    let result = {
                        let mut result = delivery.result.lock().expect("delivery lock poisoned");
                        let Some(result) = result.take() else {
                            return Err(idle_timeout());
                        };
                        delivery.claimed.store(true, Ordering::Release);
                        result
                    };
                    self.final_result = Some(Arc::new(std::sync::Mutex::new(Some(result))));
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
            incoming: bytes::Bytes::new(),
            ledger: Ledger::new(storage),
            resync: false,
            final_reclaimed: false,
            validation: None,
            scan_offset: 0,
            #[cfg(test)]
            scanned: 0,
            #[cfg(test)]
            scan_progress: None,
            #[cfg(test)]
            growths: Vec::new(),
            lease,
            lease_state,
            timeout,
            first: true,
            ended: false,
            proven,
        }
    }

    /// Whether the stream holds storage a waiting stream could reclaim: charged bytes, or a frame waiting for or undergoing validation. An uncharged received chunk does not count, since giving it up frees nothing a waiting stream can use.
    fn holds_storage(&self) -> bool {
        self.ledger.held.num_permits() > 0 || self.validation.is_some()
    }

    /// Releases the charge beyond the buffer's allocation, which is everything the ledger covers.
    fn release_excess(&mut self) {
        let excess = self
            .ledger
            .held
            .num_permits()
            .saturating_sub(self.buffer.capacity());
        drop(self.ledger.held.split(excess));
    }

    /// Charges the buffer's whole allocation, including any the allocator granted beyond the request, then releases the rest of the charge.
    async fn settle(&mut self) -> Read<()> {
        let footprint = self.buffer.capacity();
        self.reserve(footprint).await?;
        self.release_excess();
        Ok(())
    }

    /// Drops every device byte this stream holds and releases their charge: its buffer, any received chunk, and a frame waiting for or undergoing validation (which releases its own charge when it ends). Dropping bytes leaves the parser inside a part, so it then skips to the next delimiter.
    fn discard(&mut self) {
        if let Some(validation) = self.validation.take() {
            validation.abort();
            if self.ended {
                self.final_reclaimed = true;
            }
        }
        if !self.buffer.is_empty() || !self.incoming.is_empty() {
            self.resync = true;
            self.first = false;
        }
        self.buffer = Vec::new();
        self.incoming = bytes::Bytes::new();
        self.scan_offset = 0;
        self.release_excess();
    }

    /// Charges the stream for holding `need` bytes in all, waiting at most the read timeout. Only the oldest waiting stream acquires: while an older stream waits, this one stays registered as waiting, releases any storage it holds (returning `Reclaimed` once no older stream waits, so the read restarts after the bytes it dropped) and waits until no older stream is waiting, without reading further from the device. A stream still waiting when the timeout passes fails with "video stream storage exhausted".
    async fn reserve(&mut self, need: usize) -> Read<()> {
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
            if let Ok(permit) = storage
                .bytes
                .clone()
                .try_acquire_many_owned(permits(need - held)?)
            {
                self.ledger.held.merge(permit);
                return Ok(());
            }
            if waiting.is_none() {
                waiting = Some(Waiting::new(storage.clone(), id));
            }
            if older_waiting(&pressure.borrow()) {
                // Charged storage goes to the older stream; an uncharged received chunk frees nothing it can use, so it is kept unless it is part of what is dropped.
                let reclaimed = held > 0;
                if reclaimed {
                    self.discard();
                    #[cfg(test)]
                    storage.yielded.fetch_add(1, Ordering::Release);
                }
                tokio::select! {
                    _ = pressure.wait_for(|waiting| !older_waiting(waiting)) => {}
                    () = tokio::time::sleep_until(deadline) => return Err(exhausted()),
                }
                if reclaimed {
                    return Err(ReadError::Reclaimed);
                }
                continue;
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

    /// Copies received bytes into the buffer, which never holds more than `limit` bytes; bytes beyond it stay in `incoming` for the next read. With `exact`, `limit` is the known end of the part and the buffer grows straight to it. Otherwise each growth at least doubles the buffer's capacity, up to `limit`, waiting for that storage when it is not free, so a part is copied a number of times logarithmic in its length and the bytes copied stay linear in it. A buffer grows by copying into a new allocation, and both allocations are charged during the copy, so a buffer at most `limit` long peaks below twice `limit`.
    async fn fill(&mut self, limit: usize, exact: bool) -> Read<bool> {
        if self.incoming.is_empty() {
            if !*self.lease_state.borrow() {
                return Err(Error::Transport("stream lease ended".into()).into());
            }
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
                Some(chunk) => self.incoming = chunk,
                None => return Ok(false),
            }
        }
        let count = self
            .incoming
            .len()
            .min(limit.saturating_sub(self.buffer.len()));
        if count == 0 {
            return Err(Error::InvalidResponse("multipart data exceeds limit".into()).into());
        }
        let len = self.buffer.len() + count;
        let capacity = self.buffer.capacity();
        if len > capacity {
            let target = if exact {
                limit
            } else {
                len.max(capacity.saturating_mul(2).min(limit))
            };
            self.reserve(capacity + target).await?;
            let mut grown = Vec::with_capacity(target);
            grown.extend_from_slice(&self.buffer);
            self.buffer = grown;
            #[cfg(test)]
            self.growths.push(self.buffer.capacity());
        }
        let chunk = std::mem::take(&mut self.incoming);
        absorb_chunk(&mut self.buffer, &mut self.incoming, chunk, limit)?;
        self.settle().await?;
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
            self.incoming = bytes::Bytes::new();
            self.release_excess();
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
    /// Skips to the start of the next part's headers after discarded bytes, keeping only enough of the buffer to find a delimiter split across reads. Bytes are discarded only from inside a part, so a final delimiter found here closes a part that was given up, and the stream ends reporting that its final frame was reclaimed.
    async fn resynchronise(&mut self) -> Read<()> {
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
                        return Ok(());
                    }
                    b"--" => {
                        self.ended = true;
                        self.lease.abort();
                        return Err(storage_reclaimed().into());
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
        if self.final_reclaimed {
            return Err(storage_reclaimed().into());
        }
        if self.ended {
            return Ok(None);
        }
        if !*self.lease_state.borrow() {
            return Err(Error::Transport("stream lease ended".into()).into());
        }
        if self.resync {
            self.resynchronise().await?;
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
            // Growing the buffer to the part charges its headers' allocation as well, so a part that cannot fit is refused before any of its body is read.
            let capacity = self.buffer.capacity();
            if part_end > capacity && capacity + part_end > self.ledger.storage.capacity {
                return Err(storage_exceeded().into());
            }
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
            let total = self.buffer.capacity() + self.buffer.len() - rest_start;
            self.reserve(total).await?;
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
        let decoders = self.ledger.storage.decoders.clone();
        // The frame's charge travels with the frame into its validation, so aborting the validation before decoding starts releases both, and a decode that has started releases both when it finishes.
        self.validation = Some(tokio::spawn(async move {
            let permit = crate::jpeg_permit_from(decoders).await?;
            let (jpeg, charge) = crate::validate_jpeg_async(frame, permit, charge).await?;
            Ok(Frame {
                jpeg,
                _charge: charge,
            })
        }));
        self.settle().await?;
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

    /// The buffer an exact-length part of `frame` occupies: its headers, the frame, its closing delimiter and the two bytes after it, without the opening boundary line, which the parser has consumed before the part.
    fn part_end(frame: &[u8]) -> usize {
        part(frame, true, true).len() - b"--frame\r\n".len() - b"\r\n".len()
    }

    #[tokio::test]
    async fn stream_storage_counts_compressed_bytes() {
        let frame = padded_jpeg([64, 128, 192], 64 * 1024);
        let body = part(&frame, true, true);
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let (response, mut socket) = multipart().await;
        socket.write_all(&body).await.unwrap();
        let (mut video, _lease) = parser(response, storage.clone());
        let parsed = video.next_charged().await.unwrap().unwrap();
        assert_eq!(parsed.jpeg, frame);
        // A held frame charges its part's buffer: its compressed bytes with their headers and delimiter, not a stream slot.
        assert_eq!(storage.capacity() - storage.available(), part_end(&frame));
        assert!(video.next_charged().await.unwrap().is_none());
        drop(parsed);
        assert_eq!(storage.available(), storage.capacity());
    }

    #[tokio::test]
    async fn part_larger_than_storage_is_refused_from_its_length() {
        let frame = padded_jpeg([64, 128, 192], 64 * 1024);
        let head = format!(
            "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            frame.len()
        );
        // The headers fit, but the part does not, beside the headers' own buffer.
        let storage = StreamStorage::new(part_end(&frame));
        let (response, mut socket) = multipart().await;
        // Only the headers are sent, so the refusal comes from the declared length, not from received data.
        socket.write_all(head.as_bytes()).await.unwrap();
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

    // A current-thread runtime, with the steps run in a spawned task, so that `yield_now` deterministically runs every task already woken before this one resumes: tokio documents that a yielding task is re-added at the back of the pending queue and that other pending tasks are scheduled first (https://docs.rs/tokio/1.54.1/tokio/task/fn.yield_now.html). An expiry or reclaim task woken by the waiter's registration would therefore act before the read resumes.
    #[tokio::test(flavor = "current_thread")]
    async fn final_frame_being_read_is_not_reclaimed() {
        tokio::spawn(async {
            let storage = StreamStorage::new(1);
            let (mut stream, ack, release) =
                final_frame_actor(&storage, Duration::from_secs(60));
            let mut read = Box::pin(stream.next_frame());
            tokio::select! {
                result = &mut read => panic!("read completed before the actor finished: {result:?}"),
                result = ack => result.unwrap(),
            }
            // The read has claimed the final frame and is joining the actor when another stream starts waiting.
            let waiter = Waiting::new(storage.clone(), u64::MAX);
            tokio::task::yield_now().await;
            release.send(()).unwrap();
            assert_eq!(
                tokio::time::timeout(BOUND, read)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .jpeg,
                vec![1, 2, 3]
            );
            drop(waiter);
            assert!(stream.expiry.is_none());
            assert_eq!(storage.available(), 1);
        })
        .await
        .unwrap();
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
    async fn unclaimed_frame_is_reclaimed_for_a_waiting_stream() {
        let mut body = part(&jpeg(), true, false);
        body.extend_from_slice(b"--frame\r\n");
        let (response, mut socket) = multipart().await;
        socket.write_all(&body).await.unwrap();
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let (parser, _lease) = parser(response, storage.clone());
        // The idle timeout is far beyond the bound, so only a waiting stream can release the frame in time.
        let stream = Mjpeg::start(parser, Duration::from_secs(60));
        let (send, receive) = tokio::sync::oneshot::channel();
        stream.requests.send(send).await.unwrap();
        // The delivery is received and dropped unclaimed, so the actor keeps the frame for the next read.
        drop(tokio::time::timeout(BOUND, receive).await.unwrap().unwrap());
        eventually("frame was not retained", || {
            storage.available() < storage.capacity()
        })
        .await;
        let waiter = Waiting::new(storage.clone(), u64::MAX);
        eventually("unclaimed frame was not reclaimed", || {
            storage.available() == storage.capacity()
        })
        .await;
        drop(waiter);
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
    async fn cancelled_reader_yields_partial_part_to_active_stream() {
        const FRAME: usize = 1024 * 1024;
        let first = padded_jpeg([64, 128, 192], FRAME);
        let later = jpeg_colour([200, 20, 50]);
        let active = padded_jpeg([20, 200, 50], FRAME);
        assert_eq!(part_end(&first), part_end(&active));
        // One stream can grow its header buffer into a part, but two parts do not fit.
        let storage = StreamStorage::new(MAX_HEADER + part_end(&active));

        let mut idle_body = part(&first, true, false);
        idle_body.extend_from_slice(&part(&later, true, true));
        // The first part's headers and half its body. That is more than a header buffer holds, so however it is split into reads, the idle stream has grown its buffer to the whole part before it waits for the rest.
        let sent = part(&first, true, false).len() - first.len() + FRAME / 2;
        const { assert!(FRAME / 2 > MAX_HEADER) };
        let (response, mut socket) = multipart().await;
        let (resume_tx, resume) = tokio::sync::oneshot::channel::<()>();
        let idle_server = tokio::spawn(async move {
            socket.write_all(&idle_body[..sent]).await.unwrap();
            resume.await.unwrap();
            socket.write_all(&idle_body[sent..]).await.unwrap();
        });
        let (idle_parser, _idle_lease) = parser(response, storage.clone());
        let mut idle = Mjpeg::start(idle_parser, BOUND);
        {
            let mut read = Box::pin(idle.next_frame());
            let charged_part = async {
                while storage.capacity() - storage.available() < part_end(&first) {
                    tokio::task::yield_now().await;
                }
            };
            tokio::select! {
                result = &mut read => panic!("first frame completed before its body: {result:?}"),
                result = tokio::time::timeout(BOUND, charged_part) => result.expect("first part was not charged"),
            }
        }
        // The read was cancelled with the part partly received; the idle stream stops reading and gives its part to the active stream.
        let mut active_body = part(&active, true, false);
        active_body.extend_from_slice(b"--frame\r\n");
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

        // The idle stream lost its partial frame and continues with the next one.
        resume_tx.send(()).unwrap();
        assert_eq!(idle.next_frame().await.unwrap().unwrap().jpeg, later);
        assert!(idle.next_frame().await.unwrap().is_none());
        idle_server.await.unwrap();
    }

    #[tokio::test]
    async fn younger_waiting_stream_yields_storage_to_older() {
        // Two streams each hold this much, and each then needs more than the other leaves.
        const HOLD: usize = 40;
        const NEED: usize = 70;
        let storage = StreamStorage::new(100);
        let (older_response, _older_socket) = multipart().await;
        let (younger_response, _younger_socket) = multipart().await;
        let (mut older, _older_lease) = parser(older_response, storage.clone());
        let (mut younger, _younger_lease) = parser(younger_response, storage.clone());
        assert!(older.ledger.id < younger.ledger.id);
        assert!(older.reserve(HOLD).await.is_ok());
        assert!(younger.reserve(HOLD).await.is_ok());
        assert!(2 * HOLD + (NEED - HOLD) > storage.capacity());
        let (older_result, younger_result) =
            tokio::join!(older.reserve(NEED), younger.reserve(NEED));
        assert!(older_result.is_ok());
        // The younger stream gave up its storage, and reports it only once the older one no longer waits.
        assert!(matches!(younger_result, Err(ReadError::Reclaimed)));
        assert_eq!(storage.yielded.load(Ordering::Acquire), 1);
        assert_eq!(older.ledger.held.num_permits(), NEED);
        assert_eq!(younger.ledger.held.num_permits(), 0);
        drop(older);
        assert!(younger.reserve(NEED).await.is_ok());
    }

    #[tokio::test]
    async fn dropped_stream_releases_storage_of_queued_validation() {
        let jpeg = jpeg();
        let mut body = part(&jpeg, true, false);
        body.extend_from_slice(b"--frame\r\n");
        let (response, mut socket) = multipart().await;
        socket.write_all(&body).await.unwrap();
        // No decoder slot is ever free, so the parsed frame waits for validation indefinitely.
        let storage =
            StreamStorage::with_decoders(STREAM_STORAGE_BYTES, Arc::new(Semaphore::new(0)));
        let (parser, _lease) = parser(response, storage.clone());
        let mut stream = Mjpeg::start(parser, BOUND);
        {
            let mut read = Box::pin(stream.next_frame());
            let queued = async {
                while storage.capacity() - storage.available() < part_end(&jpeg) {
                    tokio::task::yield_now().await;
                }
            };
            tokio::select! {
                result = &mut read => panic!("frame validated without a decoder slot: {result:?}"),
                result = tokio::time::timeout(BOUND, queued) => result.expect("frame was not parsed"),
            }
        }
        drop(stream);
        eventually("queued validation kept its storage", || {
            storage.available() == storage.capacity()
        })
        .await;
    }

    #[tokio::test]
    async fn idle_stream_releases_storage_of_queued_validation() {
        let jpeg = jpeg();
        let later = jpeg_colour([200, 20, 50]);
        let mut body = part(&jpeg, true, false);
        body.extend_from_slice(b"--frame\r\n");
        let (response, mut socket) = multipart().await;
        socket.write_all(&body).await.unwrap();
        let decoders = Arc::new(Semaphore::new(0));
        let storage = StreamStorage::with_decoders(STREAM_STORAGE_BYTES, decoders.clone());
        let (parser, _lease) = parser(response, storage.clone());
        let mut stream = Mjpeg::start(parser, BOUND);
        {
            let mut read = Box::pin(stream.next_frame());
            let queued = async {
                while storage.capacity() - storage.available() < part_end(&jpeg) {
                    tokio::task::yield_now().await;
                }
            };
            tokio::select! {
                result = &mut read => panic!("frame validated without a decoder slot: {result:?}"),
                result = tokio::time::timeout(BOUND, queued) => result.expect("frame was not parsed"),
            }
        }
        let waiter = Waiting::new(storage.clone(), u64::MAX);
        eventually("idle stream kept its queued frame", || {
            storage.available() == storage.capacity()
        })
        .await;
        drop(waiter);
        // The abandoned frame is superseded by the next one, sent only now, once a decoder is free.
        socket
            .write_all(&part(&later, true, true)[b"--frame\r\n".len()..])
            .await
            .unwrap();
        decoders.add_permits(1);
        assert_eq!(stream.next_frame().await.unwrap().unwrap().jpeg, later);
        assert!(stream.next_frame().await.unwrap().is_none());
    }

    /// A parser for `response` charging `storage`, with its own read and storage timeout.
    fn parser_with_timeout(
        response: reqwest::Response,
        storage: Arc<StreamStorage>,
        timeout: Duration,
    ) -> (Parser, watch::Sender<bool>) {
        let (mut parser, lease) = parser(response, storage);
        parser.timeout = timeout;
        (parser, lease)
    }

    /// Holds all but `free` bytes of `storage` until dropped.
    fn hold_all_but(storage: &Arc<StreamStorage>, free: usize) -> OwnedSemaphorePermit {
        storage
            .bytes
            .clone()
            .try_acquire_many_owned(u32::try_from(storage.capacity() - free).unwrap())
            .unwrap()
    }

    #[tokio::test]
    async fn younger_waiting_stream_keeps_uncharged_chunk() {
        let storage = StreamStorage::new(100);
        let older = Ledger::new(storage.clone());
        let (response, _socket) = multipart().await;
        let (mut younger, _lease) = parser(response, storage.clone());
        younger.incoming = bytes::Bytes::from_static(b"start of the next part");
        // An uncharged chunk alone is nothing an idle stream gives up.
        assert!(!younger.holds_storage());
        let hold = hold_all_but(&storage, 0);
        let waiter = Waiting::new(storage.clone(), older.id);
        let mut registered = storage.waiting.subscribe();
        {
            let reserve = younger.reserve(10);
            tokio::pin!(reserve);
            tokio::select! {
                result = &mut reserve => panic!("acquired while an older stream waited: {:?}", result.is_ok()),
                _ = registered.wait_for(|waiting| waiting.len() == 2) => {}
            }
            drop((waiter, hold));
            assert!(tokio::time::timeout(BOUND, reserve).await.unwrap().is_ok());
        }
        assert_eq!(&younger.incoming[..], b"start of the next part");
        assert_eq!(storage.yielded.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn unlengthed_part_grows_geometrically_under_contention() {
        const CHUNK: usize = 1000;
        const LIMIT: usize = 64 * CHUNK;
        // Free storage fits every doubling copy up to a buffer of `LIMIT / 2` (`3 * LIMIT / 4` with both allocations) and an exact-fit copy just past it, but not the doubling copy from `LIMIT / 2` to `LIMIT` (`3 * LIMIT / 2`). Growing to fit would succeed there, one chunk at a time.
        const FREE: usize = 5 * LIMIT / 4;
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let (response, _socket) = multipart().await;
        let (mut video, _lease) = parser(response, storage.clone());
        let mut hold = Some(hold_all_but(&storage, FREE));
        let mut waiting = storage.waiting.subscribe();
        for _ in 0..LIMIT / CHUNK {
            video.incoming = bytes::Bytes::from(vec![b'a'; CHUNK]);
            let fill = video.fill(LIMIT, false);
            tokio::pin!(fill);
            loop {
                tokio::select! {
                    result = &mut fill => {
                        assert!(matches!(result, Ok(true)));
                        break;
                    }
                    // The doubling copy waits for storage; the other holder then gives it up.
                    _ = waiting.wait_for(|waiting| !waiting.is_empty()), if hold.is_some() => hold = None,
                }
            }
        }
        assert!(hold.is_none(), "the doubling copy did not wait for storage");
        assert_eq!(video.buffer.len(), LIMIT);
        assert_eq!(video.growths.first(), Some(&CHUNK));
        for step in video.growths.windows(2) {
            assert!(
                step[1] >= (2 * step[0]).min(LIMIT),
                "growth from {} to {} copies the buffer again within a doubling",
                step[0],
                step[1]
            );
        }
    }

    #[tokio::test]
    async fn idle_stream_reports_its_reclaimed_final_part() {
        const FRAME: usize = 1024 * 1024;
        let last = padded_jpeg([64, 128, 192], FRAME);
        let body = part(&last, true, true);
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let sent = part(&last, true, false).len() - last.len() + FRAME / 2;
        const { assert!(FRAME / 2 > MAX_HEADER) };
        let (response, mut socket) = multipart().await;
        socket.write_all(&body[..sent]).await.unwrap();
        let (idle_parser, _lease) = parser(response, storage.clone());
        let mut idle = Mjpeg::start(idle_parser, BOUND);
        {
            let mut read = Box::pin(idle.next_frame());
            let charged_part = async {
                while storage.capacity() - storage.available() < part_end(&last) {
                    tokio::task::yield_now().await;
                }
            };
            tokio::select! {
                result = &mut read => panic!("final frame completed before its body: {result:?}"),
                result = tokio::time::timeout(BOUND, charged_part) => result.expect("final part was not charged"),
            }
        }
        let waiter = Waiting::new(storage.clone(), u64::MAX);
        eventually("idle stream kept its final part", || {
            storage.available() == storage.capacity()
        })
        .await;
        drop(waiter);
        socket.write_all(&body[sent..]).await.unwrap();
        assert!(matches!(
            idle.next_frame().await,
            Err(Error::Transport(message)) if message == "video frame storage reclaimed for a waiting stream"
        ));
    }

    #[tokio::test]
    async fn younger_waiting_stream_reports_its_reclaimed_final_part() {
        let last = padded_jpeg([64, 128, 192], 64 * 1024);
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let older = Ledger::new(storage.clone());
        let (response, mut socket) = multipart().await;
        socket.write_all(&part(&last, true, true)).await.unwrap();
        let (mut younger, _lease) = parser(response, storage.clone());
        // Room for the headers but not the part, so the younger stream waits behind the older one.
        let hold = hold_all_but(&storage, MAX_HEADER);
        let waiter = Waiting::new(storage.clone(), older.id);
        let read = younger.next_charged();
        tokio::pin!(read);
        let yielded = async {
            while storage.yielded.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        };
        tokio::select! {
            result = &mut read => panic!("read finished while an older stream waited: {:?}", result.is_ok()),
            result = tokio::time::timeout(BOUND, yielded) => result.expect("younger stream did not yield"),
        }
        drop((waiter, hold));
        assert!(matches!(
            tokio::time::timeout(BOUND, read).await.unwrap(),
            Err(Error::Transport(message)) if message == "video frame storage reclaimed for a waiting stream"
        ));
    }

    #[tokio::test]
    async fn younger_waiting_stream_reads_nothing_until_its_timeout() {
        let first = padded_jpeg([64, 128, 192], 64 * 1024);
        let next = jpeg_colour([200, 20, 50]);
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        // An older stream waits throughout.
        let older = Ledger::new(storage.clone());
        let _waiter = Waiting::new(storage.clone(), older.id);
        let mut body = part(&first, true, false);
        body.extend_from_slice(&part(&next, true, true));
        let (response, mut socket) = multipart().await;
        socket.write_all(&body).await.unwrap();
        // The headers and the small next part fit, the first part does not. A stream that kept reading after giving up the first part would deliver the next one.
        let _hold = hold_all_but(&storage, MAX_HEADER + part_end(&next));
        assert!(part_end(&first) > MAX_HEADER + part_end(&next));
        let timeout = Duration::from_millis(200);
        assert!(timeout < BOUND);
        let (mut younger, _lease) = parser_with_timeout(response, storage.clone(), timeout);
        assert!(matches!(
            tokio::time::timeout(BOUND, younger.next_charged()).await.unwrap(),
            Err(Error::Transport(message)) if message == "video stream storage exhausted"
        ));
        assert_eq!(storage.yielded.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn older_waiting_stream_wins_and_younger_resumes() {
        let older_frame = padded_jpeg([64, 128, 192], 64 * 1024);
        let younger_first = padded_jpeg([20, 200, 50], 64 * 1024);
        let younger_next = jpeg_colour([200, 20, 50]);
        // Room for both header buffers but for neither part, until the other holder gives way.
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let hold = hold_all_but(&storage, 2 * MAX_HEADER);
        assert!(part_end(&older_frame) > 2 * MAX_HEADER);

        let (older_response, mut older_socket) = multipart().await;
        let (younger_response, mut younger_socket) = multipart().await;
        let (older_parser, _older_lease) = parser(older_response, storage.clone());
        let (younger_parser, _younger_lease) = parser(younger_response, storage.clone());
        let (older_id, younger_id) = (older_parser.ledger.id, younger_parser.ledger.id);
        assert!(older_id < younger_id);
        let mut older = Mjpeg::start(older_parser, BOUND);
        let mut younger = Mjpeg::start(younger_parser, BOUND);
        let mut waiting = storage.waiting.subscribe();

        // The younger stream reads first and waits for its part.
        let mut younger_body = part(&younger_first, true, false);
        younger_body.extend_from_slice(&part(&younger_next, true, true));
        younger_socket.write_all(&younger_body).await.unwrap();
        let younger_read = tokio::spawn(async move {
            let first = younger.next_frame().await;
            let second = younger.next_frame().await;
            (first, second)
        });
        tokio::time::timeout(
            BOUND,
            waiting.wait_for(|waiting| waiting.contains(&younger_id)),
        )
        .await
        .unwrap()
        .unwrap();
        // Then the older one does, and the younger gives up its storage to it.
        older_socket
            .write_all(&part(&older_frame, true, true))
            .await
            .unwrap();
        let older_read = tokio::spawn(async move {
            let frame = older.next_frame().await;
            (older, frame)
        });
        eventually("younger stream did not yield", || {
            storage.yielded.load(Ordering::Acquire) == 1
        })
        .await;
        drop(hold);
        let (mut older, frame) = tokio::time::timeout(BOUND, older_read)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame.unwrap().unwrap().jpeg, older_frame);
        assert!(older.next_frame().await.unwrap().is_none());
        // The younger stream lost the part it gave up and continues with the next one.
        let (first, second) = tokio::time::timeout(BOUND, younger_read)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.unwrap().unwrap().jpeg, younger_next);
        assert!(second.unwrap().is_none());
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
