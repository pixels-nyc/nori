//! Slot → viewer drag-and-drop export channel.
//!
//! Slot drags live entirely inside the slot compositor, so a gesture that starts
//! over slot content and ends over a host window would drop twice: once on the
//! host (via the viewer) and once inside the slot on button release. This module
//! serves a small JSON-line IPC socket, `nori-dnd-{slot}.sock`, that lets the
//! viewer:
//!
//! - learn that a slot drag started, with its MIME types (`drag_started`),
//! - pull the dragged bytes per MIME type (`get_data`; copied eagerly at drag
//!   start so the data survives the slot drag being cancelled),
//! - cancel the slot-side drag once the host pointer leaves the viewer surface
//!   (`cancel_drag`), so the gesture completes exactly once, on the host.
//!
//! Only headless slot instances serve the socket (gated on
//! `NORI_HEADLESS_SLOT` / `PIXOS_HEAD`). When no viewer is watching, drags cost
//! nothing extra.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::thread;

use calloop::channel::Sender;
use serde::{Deserialize, Serialize};
use smithay::backend::input::InputTime;
use smithay::input::dnd::{DnDGrab, DndAction, Source, SourceMetadata};
use smithay::input::pointer::{Focus, GrabStartData as PointerGrabStartData};
use smithay::utils::{IsAlive, SERIAL_COUNTER};

use crate::niri::{Niri, State};

/// Milliseconds to wait for a drag source to produce one MIME payload.
const PULL_TIMEOUT_MS: i32 = 3000;
/// Per-MIME cap on eagerly copied drag data.
const PULL_SIZE_CAP: usize = 16 * 1024 * 1024;

fn slot_id() -> Option<u32> {
    std::env::var("NORI_HEADLESS_SLOT")
        .or_else(|_| std::env::var("PIXOS_HEAD"))
        .ok()?
        .parse()
        .ok()
}

fn socket_path(slot: u32) -> std::path::PathBuf {
    let uid = unsafe { libc::getuid() };
    std::path::PathBuf::from(format!("/run/user/{uid}/nori-dnd-{slot}.sock"))
}

#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Request {
    Watch,
    CancelDrag,
    GetData { id: u64, mime: String },
    Ping,
    ExternalDragStarted { mimes: Vec<String> },
    ExternalDragData { mime: String, len: usize },
    ExternalDragFailed { mime: String },
    ExternalDragEnded,
}

#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Response<'a> {
    Watching {
        gen: u64,
    },
    DragStarted {
        gen: u64,
        mimes: &'a [String],
        actions: &'a [String],
    },
    DragEnded {
        gen: u64,
        reason: &'a str,
    },
    DataReady {
        gen: u64,
        mime: &'a str,
        ok: bool,
    },
    Data {
        id: u64,
        mime: &'a str,
        len: usize,
    },
    NoData {
        id: u64,
        pending: bool,
    },
    DragCancelled {
        ok: bool,
    },
    ExternalStarted {
        ok: bool,
    },
    NeedData {
        mime: &'a str,
    },
    Pong,
}

/// Messages from the IPC listener / puller threads to the compositor thread.
#[derive(Debug)]
pub enum DndIpcMsg {
    Watch(UnixStream),
    CancelDrag(UnixStream),
    GetData {
        id: u64,
        mime: String,
        reply_to: UnixStream,
    },
    DataReady {
        gen: u64,
        mime: String,
        bytes: Option<Vec<u8>>,
    },
    Ping(UnixStream),
    ExternalStarted {
        mimes: Vec<String>,
        reply_to: UnixStream,
    },
    ExternalData {
        mime: String,
        bytes: Vec<u8>,
    },
    ExternalFailed {
        mime: String,
    },
    ExternalNeed {
        mime: String,
    },
    ExternalEnded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragEndReason {
    Dropped,
    Cancelled,
}

impl DragEndReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Dropped => "dropped",
            Self::Cancelled => "cancelled",
        }
    }
}

enum DragBytes {
    Pending,
    Ready(Vec<u8>),
    Failed,
}

pub struct DragExport {
    gen: u64,
    mimes: Vec<String>,
    actions: Vec<String>,
    data: HashMap<String, DragBytes>,
    grab_is_touch: bool,
}

#[derive(Default)]
pub struct NoriDndState {
    watchers: Vec<UnixStream>,
    gen: u64,
    current: Option<DragExport>,
    external: Option<ExternalDrag>,
    tx: Option<Sender<DndIpcMsg>>,
}

/// A host drag injected into the slot by the viewer (milestone 2).
///
/// The viewer reports the host drag's MIME types; we run a real `DnDGrab` with
/// a virtual source so slot targets see an ordinary drag. Motion and release
/// already flow through the virtual pointer, so only grab creation and the
/// data feed are synthetic.
pub struct ExternalDrag {
    source: ViewerDndSource,
}

#[derive(Debug, Default)]
struct ExternalShared {
    cache: HashMap<String, Vec<u8>>,
    failed: HashSet<String>,
    pending: Vec<(String, OwnedFd)>,
}

/// A `dnd::Source` fed by the viewer over the DnD socket.
#[derive(Clone)]
struct ViewerDndSource {
    mimes: Vec<String>,
    shared: Arc<Mutex<ExternalShared>>,
    tx: Sender<DndIpcMsg>,
}

impl IsAlive for ViewerDndSource {
    fn alive(&self) -> bool {
        true
    }
}

impl Source for ViewerDndSource {
    fn metadata(&self) -> Option<SourceMetadata> {
        let mut actions = SourceMetadata::default().dnd_actions;
        actions.push(DndAction::Copy);
        actions.push(DndAction::Move);
        Some(SourceMetadata {
            mime_types: self.mimes.clone(),
            dnd_actions: actions,
        })
    }

    fn send(&self, mime_type: &str, fd: OwnedFd) {
        let mut shared = self.shared.lock().unwrap();
        if let Some(bytes) = shared.cache.get(mime_type) {
            let mut file = std::fs::File::from(fd);
            let _ = file.write_all(bytes);
            // fd closed on drop: end of data.
        } else if shared.failed.contains(mime_type) {
            // Closed immediately: empty payload.
            drop(fd);
        } else {
            shared.pending.push((mime_type.to_string(), fd));
            // Ask the viewer (again) for this MIME type. The loopback goes
            // through the main loop so it can broadcast to all watchers.
            let _ = self.tx.send(DndIpcMsg::ExternalNeed {
                mime: mime_type.to_string(),
            });
        }
    }

    fn accepted(&self, _mime_type: Option<String>) {}
    fn choose_action(&self, _action: DndAction) {}
    fn drop_performed(&self) {}
    fn cancel(&self) {}
    fn finished(&self) {}
}

impl ExternalDrag {
    fn push_data(&mut self, mime: &str, bytes: Vec<u8>) {
        let mut shared = self.source.shared.lock().unwrap();
        // Fulfill queued sends for this MIME type (fds close after writing).
        let pending = std::mem::take(&mut shared.pending);
        let mut still_pending = Vec::with_capacity(pending.len());
        for (pending_mime, fd) in pending {
            if pending_mime == mime {
                let mut file = std::fs::File::from(fd);
                let _ = file.write_all(&bytes);
            } else {
                still_pending.push((pending_mime, fd));
            }
        }
        shared.pending = still_pending;
        shared.cache.insert(mime.to_string(), bytes);
    }

    fn push_failed(&mut self, mime: &str) {
        let mut shared = self.source.shared.lock().unwrap();
        shared.failed.insert(mime.to_string());
        shared.pending.retain(|(pending_mime, _)| pending_mime != mime);
    }
}

fn action_name(action: DndAction) -> &'static str {
    match action {
        DndAction::None => "none",
        DndAction::Copy => "copy",
        DndAction::Move => "move",
        DndAction::Ask => "ask",
    }
}

fn send_line(stream: &mut UnixStream, value: &Response) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value).unwrap();
    line.push(b'\n');
    stream.write_all(&line)
}

impl NoriDndState {
    fn broadcast(&mut self, value: &Response) {
        self.watchers.retain_mut(|w| send_line(w, value).is_ok());
    }

    /// Called from `dnd_requested`, before the source moves into the grab.
    pub fn note_drag_started<S: Source>(&mut self, source: &S, grab_is_touch: bool) {
        self.gen += 1;
        let gen = self.gen;
        let meta: SourceMetadata = source.metadata().unwrap_or_default();
        let actions = meta
            .dnd_actions
            .iter()
            .map(|a| action_name(*a).to_string())
            .collect::<Vec<_>>();

        let mut current = DragExport {
            gen,
            mimes: meta.mime_types.clone(),
            actions,
            data: HashMap::new(),
            grab_is_touch,
        };

        // Copy payloads eagerly while the source is alive. Without watchers
        // there is nobody to export to, so skip the pipes entirely.
        if !self.watchers.is_empty() {
            if let Some(tx) = &self.tx {
                for mime in &meta.mime_types {
                    current.data.insert(mime.clone(), DragBytes::Pending);
                    let mut fds = [0; 2];
                    // SAFETY: pipe2 writes exactly two fds on success.
                    let ok = unsafe {
                        libc::pipe2(
                            fds.as_mut_ptr(),
                            libc::O_CLOEXEC | libc::O_NONBLOCK,
                        ) == 0
                    };
                    if !ok {
                        current
                            .data
                            .insert(mime.clone(), DragBytes::Failed);
                        continue;
                    }
                    // SAFETY: fds are freshly created and owned by us.
                    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
                    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
                    source.send(mime, write);
                    let tx = tx.clone();
                    let mime = mime.clone();
                    thread::spawn(move || {
                        let bytes = read_pipe(read);
                        let _ = tx.send(DndIpcMsg::DataReady { gen, mime, bytes });
                    });
                }
            }
        }

        self.broadcast(&Response::DragStarted {
            gen,
            mimes: &current.mimes,
            actions: &current.actions,
        });
        self.current = Some(current);
    }

    pub fn note_drag_ended(&mut self, reason: DragEndReason) {
        // An injected external drag ends through this same hook (its grab's
        // drop/cancel); dropping it closes any still-pending send fds so slot
        // targets see EOF rather than hanging.
        self.external = None;
        if self.current.take().is_none() {
            return;
        }
        self.broadcast(&Response::DragEnded {
            gen: self.gen,
            reason: reason.as_str(),
        });
    }

    fn on_data_ready(&mut self, gen: u64, mime: String, bytes: Option<Vec<u8>>) {
        let Some(current) = &mut self.current else {
            return;
        };
        if current.gen != gen {
            return;
        }
        let ok = bytes.is_some();
        current.data.insert(
            mime.clone(),
            bytes.map(DragBytes::Ready).unwrap_or(DragBytes::Failed),
        );
        self.broadcast(&Response::DataReady {
            gen,
            mime: &mime,
            ok,
        });
    }

    fn on_get_data(&mut self, id: u64, mime: &str, mut reply_to: UnixStream) {
        let payload = self
            .current
            .as_ref()
            .and_then(|current| current.data.get(mime));
        match payload {
            Some(DragBytes::Ready(bytes)) => {
                let header = Response::Data {
                    id,
                    mime,
                    len: bytes.len(),
                };
                if send_line(&mut reply_to, &header).is_ok() {
                    let _ = reply_to.write_all(bytes);
                }
            }
            Some(DragBytes::Pending) => {
                let _ = send_line(
                    &mut reply_to,
                    &Response::NoData { id, pending: true },
                );
            }
            _ => {
                let _ = send_line(
                    &mut reply_to,
                    &Response::NoData {
                        id,
                        pending: false,
                    },
                );
            }
        }
    }

    fn on_watch(&mut self, mut stream: UnixStream) {
        // Replay the active drag, if any, so a viewer attaching mid-gesture
        // sees the same state as one that watched from the start.
        let replay = self.current.as_ref().map(|current| {
            (
                current.gen,
                current.mimes.clone(),
                current.actions.clone(),
                current
                    .data
                    .iter()
                    .filter_map(|(mime, bytes)| match bytes {
                        DragBytes::Ready(_) => Some((mime.clone(), true)),
                        DragBytes::Failed => Some((mime.clone(), false)),
                        DragBytes::Pending => None,
                    })
                    .collect::<Vec<_>>(),
            )
        });
        if send_line(&mut stream, &Response::Watching { gen: self.gen }).is_err() {
            return;
        }
        if let Some((gen, mimes, actions, ready)) = replay {
            let _ = send_line(
                &mut stream,
                &Response::DragStarted {
                    gen,
                    mimes: &mimes,
                    actions: &actions,
                },
            );
            for (mime, ok) in ready {
                let _ = send_line(
                    &mut stream,
                    &Response::DataReady {
                        gen,
                        mime: &mime,
                        ok,
                    },
                );
            }
        }
        self.watchers.push(stream);
    }
}

/// Read one puller pipe to EOF, timeout, or [`PULL_SIZE_CAP`].
///
/// Returns `None` only when nothing arrived before the timeout with no EOF
/// (source never responded); empty and partial payloads are `Some`.
fn read_pipe(fd: OwnedFd) -> Option<Vec<u8>> {
    read_pipe_with_timeout(fd, PULL_TIMEOUT_MS as u64)
}

fn read_pipe_with_timeout(fd: OwnedFd, timeout_ms: u64) -> Option<Vec<u8>> {
    use std::os::fd::AsRawFd;
    let mut file = std::fs::File::from(fd);
    let raw = file.as_raw_fd();
    let mut out = Vec::new();
    let mut buf = [0u8; 65536];
    let mut saw_eof = false;
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd: raw,
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        // SAFETY: pfd points to a valid single-entry array.
        let ready = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis().min(500) as i32) };
        if ready <= 0 {
            continue;
        }
        match file.read(&mut buf) {
            Ok(0) => {
                saw_eof = true;
                break;
            }
            Ok(n) => {
                let room = PULL_SIZE_CAP.saturating_sub(out.len());
                out.extend_from_slice(&buf[..n.min(room)]);
                if out.len() >= PULL_SIZE_CAP {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => return None,
        }
    }
    if saw_eof || !out.is_empty() {
        Some(out)
    } else {
        None
    }
}

impl Niri {
    /// Serve the viewer DnD socket (headless slots only). Safe to call on any
    /// instance; non-slot instances get no socket and keep `tx` unset.
    pub fn nori_dnd_setup(&mut self) {
        let Some(slot) = slot_id() else {
            return;
        };
        let (tx, rx) = calloop::channel::channel::<DndIpcMsg>();
        self.nori_dnd.tx = Some(tx.clone());
        self.event_loop
            .insert_source(rx, |event, _, state| match event {
                calloop::channel::Event::Msg(msg) => state.handle_dnd_ipc(msg),
                calloop::channel::Event::Closed => (),
            })
            .unwrap();

        let path = socket_path(slot);
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(err) => {
                tracing::warn!("nori-dnd: cannot bind {}: {err:?}", path.display());
                self.nori_dnd.tx = None;
                return;
            }
        };
        tracing::info!("nori-dnd: serving {}", path.display());
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else { continue };
                let tx = tx.clone();
                thread::spawn(move || serve_conn(stream, tx));
            }
        });
    }
}

fn serve_conn(stream: UnixStream, tx: Sender<DndIpcMsg>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            continue;
        };
        let Ok(req) = serde_json::from_str::<Request>(text) else {
            continue;
        };
        // Binary frame: the JSON header is followed by exactly `len` bytes.
        if let Request::ExternalDragData { mime, len } = &req {
            if *len > PULL_SIZE_CAP {
                continue;
            }
            let mut bytes = vec![0u8; *len];
            if reader.read_exact(&mut bytes).is_err() {
                break;
            }
            if tx
                .send(DndIpcMsg::ExternalData {
                    mime: mime.clone(),
                    bytes,
                })
                .is_err()
            {
                break;
            }
            continue;
        }
        let Ok(reply) = stream.try_clone() else { break };
        let msg = match req {
            Request::Watch => DndIpcMsg::Watch(reply),
            Request::CancelDrag => DndIpcMsg::CancelDrag(reply),
            Request::GetData { id, mime } => DndIpcMsg::GetData {
                id,
                mime,
                reply_to: reply,
            },
            Request::Ping => DndIpcMsg::Ping(reply),
            Request::ExternalDragStarted { mimes } => DndIpcMsg::ExternalStarted {
                mimes,
                reply_to: reply,
            },
            Request::ExternalDragData { .. } => unreachable!(),
            Request::ExternalDragFailed { mime } => DndIpcMsg::ExternalFailed { mime },
            Request::ExternalDragEnded => DndIpcMsg::ExternalEnded,
        };
        if tx.send(msg).is_err() {
            break;
        }
    }
}

impl State {
    fn handle_dnd_ipc(&mut self, msg: DndIpcMsg) {
        match msg {
            DndIpcMsg::Watch(stream) => self.niri.nori_dnd.on_watch(stream),
            DndIpcMsg::CancelDrag(mut reply) => {
                let ok = self.cancel_dnd_for_viewer();
                let _ = send_line(&mut reply, &Response::DragCancelled { ok });
            }
            DndIpcMsg::GetData { id, mime, reply_to } => {
                self.niri.nori_dnd.on_get_data(id, &mime, reply_to);
            }
            DndIpcMsg::DataReady { gen, mime, bytes } => {
                self.niri.nori_dnd.on_data_ready(gen, mime, bytes);
            }
            DndIpcMsg::Ping(mut reply) => {
                let _ = send_line(&mut reply, &Response::Pong);
            }
            DndIpcMsg::ExternalStarted { mimes, mut reply_to } => {
                let ok = self.start_external_drag(mimes);
                let _ = send_line(&mut reply_to, &Response::ExternalStarted { ok });
            }
            DndIpcMsg::ExternalData { mime, bytes } => {
                if let Some(external) = self.niri.nori_dnd.external.as_mut() {
                    external.push_data(&mime, bytes);
                }
            }
            DndIpcMsg::ExternalFailed { mime } => {
                if let Some(external) = self.niri.nori_dnd.external.as_mut() {
                    external.push_failed(&mime);
                }
            }
            DndIpcMsg::ExternalNeed { mime } => {
                self.niri
                    .nori_dnd
                    .broadcast(&Response::NeedData { mime: &mime });
            }
            DndIpcMsg::ExternalEnded => {
                self.end_external_drag();
            }
        }
    }

    /// Inject a host drag as a real slot `DnDGrab` with a viewer-fed source.
    ///
    /// Refuses when the pointer already holds a grab, so we never steal an
    /// in-progress move, resize, or client drag.
    fn start_external_drag(&mut self, mimes: Vec<String>) -> bool {
        if mimes.is_empty() {
            return false;
        }
        // Replace a stale external drag, if any.
        self.end_external_drag();

        let Some(pointer) = self.niri.seat.get_pointer() else {
            return false;
        };
        if pointer.is_grabbed() {
            return false;
        }
        let Some(tx) = self.niri.nori_dnd.tx.clone() else {
            return false;
        };
        let source = ViewerDndSource {
            mimes,
            shared: Arc::new(Mutex::new(ExternalShared::default())),
            tx,
        };
        let start_data = PointerGrabStartData {
            focus: None,
            button: 272, // BTN_LEFT; motion/release flow through forwarding.
            location: pointer.current_location(),
        };
        let seat = self.niri.seat.clone();
        let grab = DnDGrab::new_pointer(
            &self.niri.display_handle,
            start_data,
            source.clone(),
            seat,
        );
        pointer.set_grab(
            self,
            grab,
            SERIAL_COUNTER.next_serial(),
            Focus::Keep,
        );
        self.niri.nori_dnd.external = Some(ExternalDrag { source });
        self.niri.queue_redraw_all();
        true
    }

    /// End the injected drag, if ours is still the active grab.
    fn end_external_drag(&mut self) {
        if self.niri.nori_dnd.external.is_none() {
            return;
        }
        // Only unset when the grab is still a DnD grab: if something replaced
        // it, the replacement's own lifecycle already ended our drag.
        if let Some(pointer) = self.niri.seat.get_pointer() {
            let is_dnd = pointer
                .with_grab(|_, grab| Self::is_dnd_grab(grab.as_any()))
                .unwrap_or(false);
            if is_dnd {
                pointer.unset_grab(
                    self,
                    SERIAL_COUNTER.next_serial(),
                    InputTime::now(),
                );
            }
        }
        self.niri.nori_dnd.external = None;
    }

    /// Cancel the active drag, if the current grab is a DnD grab. Returns
    /// false when there is no drag (or the grab is something else, e.g. a
    /// move/resize), so the viewer can tell "nothing to cancel" apart.
    fn cancel_dnd_for_viewer(&mut self) -> bool {
        if let Some(pointer) = self.niri.seat.get_pointer() {
            let is_dnd = pointer
                .with_grab(|_, grab| Self::is_dnd_grab(grab.as_any()))
                .unwrap_or(false);
            if is_dnd {
                pointer.unset_grab(
                    self,
                    SERIAL_COUNTER.next_serial(),
                    InputTime::now(),
                );
                return true;
            }
        }
        if let Some(touch) = self.niri.seat.get_touch() {
            let tracked_touch_drag = self
                .niri
                .nori_dnd
                .current
                .as_ref()
                .is_some_and(|current| current.grab_is_touch);
            if tracked_touch_drag && touch.is_grabbed() {
                touch.unset_grab(self);
                return true;
            }
        }
        false
    }
}

/// Synchronous pull helper used by tests: read all of `data` through a pipe
/// pair the way the puller threads do.
#[cfg(test)]
fn pull_through_pipe(data: &[u8]) -> Option<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel::<Option<Vec<u8>>>();
    let mut fds = [0; 2];
    assert_eq!(
        unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
        0
    );
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let mut write = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    thread::spawn(move || {
        tx.send(read_pipe(read)).unwrap();
    });
    write.write_all(data).unwrap();
    drop(write);
    rx.recv().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_parsing() {
        assert!(matches!(
            serde_json::from_str::<Request>(r#"{"cmd":"watch"}"#).unwrap(),
            Request::Watch
        ));
        assert!(matches!(
            serde_json::from_str::<Request>(r#"{"cmd":"cancel_drag"}"#).unwrap(),
            Request::CancelDrag
        ));
        match serde_json::from_str::<Request>(
            r#"{"cmd":"get_data","id":7,"mime":"text/plain"}"#,
        )
        .unwrap()
        {
            Request::GetData { id, mime } => {
                assert_eq!(id, 7);
                assert_eq!(mime, "text/plain");
            }
            _ => panic!("wrong variant"),
        }
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"bogus"}"#).is_err());
    }

    #[test]
    fn response_framing_is_single_line_json() {
        let line = serde_json::to_string(&Response::DragStarted {
            gen: 3,
            mimes: &["text/plain".to_string()],
            actions: &["copy".to_string()],
        })
        .unwrap();
        assert!(!line.contains('\n'));
        let back: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(back["event"], "drag_started");
        assert_eq!(back["gen"], 3);
    }

    #[test]
    fn pipe_roundtrip() {
        let data = b"hello drag".to_vec();
        assert_eq!(pull_through_pipe(&data).unwrap(), data);
    }

    #[test]
    fn pipe_timeout_yields_none() {
        // Write end held open with no data and no EOF: must time out, not hang.
        let mut fds = [0; 2];
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
            0
        );
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let _write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        assert_eq!(read_pipe_with_timeout(read, 50), None);
    }

    #[test]
    fn pipe_empty_payload_is_some() {
        // Immediate EOF with no bytes is a (degenerate) successful pull.
        assert_eq!(pull_through_pipe(b""), Some(Vec::new()));
    }

    #[test]
    fn serve_conn_parses_binary_frame() {
        use std::os::unix::net::UnixStream;
        let (mut client, server) = UnixStream::pair().unwrap();
        let (tx, rx) = calloop::channel::channel::<DndIpcMsg>();
        let handle = thread::spawn(move || serve_conn(server, tx));
        client
            .write_all(b"{\"cmd\":\"external_drag_data\",\"mime\":\"text/plain\",\"len\":5}\nhello")
            .unwrap();
        client.write_all(b"{\"cmd\":\"ping\"}\n").unwrap();
        drop(client);
        handle.join().unwrap();
        match rx.try_recv().unwrap() {
            DndIpcMsg::ExternalData { mime, bytes } => {
                assert_eq!(mime, "text/plain");
                assert_eq!(bytes, b"hello");
            }
            other => panic!("wrong message: {other:?}"),
        }
        assert!(matches!(rx.try_recv().unwrap(), DndIpcMsg::Ping(_)));
    }

    #[test]
    fn viewer_source_metadata_and_send() {
        let (tx, _rx) = calloop::channel::channel::<DndIpcMsg>();
        let source = ViewerDndSource {
            mimes: vec!["text/plain".to_string()],
            shared: Arc::new(Mutex::new(ExternalShared::default())),
            tx,
        };
        let meta = source.metadata().unwrap();
        assert_eq!(meta.mime_types, vec!["text/plain".to_string()]);

        // Cache hit: bytes written to the fd.
        source
            .shared
            .lock()
            .unwrap()
            .cache
            .insert("text/plain".to_string(), b"hi".to_vec());
        let mut fds = [0; 2];
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        source.send("text/plain", write);
        let mut out = Vec::new();
        let mut file = std::fs::File::from(read);
        file.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"hi");

        // Cache miss: queued as pending (fd stays open).
        let mut fds = [0; 2];
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        source.send("text/html", write);
        assert_eq!(source.shared.lock().unwrap().pending.len(), 1);
        drop(read);
    }

    #[test]
    fn action_names() {
        assert_eq!(action_name(DndAction::Copy), "copy");
        assert_eq!(action_name(DndAction::Move), "move");
        assert_eq!(action_name(DndAction::Ask), "ask");
        assert_eq!(action_name(DndAction::None), "none");
    }
}
