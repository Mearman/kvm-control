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
/// A stream charges every byte it holds from the device in a buffer of its own: its parse buffer's allocation, and each frame it has parsed until a consumer takes it, the frame is dropped, or its validation ends (a validation that has started decoding cannot be interrupted and releases the frame when it finishes). Received bytes the stream has not yet copied are not charged. Most are a slice of the HTTP connection's own read buffer, which every open connection has whether or not a stream holds a slice of it: the rest of a chunk beyond what the stream is reading, kept while it copies the chunk, waits for storage, or is idle after a read stopped part way through it, and bytes after a complete part's delimiter, which return to their source as a slice of it. The others are bytes a stream keeps when it gives up storage, at most `MAX_HEADER` per stream (see `Parser::discard`), a fixed bound per open stream like the connection's buffer.
///
/// A stream registers as waiting only while a reservation is blocked, and other streams yield to waiting streams so that neither idle nor partly read streams can starve them:
///
/// - a stream with no consumer read outstanding that holds charged storage releases it while any stream waits: its buffer, an unclaimed frame, and a frame waiting for or undergoing validation. Giving up headers loses nothing, since they are kept and read again; a frame or the body of a part is lost, and the stream continues with the part after it, finding that part's start by the given-up part's declared length or by scanning for its delimiter. A final frame given up this way, or a final part it was reading, is reported as an error;
/// - a blocked stream that holds charged storage gives it up to any older blocked stream, whether or not that one holds storage, and restarts its read; a blocked stream that holds none waits in the budget's queue for what it needs. Blocked streams therefore never hold storage an older one needs.
///
/// The oldest blocked stream therefore waits only for streams that are reading or validating, which release their storage when their frame is delivered or dropped, the stream ends, or it times out, or that queued for storage before it and will read once served.
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

/// What a stream is reading. Positions are indices into the buffer, which holds the current part from its headers.
#[derive(Clone, Copy)]
enum Part {
    /// The opening boundary (while `first`) or a part's headers.
    Head,
    /// The body of a part whose declared length puts its closing delimiter at `delimiter`.
    Length { delimiter: usize },
    /// The body of a part without a declared length, scanned for its delimiter up to `scan_offset`.
    Scan,
    /// Bytes after a given-up part without a declared length, scanned for its delimiter.
    Resync,
    /// The closing delimiter of a given-up part with a declared length, after `skip` more body bytes.
    Delimiter,
}

struct Parser {
    response: reqwest::Response,
    boundary: Vec<u8>,
    buffer: Vec<u8>,
    /// The part of the last received chunk not yet copied into `buffer`: a slice of the HTTP connection's read buffer, not charged (see `StreamStorage`).
    incoming: bytes::Bytes,
    ledger: Ledger,
    /// Received bytes that precede `incoming`: bytes a discard kept, or bytes after a complete part's delimiter returned from the buffer. They are a slice of what was received, at most `MAX_HEADER` bytes (see `discard`), held uncharged like `incoming`.
    carry: bytes::Bytes,
    /// What the stream is reading, so that a discard keeps or skips exactly the bytes of the part it gives up.
    part: Part,
    /// Body bytes of a given-up part with a declared length still to be dropped from `carry`, `incoming` and the device before its delimiter.
    skip: usize,
    /// The last copy into the buffer: whether it came from `carry`, the source before the copy, and how many bytes were copied. Bytes after a complete part's delimiter all come from the last copy (an earlier copy would have completed the delimiter), so they return to their source as a slice of it.
    last_copy: Option<(bool, bytes::Bytes, usize)>,
    /// Set when the final frame was given up to a waiting stream, which the next read reports.
    final_reclaimed: bool,
    /// Set when the stream gives up a frame or part of one, and cleared only when it next delivers a frame: it stays set while the stream drops or scans past the given-up part, and while it reads and validates the next part, including while that frame waits for a decoder slot. The actor reports a read it ends at the consumer deadline while this is set as storage exhaustion, since a frame was lost to storage pressure within that window.
    lost: bool,
    /// Set while a reservation is blocked waiting for storage. It stays set when the read is dropped there, so the actor can tell that a read it ended at the consumer deadline was starved, and is cleared when the actor sees the read cancelled and at the start of each read.
    starved: bool,
    validation: Option<JoinHandle<Result<Frame>>>,
    scan_offset: usize,
    #[cfg(test)]
    scanned: usize,
    #[cfg(test)]
    scan_progress: Option<tokio::sync::mpsc::UnboundedSender<usize>>,
    /// The capacity of the buffer after each growth.
    #[cfg(test)]
    growths: Vec<usize>,
    /// Receives the total bytes copied into the buffer after each copy.
    #[cfg(test)]
    copied: Option<tokio::sync::mpsc::UnboundedSender<usize>>,
    #[cfg(test)]
    copied_total: usize,
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
            .headers(device.auth_headers())
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
        session.video.get(url).headers(device.auth_headers()).send(),
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
    terminal: watch::Receiver<Option<Terminal>>,
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

fn storage_exhausted() -> Error {
    Error::Transport("video stream storage exhausted".into())
}

/// Why the actor ended its stream at the consumer deadline: the consumer was idle, or a read was outstanding while the stream was blocked waiting for storage or had given up a frame, or part of one, to storage pressure without delivering a frame since.
#[derive(Clone, Copy)]
enum Terminal {
    Idle,
    Storage,
}

impl Terminal {
    fn error(self) -> Error {
        match self {
            Self::Idle => idle_timeout(),
            Self::Storage => storage_exhausted(),
        }
    }
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
        let (terminal_tx, terminal) = watch::channel(None);
        let actor = tokio::spawn(async move {
            let mut pending: Option<Result<Option<Frame>>> = None;
            let deadline = tokio::time::sleep(idle);
            tokio::pin!(deadline);
            loop {
                let holds_storage = parser.holds_storage() || matches!(pending, Some(Ok(Some(_))));
                let mut request = tokio::select! {
                    _ = &mut deadline => { let _ = terminal_tx.send(Some(Terminal::Idle)); break; }
                    request = incoming.recv() => match request { Some(request) => request, None => break },
                    // No consumer read is outstanding, so this stream is idle and yields what it holds to the waiting stream.
                    _ = pressure.wait_for(|waiting| !waiting.is_empty()), if holds_storage => {
                        parser.discard();
                        if matches!(pending, Some(Ok(Some(_)))) {
                            parser.lost = true;
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
                        // The read was still outstanding at the consumer deadline; if it was waiting for storage, or recovering from a frame it gave up for lack of storage, that is what the consumer is told.
                        _ = &mut deadline => {
                            let terminal = if parser.starved || parser.lost { Terminal::Storage } else { Terminal::Idle };
                            let _ = terminal_tx.send(Some(terminal));
                            break;
                        }
                        result = parser.next_charged() => result,
                        // The consumer cancelled its read, so the stream stops reading and is idle until the next one; what it holds stays for that read unless a stream waits, and it no longer waits for storage itself.
                        () = request.closed() => {
                            parser.starved = false;
                            continue;
                        }
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
                                let _ = terminal_tx.send(Some(Terminal::Idle));
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
            return match *self.terminal.borrow() {
                Some(terminal) => Err(terminal.error()),
                None => Ok(None),
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
            Err(_) => match *self.terminal.borrow() {
                Some(terminal) => Err(terminal.error()),
                None => Ok(None),
            },
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
            carry: bytes::Bytes::new(),
            part: Part::Head,
            skip: 0,
            last_copy: None,
            final_reclaimed: false,
            lost: false,
            starved: false,
            validation: None,
            scan_offset: 0,
            #[cfg(test)]
            scanned: 0,
            #[cfg(test)]
            scan_progress: None,
            #[cfg(test)]
            growths: Vec::new(),
            #[cfg(test)]
            copied: None,
            #[cfg(test)]
            copied_total: 0,
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

    /// Gives up the device bytes this stream holds and releases their charge: a frame waiting for or undergoing validation (which releases its own charge when it ends) and the buffer. What the buffer held of the current part decides what is kept, so no part is skipped that was not given up and no frame body is kept:
    ///
    /// - the opening boundary or a part's headers: kept whole as received bytes, at most `MAX_HEADER`, and read again, so nothing is lost;
    /// - the body of a part with a declared length: the part is given up by count. The rest of its body is dropped from received bytes and the device as it arrives, without scanning it, and its closing delimiter is then checked, so delimiter-like bytes in the body are never taken for one; delimiter bytes already buffered are kept, at most `MAX_BOUNDARY + 5`;
    /// - the body of a part without one, or bytes after such a part: the scan has found no delimiter before `scan_offset`, so only the bytes from there, at most `MAX_BOUNDARY + 5`, are kept, and scanning continues from them.
    ///
    /// Kept bytes precede any received bytes in `carry`, which therefore stays a slice of received bytes of at most `MAX_HEADER`: a later discard keeps a suffix of what an earlier one kept, or bytes of its own buffer within those bounds. A final delimiter reached after giving up a part ends the stream, reporting its final part reclaimed.
    fn discard(&mut self) {
        if let Some(validation) = self.validation.take() {
            validation.abort();
            self.lost = true;
            if self.ended {
                self.final_reclaimed = true;
            }
        }
        if !matches!(self.part, Part::Head) {
            self.lost = true;
        }
        let keep_from = match self.part {
            Part::Head | Part::Delimiter => Some(0),
            Part::Length { delimiter } => {
                if self.buffer.len() <= delimiter {
                    self.skip = delimiter - self.buffer.len();
                    None
                } else {
                    Some(delimiter)
                }
            }
            Part::Scan | Part::Resync => Some(self.scan_offset),
        };
        self.part = match self.part {
            Part::Head => Part::Head,
            Part::Length { .. } | Part::Delimiter => Part::Delimiter,
            Part::Scan | Part::Resync => Part::Resync,
        };
        if let Some(start) = keep_from.filter(|start| *start < self.buffer.len()) {
            let mut kept = self.buffer[start..].to_vec();
            kept.extend_from_slice(&self.carry);
            self.carry = bytes::Bytes::from(kept);
        }
        self.buffer = Vec::new();
        self.scan_offset = 0;
        self.last_copy = None;
        self.release_excess();
    }

    /// Charges the stream for holding `need` bytes in all, waiting at most the read timeout. A reservation that cannot be met at once registers the stream as waiting for as long as it is blocked; other streams yield to waiting streams. A blocked stream that holds charged storage gives it up to any older blocked stream (returning `Reclaimed`, so the read restarts after the bytes it dropped) and so never holds storage an older one needs; a blocked stream that holds none waits in the budget's queue for the storage it needs. A reservation still blocked when the timeout passes fails with "video stream storage exhausted".
    async fn reserve(&mut self, need: usize) -> Read<()> {
        let result = self.reserve_blocking(need).await;
        self.starved = false;
        result
    }

    /// `reserve`, leaving `starved` set while it is blocked, including when it is dropped there.
    async fn reserve_blocking(&mut self, need: usize) -> Read<()> {
        let storage = self.ledger.storage.clone();
        if need > storage.capacity {
            return Err(storage_exceeded().into());
        }
        let permits = |count: usize| u32::try_from(count).map_err(|_| storage_exceeded());
        let id = self.ledger.id;
        let older_blocked =
            move |waiting: &BTreeSet<u64>| waiting.first().is_some_and(|first| *first < id);
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut pressure = storage.waiting.subscribe();
        let mut blocked = None;
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
            if blocked.is_none() {
                blocked = Some(Waiting::new(storage.clone(), id));
            }
            self.starved = true;
            if held > 0 && older_blocked(&pressure.borrow()) {
                self.discard();
                #[cfg(test)]
                storage.yielded.fetch_add(1, Ordering::Release);
                return Err(ReadError::Reclaimed);
            }
            tokio::select! {
                permit = storage.bytes.clone().acquire_many_owned(permits(need - held)?) => {
                    self.ledger.held.merge(permit.expect("stream storage is never closed"));
                    return Ok(());
                }
                _ = pressure.wait_for(older_blocked), if held > 0 => {}
                () = tokio::time::sleep_until(deadline) => return Err(ReadError::Failed(storage_exhausted())),
            }
        }
    }

    /// Receives the next chunk from the device, or `None` at the end of the stream.
    async fn receive(&mut self) -> Read<Option<bytes::Bytes>> {
        if !*self.lease_state.borrow() {
            return Err(Error::Transport("stream lease ended".into()).into());
        }
        tokio::select! {
            changed = self.lease_state.changed() => {
                let _ = changed;
                Err(Error::Transport("stream lease ended".into()).into())
            }
            result = tokio::time::timeout(self.timeout, self.response.chunk()) => Ok(result
                .map_err(|_| Error::Transport("video read timed out".into()))?
                .map_err(|error| crate::request_error("video read", &error))?),
        }
    }

    /// Copies received bytes, from `carry` before `incoming`, into the buffer, which never holds more than `limit` bytes; bytes beyond it stay where they were for the next read. With `exact`, `limit` is the known end of the part and the buffer grows straight to it. Otherwise each growth at least doubles the buffer's capacity, up to `limit`, waiting for that storage when it is not free, so a part is copied a number of times logarithmic in its length and the bytes copied stay linear in it. A buffer grows by copying into a new allocation, and both allocations are charged, before the copy, for the capacity actually allocated, so a buffer at most `limit` long peaks below twice `limit`.
    async fn fill(&mut self, limit: usize, exact: bool) -> Read<bool> {
        if self.carry.is_empty() && self.incoming.is_empty() {
            match self.receive().await? {
                Some(chunk) if chunk.is_empty() => return Ok(true),
                Some(chunk) => self.incoming = chunk,
                None => return Ok(false),
            }
        }
        let from_carry = !self.carry.is_empty();
        let available = if from_carry {
            self.carry.len()
        } else {
            self.incoming.len()
        };
        let count = available.min(limit.saturating_sub(self.buffer.len()));
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
            if grown.capacity() > target {
                self.reserve(capacity + grown.capacity()).await?;
            }
            grown.extend_from_slice(&self.buffer);
            self.buffer = grown;
            #[cfg(test)]
            self.growths.push(self.buffer.capacity());
        }
        let source = if from_carry {
            &mut self.carry
        } else {
            &mut self.incoming
        };
        let chunk = std::mem::take(source);
        absorb_chunk(&mut self.buffer, source, chunk.clone(), limit)?;
        self.last_copy = Some((from_carry, chunk, count));
        #[cfg(test)]
        {
            self.copied_total += count;
            if let Some(copied) = &self.copied {
                let _ = copied.send(self.copied_total);
            }
        }
        self.release_excess();
        Ok(true)
    }

    /// Drops the rest of a given-up part with a declared length, then reads and checks its closing delimiter. A final delimiter ends the stream, reporting the final part reclaimed.
    async fn skip_part(&mut self) -> Read<()> {
        while self.skip > 0 {
            if self.carry.is_empty() && self.incoming.is_empty() {
                match self.receive().await? {
                    Some(chunk) => self.incoming = chunk,
                    None => return Err(Error::InvalidResponse("truncated frame".into()).into()),
                }
                continue;
            }
            let source = if self.carry.is_empty() {
                &mut self.incoming
            } else {
                &mut self.carry
            };
            let count = source.len().min(self.skip);
            *source = source.slice(count..);
            self.skip -= count;
        }
        let delimiter = [b"\r\n".as_slice(), self.boundary.as_slice()].concat();
        let end = delimiter.len() + 2;
        while self.buffer.len() < end {
            if !self.fill(end, true).await? {
                return Err(Error::InvalidResponse("truncated frame".into()).into());
            }
        }
        if !self.buffer.starts_with(&delimiter) {
            return Err(Error::InvalidResponse("invalid frame boundary".into()).into());
        }
        match [self.buffer[end - 2], self.buffer[end - 1]] {
            [b'\r', b'\n'] => {
                self.buffer = Vec::new();
                self.last_copy = None;
                self.release_excess();
                self.part = Part::Head;
                Ok(())
            }
            [b'-', b'-'] => {
                self.ended = true;
                self.lease.abort();
                Err(storage_reclaimed().into())
            }
            _ => Err(Error::InvalidResponse("invalid boundary suffix".into()).into()),
        }
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
        self.starved = false;
        let result = self.read_frame().await;
        if matches!(result, Ok(Some(_))) {
            self.lost = false;
        }
        if result.is_err() || self.ended {
            self.ended = true;
            self.lease.abort();
            self.buffer = Vec::new();
            self.carry = bytes::Bytes::new();
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
    /// Scans the bytes after a given-up part without a declared length for its closing delimiter and continues with the part after it, keeping only enough of the buffer to find a delimiter split across reads. A final delimiter found here closes the given-up part, so the stream ends reporting its final part reclaimed.
    async fn resynchronise(&mut self) -> Read<()> {
        let delimiter = [b"\r\n".as_slice(), self.boundary.as_slice()].concat();
        loop {
            if let Some(relative) = find(&self.buffer[self.scan_offset..], &delimiter) {
                let pos = self.scan_offset + relative;
                let end = pos + delimiter.len() + 2;
                self.scan_offset = pos;
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
                        self.part = Part::Head;
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
        match self.part {
            Part::Resync => self.resynchronise().await?,
            Part::Delimiter => self.skip_part().await?,
            Part::Head | Part::Length { .. } | Part::Scan => {}
        }
        // A read restarted after cancellation parses the part again from its headers, which the buffer still holds.
        self.part = Part::Head;
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
            self.part = Part::Length {
                delimiter: body_start + length,
            };
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
            self.part = Part::Scan;
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
            // Bytes after the part's delimiter come from the last copy (see `last_copy`), so they return to its source as a slice of it, uncharged like the rest of that source; the frame keeps the buffer's allocation, and nothing waits for storage between a complete part and its validation.
            let excess = self.buffer.len() - rest_start;
            let (from_carry, source, count) = self
                .last_copy
                .take()
                .expect("bytes after a delimiter were copied");
            let restored = source.slice(count - excess..);
            if from_carry {
                self.carry = restored;
            } else {
                self.incoming = restored;
            }
            self.buffer.truncate(rest_start);
        }
        self.part = Part::Head;
        self.last_copy = None;
        let mut frame = std::mem::take(&mut self.buffer);
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
        self.release_excess();
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
        let (_terminal_tx, terminal) = watch::channel(None);
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

    // The steps run in a spawned task on the test's current-thread runtime. A correct read passes whatever the scheduling, since no expiry or reclaim task exists while it reads. The test detects a regression that starts one only through tokio's current implementation, in which `yield_now` defers the yielding task behind tasks already woken (`context::defer`), so a task woken by the waiter's registration acts before the read resumes; the documentation lists this ordering among its non-guarantees (https://docs.rs/tokio/1.54.1/tokio/task/fn.yield_now.html).
    #[tokio::test]
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
        // Both blocked holding storage, so the younger stream gave it up.
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

    /// Holds every byte of `storage` now free until dropped.
    fn held_available(storage: &Arc<StreamStorage>) -> OwnedSemaphorePermit {
        storage
            .bytes
            .clone()
            .try_acquire_many_owned(u32::try_from(storage.available()).unwrap())
            .unwrap()
    }

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
        // The rest is larger than the socket buffers can hold while the stream is not reading, so it is written while the next read drops it.
        let rest = body[sent..].to_vec();
        let writer = tokio::spawn(async move { socket.write_all(&rest).await });
        assert!(matches!(
            tokio::time::timeout(BOUND, idle.next_frame()).await.unwrap(),
            Err(Error::Transport(message)) if message == "video frame storage reclaimed for a waiting stream"
        ));
        tokio::time::timeout(BOUND, writer)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn younger_waiting_stream_reports_its_reclaimed_final_part() {
        let last = padded_jpeg([64, 128, 192], 64 * 1024);
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let older = Ledger::new(storage.clone());
        let (response, mut socket) = multipart().await;
        socket.write_all(&part(&last, true, true)).await.unwrap();
        let (mut younger, _lease) = parser(response, storage.clone());
        // Room for the headers but not the part, so the younger stream blocks with storage behind the older one, gives the part up, drops its body by count and reads its final delimiter, however the device's bytes arrive in chunks.
        let _hold = hold_all_but(&storage, MAX_HEADER);
        let _waiter = Waiting::new(storage.clone(), older.id);
        assert!(matches!(
            tokio::time::timeout(BOUND, younger.next_charged()).await.unwrap(),
            Err(Error::Transport(message)) if message == "video frame storage reclaimed for a waiting stream"
        ));
        assert_eq!(storage.yielded.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn younger_stream_gives_up_its_part_and_reads_the_next_with_free_storage() {
        let first = padded_jpeg([64, 128, 192], 64 * 1024);
        let next = jpeg_colour([200, 20, 50]);
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        // An older stream stays blocked throughout.
        let older = Ledger::new(storage.clone());
        let _waiter = Waiting::new(storage.clone(), older.id);
        let mut body = part(&first, true, false);
        body.extend_from_slice(&part(&next, true, true));
        let (response, mut socket) = multipart().await;
        socket.write_all(&body).await.unwrap();
        // The headers and the small next part fit, the first part does not.
        let _hold = hold_all_but(&storage, MAX_HEADER + part_end(&next));
        assert!(part_end(&first) > MAX_HEADER + part_end(&next));
        let (mut younger, _lease) = parser(response, storage.clone());
        // Blocked with storage behind the older stream, the younger gives up its part, drops the part's body by count without storage, and reads the next part with the storage that is free.
        let result = tokio::time::timeout(BOUND, younger.next_charged())
            .await
            .unwrap();
        assert_eq!(to_snapshot(result).unwrap().unwrap().jpeg, next);
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
            storage.yielded.load(Ordering::Acquire) >= 1
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

    /// A parser that has read a stream whose bytes end at `cut` and then given up what it holds, as an idle or deferred stream would, with the socket the rest is sent through and the receiver of the parser's copy totals.
    struct Discarded {
        video: Parser,
        socket: TcpStream,
        copied: tokio::sync::mpsc::UnboundedReceiver<usize>,
        storage: Arc<StreamStorage>,
        _lease: watch::Sender<bool>,
    }

    async fn discarded_at(cut: &[u8]) -> Discarded {
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let (response, mut socket) = multipart().await;
        socket.write_all(cut).await.unwrap();
        let (mut video, lease) = parser(response, storage.clone());
        let (copied_tx, mut copied) = tokio::sync::mpsc::unbounded_channel();
        video.copied = Some(copied_tx);
        {
            let read = video.next_charged();
            tokio::pin!(read);
            let all_copied = async { while copied.recv().await.unwrap() < cut.len() {} };
            tokio::select! {
                result = &mut read => panic!("read finished before the rest was sent: {:?}", result.is_ok()),
                result = tokio::time::timeout(BOUND, all_copied) => result.expect("sent bytes were not copied"),
            }
        }
        assert!(video.holds_storage());
        video.discard();
        assert!(!video.holds_storage());
        Discarded {
            video,
            socket,
            copied,
            storage,
            _lease: lease,
        }
    }

    impl Discarded {
        /// Sends `rest` in one write, ends the stream, and returns the next `reads` results.
        async fn finish(mut self, rest: &[u8], reads: usize) -> Vec<Result<Option<Snapshot>>> {
            self.socket.write_all(rest).await.unwrap();
            drop(self.socket);
            let mut results = Vec::new();
            for _ in 0..reads {
                results.push(to_snapshot(
                    tokio::time::timeout(BOUND, self.video.next_charged())
                        .await
                        .unwrap(),
                ));
            }
            drop(self.video);
            assert_eq!(self.storage.available(), self.storage.capacity());
            results
        }
    }

    /// Reads a stream whose bytes end at `cut`, gives up what the stream holds as an idle or deferred stream would, then sends `rest`, and returns how many received bytes the discard kept and the next read's result.
    async fn read_after_discard_at(cut: &[u8], rest: &[u8]) -> (usize, Result<Option<Snapshot>>) {
        let discarded = discarded_at(cut).await;
        let kept = discarded.video.carry.len();
        let mut results = discarded.finish(rest, 1).await;
        (kept, results.remove(0))
    }

    #[tokio::test]
    async fn discard_after_delimiter_keeps_the_next_part() {
        let next = jpeg_colour([200, 20, 50]);
        let delimiter = b"\r\n--frame".len();
        // A part without a length whose delimiter arrived but not the two bytes after it, and a part with one cut inside its delimiter.
        let unlengthed = part(&jpeg(), false, false);
        let lengthed = part(&jpeg(), true, false);
        // Each part ends with the `\r\n` that begins the delimiter, and the next part begins with the rest of it and its suffix, `--frame\r\n`.
        for (body, cut) in [
            (&unlengthed, unlengthed.len() + b"--frame".len()),
            (&lengthed, lengthed.len() + b"--fr".len()),
        ] {
            let mut stream = body.clone();
            stream.extend_from_slice(&part(&next, true, true));
            let (kept, result) = read_after_discard_at(&stream[..cut], &stream[cut..]).await;
            assert!(kept <= delimiter + 1, "kept {kept} bytes");
            assert_eq!(result.unwrap().unwrap().jpeg, next);
        }
    }

    #[tokio::test]
    async fn discard_before_final_suffix_reports_reclaimed_final_part() {
        let unlengthed = part(&jpeg(), false, true);
        let lengthed = part(&jpeg(), true, true);
        // A part without a length whose final delimiter arrived but not the two bytes after it, a part with one cut inside its final delimiter, and one cut inside its body, which is given up by count.
        let suffix = b"--\r\n".len();
        let body = part(&jpeg(), true, false).len() - jpeg().len() - b"\r\n".len() + 2;
        for (stream, cut) in [
            (&unlengthed, unlengthed.len() - suffix),
            (&lengthed, lengthed.len() - suffix - b"rame".len()),
            (&lengthed, body),
        ] {
            let (_, result) = read_after_discard_at(&stream[..cut], &stream[cut..]).await;
            assert!(
                matches!(
                    &result,
                    Err(Error::Transport(message)) if message == "video frame storage reclaimed for a waiting stream"
                ),
                "{result:?}"
            );
        }
    }

    #[tokio::test]
    async fn declared_length_part_is_given_up_by_count() {
        // A frame whose body holds a delimiter and headers in a comment segment.
        let mut frame = jpeg();
        let fake = b"\r\n--frame\r\nContent-Type: image/jpeg\r\n\r\n";
        let mut segment = vec![0xff, 0xfe];
        segment.extend_from_slice(&u16::try_from(fake.len() + 2).unwrap().to_be_bytes());
        segment.extend_from_slice(fake);
        frame.splice(2..2, segment);
        let next = jpeg_colour([200, 20, 50]);
        let mut stream = part(&frame, true, false);
        let headers = stream.len() - frame.len() - b"\r\n".len();
        stream.extend_from_slice(&part(&next, true, true));
        // The cut is inside the body, after the delimiter-like bytes: the start-of-image marker, the segment's marker and length, and the fake delimiter and headers.
        let cut = headers + 2 + 4 + fake.len() + 1;
        let (kept, result) = read_after_discard_at(&stream[..cut], &stream[cut..]).await;
        assert_eq!(kept, 0, "a body given up by count keeps none of it");
        assert_eq!(result.unwrap().unwrap().jpeg, next);
    }

    #[tokio::test]
    async fn discard_during_preamble_keeps_it() {
        let body = part(&jpeg(), true, true);
        let prefix = b"--fr".len();
        let (kept, result) = read_after_discard_at(&body[..prefix], &body[prefix..]).await;
        assert_eq!(kept, prefix);
        assert_eq!(result.unwrap().unwrap().jpeg, jpeg());

        // An invalid preamble given up is still rejected.
        let mut junk = b"xx".to_vec();
        junk.extend_from_slice(&body);
        let (_, result) = read_after_discard_at(&junk[..2], &junk[2..]).await;
        assert!(
            matches!(&result, Err(Error::InvalidResponse(message)) if message == "invalid multipart preamble"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn discard_during_headers_loses_nothing() {
        let next = jpeg_colour([200, 20, 50]);
        let mut stream = part(&jpeg(), true, false);
        stream.extend_from_slice(&part(&next, true, true));
        let (response, _socket) = multipart().await;
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let (mut video, _lease) = parser(response, storage);
        // The stream has copied part of the first part's headers, and holds the rest of the stream received.
        let opening = b"--frame\r\n".len();
        let headers = part(&jpeg(), true, false).len() - jpeg().len() - b"\r\n".len();
        video.first = false;
        video.buffer = stream[opening..headers].to_vec();
        video.incoming = bytes::Bytes::copy_from_slice(&stream[headers..]);
        video.discard();
        assert!(video.buffer.is_empty());
        assert_eq!(&video.carry[..], &stream[opening..headers]);
        assert_eq!(
            to_snapshot(video.next_charged().await)
                .unwrap()
                .unwrap()
                .jpeg,
            jpeg()
        );
        assert_eq!(
            to_snapshot(video.next_charged().await)
                .unwrap()
                .unwrap()
                .jpeg,
            next
        );
    }

    #[tokio::test]
    async fn stream_reading_after_acquiring_storage_is_not_waiting() {
        let frame = padded_jpeg([64, 128, 192], 64 * 1024);
        let mut first_part = part(&frame, true, false);
        let headers = first_part.len() - frame.len() - b"\r\n".len();
        // The rest of the delimiter and its suffix complete the part.
        first_part.extend_from_slice(b"--frame\r\n");
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let (response, mut socket) = multipart().await;
        let (mut reader, _lease) = parser(response, storage.clone());
        let id = reader.ledger.id;
        let (copied_tx, mut copied) = tokio::sync::mpsc::unbounded_channel();
        reader.copied = Some(copied_tx);
        // The first reservation blocks until the other holder gives way; the stream then reads its headers and waits on the device for the body.
        let hold = hold_all_but(&storage, 0);
        socket.write_all(&first_part[..headers]).await.unwrap();
        let read = tokio::spawn(async move {
            let frame = reader.next_charged().await;
            (reader, frame)
        });
        let mut waiting = storage.waiting.subscribe();
        tokio::time::timeout(BOUND, waiting.wait_for(|waiting| waiting.contains(&id)))
            .await
            .unwrap()
            .unwrap();
        drop(hold);
        tokio::time::timeout(BOUND, async {
            while copied.recv().await.unwrap() < headers {}
        })
        .await
        .unwrap();
        assert!(
            storage.waiting.borrow().is_empty(),
            "a stream that has its storage and only reads still counts as waiting"
        );

        // An idle stream therefore keeps its unclaimed frame meanwhile.
        let idle_frame = jpeg_colour([200, 20, 50]);
        let mut idle_body = part(&idle_frame, true, false);
        idle_body.extend_from_slice(b"--frame\r\n");
        let (idle_response, mut idle_socket) = multipart().await;
        idle_socket.write_all(&idle_body).await.unwrap();
        let (idle_parser, _idle_lease) = parser(idle_response, storage.clone());
        let mut idle = Mjpeg::start(idle_parser, BOUND);
        let (send, receive) = tokio::sync::oneshot::channel();
        idle.requests.send(send).await.unwrap();
        drop(tokio::time::timeout(BOUND, receive).await.unwrap().unwrap());
        assert_eq!(idle.next_frame().await.unwrap().unwrap().jpeg, idle_frame);

        socket.write_all(&first_part[headers..]).await.unwrap();
        let (_, result) = tokio::time::timeout(BOUND, read).await.unwrap().unwrap();
        assert_eq!(result.unwrap().unwrap().jpeg, frame);
    }

    #[tokio::test]
    async fn younger_blocked_stream_acquires_storage_freed_while_an_older_one_reads() {
        let storage = StreamStorage::new(100);
        let (older_response, _older_socket) = multipart().await;
        let (younger_response, _younger_socket) = multipart().await;
        let (mut older, _older_lease) = parser(older_response, storage.clone());
        let (mut younger, _younger_lease) = parser(younger_response, storage.clone());
        let (older_id, younger_id) = (older.ledger.id, younger.ledger.id);
        assert!(older_id < younger_id);
        let mut waiting = storage.waiting.subscribe();
        // The older stream blocks once, then acquires and goes on reading.
        let hold = hold_all_but(&storage, 0);
        {
            let reserve = older.reserve(30);
            tokio::pin!(reserve);
            tokio::select! {
                result = &mut reserve => panic!("acquired without storage: {:?}", result.is_ok()),
                result = tokio::time::timeout(BOUND, waiting.wait_for(|waiting| waiting.contains(&older_id))) => {
                    result.unwrap().unwrap();
                }
            }
            drop(hold);
            assert!(tokio::time::timeout(BOUND, reserve).await.unwrap().is_ok());
        }
        // The younger stream holds storage and blocks for more, which is then freed.
        assert!(younger.reserve(40).await.is_ok());
        let hold = held_available(&storage);
        {
            let reserve = younger.reserve(70);
            tokio::pin!(reserve);
            tokio::select! {
                result = &mut reserve => panic!("acquired without storage: {:?}", result.is_ok()),
                result = tokio::time::timeout(BOUND, waiting.wait_for(|waiting| waiting.contains(&younger_id))) => {
                    result.unwrap().unwrap();
                }
            }
            drop(hold);
            assert!(tokio::time::timeout(BOUND, reserve).await.unwrap().is_ok());
        }
        assert_eq!(storage.yielded.load(Ordering::Acquire), 0);
        assert_eq!(younger.ledger.held.num_permits(), 70);
    }

    #[tokio::test]
    async fn idle_stream_still_dropping_given_up_part_at_deadline_reports_storage_exhausted() {
        let frame = padded_jpeg([64, 128, 192], 64 * 1024);
        let body = part(&frame, true, true);
        let headers = body.len() - frame.len() - b"\r\n--frame--\r\n".len();
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        // The headers fit, the part does not.
        let hold = hold_all_but(&storage, 2 * MAX_HEADER);
        assert!(part_end(&frame) > 2 * MAX_HEADER);
        let (response, mut socket) = multipart().await;
        socket.write_all(&body[..headers]).await.unwrap();
        let (mut video, _lease) = parser(response, storage.clone());
        let id = video.ledger.id;
        let (copied_tx, mut copied) = tokio::sync::mpsc::unbounded_channel();
        video.copied = Some(copied_tx);
        // The read and storage timeout is the bound, so only the consumer deadline, half of it, can end the second read within the bound.
        let mut stream = Mjpeg::start(video, BOUND / 2);
        let mut waiting = storage.waiting.subscribe();
        {
            let read = stream.next_frame();
            tokio::pin!(read);
            let headers_copied = async { while copied.recv().await.unwrap() < headers {} };
            tokio::select! {
                result = &mut read => panic!("read finished before its headers: {result:?}"),
                result = tokio::time::timeout(BOUND, headers_copied) => result.unwrap(),
            }
            // Body bytes arriving after the headers are parsed, more than the header buffer can hold, make the buffer grow to the part, which is what the reservation waits for.
            socket
                .write_all(&body[headers..headers + MAX_HEADER + 1])
                .await
                .unwrap();
            tokio::select! {
                result = &mut read => panic!("read finished without storage: {result:?}"),
                result = tokio::time::timeout(BOUND, waiting.wait_for(|waiting| waiting.contains(&id))) => {
                    result.unwrap().unwrap();
                }
            }
        }
        // Another stream waits, so the idle stream gives up its part, and its next read only drops the part's body from a device that sends nothing more; the frame was lost to storage, which is what the consumer is told.
        let waiter = Waiting::new(storage.clone(), u64::MAX);
        eventually("idle stream kept its part", || {
            storage.available() == 2 * MAX_HEADER
        })
        .await;
        drop(waiter);
        assert!(matches!(
            tokio::time::timeout(BOUND, stream.next_frame()).await.unwrap(),
            Err(Error::Transport(message)) if message == "video stream storage exhausted"
        ));
        drop(hold);
    }

    #[tokio::test]
    async fn starved_read_reports_storage_exhausted_at_the_consumer_deadline() {
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let (response, mut socket) = multipart().await;
        socket.write_all(&part(&jpeg(), true, true)).await.unwrap();
        // The read and storage timeout is the bound, so the consumer deadline, a tenth of it, ends the starved read first.
        let (video, _lease) = parser(response, storage.clone());
        let _hold = hold_all_but(&storage, 0);
        let mut stream = Mjpeg::start(video, BOUND / 10);
        assert!(matches!(
            tokio::time::timeout(BOUND, stream.next_frame()).await.unwrap(),
            Err(Error::Transport(message)) if message == "video stream storage exhausted"
        ));
        assert!(matches!(
            stream.next_frame().await,
            Err(Error::Transport(message)) if message == "video stream storage exhausted"
        ));
    }

    #[tokio::test]
    async fn deferred_stream_still_dropping_given_up_part_at_deadline_reports_storage_exhausted() {
        let frame = padded_jpeg([64, 128, 192], 64 * 1024);
        let body = part(&frame, true, true);
        let headers = body.len() - frame.len() - b"\r\n--frame--\r\n".len();
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        // An older stream stays blocked throughout, and the headers fit but the part does not.
        let older = Ledger::new(storage.clone());
        let _waiter = Waiting::new(storage.clone(), older.id);
        let _hold = hold_all_but(&storage, 2 * MAX_HEADER);
        assert!(part_end(&frame) > 2 * MAX_HEADER);
        let (response, mut socket) = multipart().await;
        // The headers and more body than the header buffer holds; the rest of the body never arrives.
        socket
            .write_all(&body[..headers + MAX_HEADER + 1])
            .await
            .unwrap();
        let (video, _lease) = parser(response, storage.clone());
        // The read and storage timeout is the bound, so only the consumer deadline, half of it, can end the read within the bound. The stream gives its part up to the older one and is still dropping the part's body then.
        let mut stream = Mjpeg::start(video, BOUND / 2);
        assert!(matches!(
            tokio::time::timeout(BOUND, stream.next_frame()).await.unwrap(),
            Err(Error::Transport(message)) if message == "video stream storage exhausted"
        ));
        assert_eq!(storage.yielded.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn frame_delivered_after_a_give_up_clears_the_loss() {
        let next = jpeg_colour([200, 20, 50]);
        let mut stream = part(&jpeg(), true, false);
        let headers = stream.len() - jpeg().len() - b"\r\n".len();
        stream.extend_from_slice(&part(&next, true, true));
        let cut = headers + 2;
        let mut discarded = discarded_at(&stream[..cut]).await;
        assert!(discarded.video.lost);
        discarded.socket.write_all(&stream[cut..]).await.unwrap();
        let result = tokio::time::timeout(BOUND, discarded.video.next_charged())
            .await
            .unwrap();
        assert_eq!(to_snapshot(result).unwrap().unwrap().jpeg, next);
        assert!(!discarded.video.lost);

        // Giving up headers loses nothing.
        let body = part(&jpeg(), true, true);
        let discarded = discarded_at(&body[..b"--frame\r\nContent".len()]).await;
        assert!(!discarded.video.lost);
    }

    /// An unlengthed part of a large padded frame followed by `after`, and the offset of the given-up part's closing delimiter.
    fn unlengthed_then(after: &[u8]) -> (Vec<u8>, usize) {
        let mut stream = part(&padded_jpeg([64, 128, 192], 64 * 1024), false, false);
        let delimiter = stream.len() - b"\r\n".len();
        stream.extend_from_slice(after);
        (stream, delimiter)
    }

    #[tokio::test]
    async fn unlengthed_part_given_up_mid_body_resumes_at_its_delimiter() {
        let next = jpeg_colour([200, 20, 50]);
        let (stream, delimiter) = unlengthed_then(&part(&next, true, true));
        let headers = part(&jpeg(), false, false).len() - jpeg().len() - b"\r\n".len();
        // Cuts just inside the body, part way through it, and just before the delimiter: the rest of the body is longer than a header buffer in the first two, so resynchronising drains and refills it.
        for cut in [headers + 2, headers + 1000, delimiter - 1] {
            let (kept, result) = read_after_discard_at(&stream[..cut], &stream[cut..]).await;
            assert!(kept < b"\r\n--frame".len(), "kept {kept} bytes at {cut}");
            assert_eq!(result.unwrap().unwrap().jpeg, next, "cut at {cut}");
        }
    }

    #[tokio::test]
    async fn resynchronising_finds_a_delimiter_received_one_byte_at_a_time() {
        let next = jpeg_colour([200, 20, 50]);
        let (stream, delimiter) = unlengthed_then(&part(&next, true, true));
        let cut = delimiter - 100;
        let mut discarded = discarded_at(&stream[..cut]).await;
        // Resynchronising copies the kept bytes, then every byte received, once each.
        let mut expected =
            discarded.video.copied_total + discarded.video.carry.len() + (delimiter - cut);
        discarded
            .socket
            .write_all(&stream[cut..delimiter])
            .await
            .unwrap();
        let read = discarded.video.next_charged();
        tokio::pin!(read);
        // Each delimiter byte is sent only once the parser has copied everything before it, so each arrives in a chunk of its own.
        for index in delimiter..delimiter + b"\r\n--frame\r\n".len() {
            let copied = &mut discarded.copied;
            let caught_up = async { while copied.recv().await.unwrap() < expected {} };
            tokio::select! {
                result = &mut read => panic!("read finished before the delimiter: {:?}", result.is_ok()),
                result = tokio::time::timeout(BOUND, caught_up) => result.expect("bytes were not copied"),
            }
            discarded
                .socket
                .write_all(&stream[index..=index])
                .await
                .unwrap();
            expected += 1;
        }
        discarded
            .socket
            .write_all(&stream[delimiter + b"\r\n--frame\r\n".len()..])
            .await
            .unwrap();
        let result = tokio::time::timeout(BOUND, read).await.unwrap();
        assert_eq!(to_snapshot(result).unwrap().unwrap().jpeg, next);
    }

    #[tokio::test]
    async fn resynchronising_skips_a_delimiter_with_a_bad_suffix() {
        let next = jpeg_colour([200, 20, 50]);
        let mut frame = jpeg();
        let fake = b"\r\n--frameX";
        let mut segment = vec![0xff, 0xfe];
        segment.extend_from_slice(&u16::try_from(fake.len() + 2).unwrap().to_be_bytes());
        segment.extend_from_slice(fake);
        frame.splice(2..2, segment);
        let mut stream = part(&frame, false, false);
        let headers = stream.len() - frame.len() - b"\r\n".len();
        stream.extend_from_slice(&part(&next, true, true));
        // Given up before the delimiter-like bytes, so resynchronising meets them first.
        let cut = headers + 2;
        let (_, result) = read_after_discard_at(&stream[..cut], &stream[cut..]).await;
        assert_eq!(result.unwrap().unwrap().jpeg, next);
    }

    #[tokio::test]
    async fn resynchronising_copy_holding_the_next_parts_returns_the_rest() {
        let second = jpeg_colour([200, 20, 50]);
        let third = jpeg_colour([20, 200, 50]);
        let mut stream = part(&jpeg(), false, false);
        let headers = stream.len() - jpeg().len() - b"\r\n".len();
        stream.extend_from_slice(&part(&second, true, false));
        stream.extend_from_slice(&part(&third, true, true));
        // The rest of the given-up part, the whole second part and the third arrive in one write, so resynchronising's copy can hold the second part and the start of the third; the bytes after the second part's delimiter return from that copy.
        let cut = headers + 2;
        let results = discarded_at(&stream[..cut])
            .await
            .finish(&stream[cut..], 3)
            .await;
        let mut results = results.into_iter();
        assert_eq!(results.next().unwrap().unwrap().unwrap().jpeg, second);
        assert_eq!(results.next().unwrap().unwrap().unwrap().jpeg, third);
        assert!(results.next().unwrap().unwrap().is_none());
    }

    #[tokio::test]
    async fn given_up_part_longer_or_shorter_than_declared_is_rejected() {
        let frame = jpeg();
        let next = part(&jpeg_colour([200, 20, 50]), true, true);
        for (declared, extra) in [
            (frame.len(), b"extra".as_slice()),
            (frame.len() + 5, b"".as_slice()),
        ] {
            let mut stream = format!(
                "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {declared}\r\n\r\n"
            )
            .into_bytes();
            let headers = stream.len();
            stream.extend_from_slice(&frame);
            stream.extend_from_slice(extra);
            stream.extend_from_slice(b"\r\n");
            stream.extend_from_slice(&next);
            let cut = headers + 2;
            let (_, result) = read_after_discard_at(&stream[..cut], &stream[cut..]).await;
            assert!(
                matches!(&result, Err(Error::InvalidResponse(message)) if message == "invalid frame boundary"),
                "declared {declared}: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn second_discard_while_dropping_a_given_up_body_changes_nothing() {
        let next = jpeg_colour([200, 20, 50]);
        let mut stream = part(&jpeg(), true, false);
        let headers = stream.len() - jpeg().len() - b"\r\n".len();
        stream.extend_from_slice(&part(&next, true, true));
        let cut = headers + 2;
        let mut discarded = discarded_at(&stream[..cut]).await;
        let skip = discarded.video.skip;
        assert!(skip > 0);
        discarded.video.discard();
        assert_eq!(discarded.video.skip, skip);
        assert!(discarded.video.carry.is_empty());
        assert_eq!(discarded.video.ledger.held.num_permits(), 0);
        let results = discarded.finish(&stream[cut..], 1).await;
        assert_eq!(results[0].as_ref().unwrap().as_ref().unwrap().jpeg, next);
    }

    #[tokio::test]
    async fn discard_of_a_partial_delimiter_keeps_it_ahead_of_received_bytes() {
        let next = jpeg_colour([200, 20, 50]);
        let mut rest = b"ame\r\n".to_vec();
        rest.extend_from_slice(&part(&next, true, true)[b"--frame\r\n".len()..]);
        let (response, mut socket) = multipart().await;
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let (mut video, _lease) = parser(response, storage.clone());
        // The stream has dropped a given-up body and copied the start of its delimiter. While the delimiter is incomplete both received-byte sources are empty: the delimiter read grows the buffer straight to the delimiter's end, each copy either reaches that end or empties its source (the carry first, then the received chunk, without suspending between them), and the read suspends with part of the delimiter buffered only to receive more (its one storage wait comes before the first copy, with the buffer empty), so a discard that keeps delimiter bytes has nothing received after them.
        video.first = false;
        video.part = Part::Delimiter;
        video.buffer = b"\r\n--fr".to_vec();
        video.discard();
        assert!(video.buffer.is_empty());
        assert!(video.incoming.is_empty());
        assert_eq!(&video.carry[..], b"\r\n--fr");
        // The rest arrives later, and is read after the kept bytes.
        socket.write_all(&rest).await.unwrap();
        assert_eq!(
            to_snapshot(video.next_charged().await)
                .unwrap()
                .unwrap()
                .jpeg,
            next
        );
        drop(video);
        assert_eq!(storage.available(), storage.capacity());
    }

    #[tokio::test]
    async fn headers_discarded_again_are_kept_once() {
        let next = jpeg_colour([200, 20, 50]);
        let mut stream = part(&jpeg(), true, false);
        stream.extend_from_slice(&part(&next, true, true));
        let opening = b"--frame\r\n".len();
        let headers = part(&jpeg(), true, false).len() - jpeg().len() - b"\r\n".len();
        let (response, _socket) = multipart().await;
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let (mut video, _lease) = parser(response, storage.clone());
        // Headers an earlier discard kept, partly copied into the buffer again.
        video.first = false;
        video.buffer = stream[opening..opening + 10].to_vec();
        video.carry = bytes::Bytes::copy_from_slice(&stream[opening + 10..headers]);
        video.incoming = bytes::Bytes::copy_from_slice(&stream[headers..]);
        video.discard();
        assert_eq!(&video.carry[..], &stream[opening..headers]);
        assert!(video.carry.len() <= MAX_HEADER);
        assert_eq!(video.ledger.held.num_permits(), 0);
        assert_eq!(
            to_snapshot(video.next_charged().await)
                .unwrap()
                .unwrap()
                .jpeg,
            jpeg()
        );
        assert_eq!(
            to_snapshot(video.next_charged().await)
                .unwrap()
                .unwrap()
                .jpeg,
            next
        );
        drop(video);
        assert_eq!(storage.available(), storage.capacity());
    }

    #[tokio::test]
    async fn read_after_an_unclaimed_frame_was_reclaimed_reports_storage_exhausted() {
        let mut body = part(&jpeg(), true, false);
        body.extend_from_slice(b"--frame\r\n");
        let (response, mut socket) = multipart().await;
        socket.write_all(&body).await.unwrap();
        let storage = StreamStorage::new(STREAM_STORAGE_BYTES);
        let (parser, _lease) = parser(response, storage.clone());
        // The read and storage timeout is the bound, so only the consumer deadline, half of it, can end the next read within the bound.
        let mut stream = Mjpeg::start(parser, BOUND / 2);
        let (send, receive) = tokio::sync::oneshot::channel();
        stream.requests.send(send).await.unwrap();
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
        // The device sends nothing more, so the next read ends at the consumer deadline, with the frame lost to storage since the last delivery.
        assert!(matches!(
            tokio::time::timeout(BOUND, stream.next_frame()).await.unwrap(),
            Err(Error::Transport(message)) if message == "video stream storage exhausted"
        ));
    }

    #[tokio::test]
    async fn read_after_a_queued_validation_was_reclaimed_reports_storage_exhausted() {
        let jpeg = jpeg();
        let mut body = part(&jpeg, true, false);
        body.extend_from_slice(b"--frame\r\n");
        let (response, mut socket) = multipart().await;
        socket.write_all(&body).await.unwrap();
        let decoders = Arc::new(Semaphore::new(0));
        let storage = StreamStorage::with_decoders(STREAM_STORAGE_BYTES, decoders);
        let (parser, _lease) = parser(response, storage.clone());
        let mut stream = Mjpeg::start(parser, BOUND / 2);
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
        // The frame is complete, so the stream is between parts; giving up its queued validation is the only loss.
        let waiter = Waiting::new(storage.clone(), u64::MAX);
        eventually("idle stream kept its queued frame", || {
            storage.available() == storage.capacity()
        })
        .await;
        drop(waiter);
        assert!(matches!(
            tokio::time::timeout(BOUND, stream.next_frame()).await.unwrap(),
            Err(Error::Transport(message)) if message == "video stream storage exhausted"
        ));
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
