use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use gbm::{BufferObject, BufferObjectFlags, Device, Format, Modifier};

mod cursor;

use wayland_client::event_created_child;
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_callback::WlCallback,
    wl_compositor::WlCompositor,
    wl_data_device::{self, WlDataDevice},
    wl_data_device_manager::{DndAction, WlDataDeviceManager},
    wl_data_offer::{self, WlDataOffer},
    wl_data_source::{self, WlDataSource},
    wl_keyboard::{self, KeyState, WlKeyboard},
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_output::{self, WlOutput},
    wl_pointer::{self, ButtonState, WlPointer},
    wl_registry::{self, WlRegistry},
    wl_seat::{self, WlSeat},
    wl_surface::WlSurface,
    wl_touch::{self, WlTouch},
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};

use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
    zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_feedback_v1::{self, ZwpLinuxDmabufFeedbackV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport,
    wp_viewporter::WpViewporter,
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

#[derive(Parser, Debug)]
#[command(name = "nori-view", about = "High-Performance Zero-Copy DMA-BUF Wayland Bridge")]
struct Args {
    /// Slot number (e.g. 1 for wayland-nori-1)
    #[arg(short, long)]
    slot: Option<u32>,

    /// Target Wayland display socket name or path
    #[arg(short, long)]
    target: Option<String>,

    /// Host Wayland display socket name or path (defaults to WAYLAND_DISPLAY)
    #[arg(long)]
    host: Option<String>,

    /// Target output name to capture (e.g. headless-1)
    #[arg(short, long, default_value = "headless-1")]
    output: String,

    /// Window title on host
    #[arg(long)]
    title: Option<String>,

    /// Application ID on host
    #[arg(long)]
    app_id: Option<String>,

    /// Fullscreen mode on host (enabled by default)
    #[arg(short, long, default_value_t = true)]
    fullscreen: bool,

    /// Disable fullscreen mode on host (run windowed)
    #[arg(long)]
    windowed: bool,

    /// DRM render node path
    #[arg(long, default_value = "/dev/dri/renderD128")]
    drm_node: PathBuf,

    /// Head number shown as the cursor numeral (defaults to --slot)
    #[arg(long)]
    head_num: Option<u32>,

    /// Node name selecting the cursor accent color (defaults to "local")
    #[arg(long)]
    node: Option<String>,

    /// Cursor accent override: palette name or #rrggbb (defaults by node)
    #[arg(long)]
    accent: Option<String>,
}

struct DmaBuffer {
    _bo: BufferObject<()>,
    target_buffer: WlBuffer,
    host_buffer: WlBuffer,
}

// ---------------------------------------------------------------------------
// Host State & Handlers
// ---------------------------------------------------------------------------

struct HostState {
    compositor: Option<WlCompositor>,
    xdg_wm_base: Option<XdgWmBase>,
    linux_dmabuf: Option<ZwpLinuxDmabufV1>,
    dmabuf_feedback: FeedbackCollector,
    dmabuf_feedback_obj: Option<ZwpLinuxDmabufFeedbackV1>,
    seat: Option<WlSeat>,
    inhibit_manager: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
    viewporter: Option<WpViewporter>,
    viewport: Option<WpViewport>,

    surface: Option<WlSurface>,
    xdg_surface: Option<XdgSurface>,
    toplevel: Option<XdgToplevel>,

    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    touch: Option<WlTouch>,

    configured: bool,
    width: u32,
    height: u32,

    pointer_events: Vec<PointerEvent>,
    keyboard_events: Vec<KeyboardEvent>,
    pending_axis_source: wl_pointer::AxisSource,
    last_axis_value: [f64; 2],
    held_keys: HashSet<u32>,
    inhibitor_active: bool,
    mod_held: bool,
    shift_held: bool,
    buffer_busy: Vec<bool>,
    frame_done: bool,
    is_fullscreen: bool,
    is_focused: bool,
    last_unfocused_capture: Option<std::time::Instant>,
    unfocused_since: Option<std::time::Instant>,
    capture_halted: bool,
    closed: bool,
    dnd: DndBridge,
}

impl HostState {
    /// Turn a slot drag that left the viewer surface into a host-side drag.
    ///
    /// Called every frame while `escape_wanted` (set on pointer Leave). Waits
    /// for cached payload data, then starts the host drag with the remembered
    /// press serial and cancels the slot drag so the gesture lands once.
    fn dnd_maybe_escape(&mut self, qh: &QueueHandle<Self>) {
        if !self.dnd.escape_wanted {
            return;
        }
        let Some(drag) = &self.dnd.drag else {
            self.dnd.escape_wanted = false;
            return;
        };
        if drag.escaped || self.dnd.pointer_inside || !self.dnd.button_held {
            // Re-entered, released, or already escaped: nothing to do. A fresh
            // Leave re-arms if the drag is still alive.
            self.dnd.escape_wanted = false;
            return;
        }
        if drag.cache.is_empty() {
            // Payloads still in flight; retry over the next frames.
            return;
        }
        let (Some(serial), Some(dd), Some(mgr), Some(surface)) = (
            self.dnd.last_press_serial,
            self.dnd.dd.clone(),
            self.dnd.dd_manager.clone(),
            self.surface.clone(),
        ) else {
            println!("[nori-view] DnD escape impossible (no serial/device/surface)");
            self.dnd.escape_wanted = false;
            return;
        };
        let mimes: Vec<String> = self
            .dnd
            .drag
            .as_ref()
            .map(|drag| drag.cache.keys().cloned().collect())
            .unwrap_or_default();

        let source = mgr.create_data_source(qh, ());
        for mime in &mimes {
            source.offer(mime.clone());
        }
        // copy|move; v1 always copies (the slot source sees cancel).
        source.set_actions(DndAction::Copy | DndAction::Move);
        let icon = self.dnd_create_icon(qh);
        dd.start_drag(
            Some(&source),
            &surface,
            icon.as_ref().map(|icon| &icon.surface),
            serial,
        );
        println!("[nori-view] DnD escaped to host with mimes {mimes:?}");

        self.dnd.icon = icon;
        self.dnd.source = Some(source);
        self.dnd.host_drag_active = true;
        self.dnd.escape_wanted = false;
        if let Some(drag) = self.dnd.drag.as_mut() {
            drag.escaped = true;
        }
        self.dnd
            .send_cmd(&serde_json::json!({"cmd": "cancel_drag"}).to_string());
    }

    /// File classes from the cached URI-list payload, if any.
    fn drag_files(&self) -> (Vec<cursor::FileClass>, usize) {
        self.dnd
            .drag
            .as_ref()
            .and_then(|drag| drag.cache.get("text/uri-list"))
            .and_then(|uris| std::str::from_utf8(uris).ok())
            .map(cursor::classify_uris)
            .unwrap_or_default()
    }

    /// Spec for the viewer cursor right now: idle badged arrow, or drag/file
    /// composition while a live unescaped drag sits under a held button.
    fn cursor_spec(&self) -> cursor::CursorSpec {
        let mut spec = cursor::CursorSpec::idle(self.dnd.head_num, self.dnd.accent);
        let active = self.dnd.pointer_inside
            && self.dnd.button_held
            && self.dnd.drag.as_ref().is_some_and(|d| !d.escaped);
        if !active {
            return spec;
        }
        let (files, count) = self.drag_files();
        if count > 0 {
            spec.files = files;
            spec.file_count = count;
        }
        spec.drag = Some(cursor::DragBadge::Neutral);
        spec
    }

    /// Spec for the escape icon: file glyphs from the cache plus the live
    /// host acceptance state.
    fn icon_spec(&self, accepted: bool) -> cursor::CursorSpec {
        let mut spec = cursor::CursorSpec::idle(self.dnd.head_num, self.dnd.accent);
        let (files, count) = self.drag_files();
        spec.files = files;
        spec.file_count = count;
        spec.drag = Some(if accepted {
            cursor::DragBadge::Accepted
        } else {
            cursor::DragBadge::Neutral
        });
        spec
    }

    /// Redraw the viewer cursor when its spec changed (cheap no-op otherwise).
    fn sync_cursor(&mut self) {
        let spec = self.cursor_spec();
        // Bounded trace: only while a slot drag is live.
        if let Some(drag) = self.dnd.drag.as_ref() {
            let changed = self
                .dnd
                .cursor
                .as_ref()
                .map(|c| c.spec != spec)
                .unwrap_or(false);
            println!(
                "[nori-view] DnD cursor sync: inside={} held={} escaped={} files={:?} badge={:?} redraw={}",
                self.dnd.pointer_inside,
                self.dnd.button_held,
                drag.escaped,
                spec.files,
                spec.drag,
                changed,
            );
        }
        if let Some(cursor) = self.dnd.cursor.as_mut() {
            cursor.redraw(spec);
        }
    }

    /// Build the host drag icon from the current drag cache. None without shm
    /// (drag works, just without an icon).
    fn dnd_create_icon(&mut self, qh: &QueueHandle<Self>) -> Option<CursorSurface> {
        let (shm, compositor) = (self.dnd.shm.clone()?, self.compositor.clone()?);
        let spec = self.icon_spec(false);
        CursorSurface::create(&compositor, &shm, qh, "icon", spec)
    }

    fn dnd_destroy_icon(&mut self) {
        if let Some(icon) = self.dnd.icon.take() {
            icon.destroy();
        }
    }

    fn release_all_held_keys(&mut self) {
        if !self.held_keys.is_empty() {
            println!("[nori-view] Releasing all {} held keys on slot", self.held_keys.len());
            for key in self.held_keys.drain() {
                self.keyboard_events.push(KeyboardEvent::Key {
                    time: 0,
                    key,
                    state: 0,
                });
            }
        }
        self.mod_held = false;
        self.shift_held = false;
    }
}

#[derive(Clone, Debug)]
enum PointerEvent {
    MotionAbsolute { time: u32, x: f64, y: f64 },
    Button { time: u32, button: u32, state: ButtonState },
    Axis {
        time: u32,
        axis: wl_pointer::Axis,
        value: f64,
        source: wl_pointer::AxisSource,
    },
    AxisDiscrete {
        axis: wl_pointer::Axis,
        value: f64,
        discrete: i32,
    },
    AxisStop { time: u32, axis: wl_pointer::Axis },
    Frame,
}

#[derive(Clone, Debug)]
enum KeyboardEvent {
    Key { time: u32, key: u32, state: u32 },
    Modifiers { depressed: u32, latched: u32, locked: u32, group: u32 },
}

// ---------------------------------------------------------------------------
// DnD bridge: slot drags escape to the host exactly once.
//
// The slot notifies us of drags over `nori-dnd-{slot}.sock` and serves eagerly
// copied payloads there. While the pointer stays inside the viewer the slot
// drag proceeds internally and we do nothing. When the host pointer leaves the
// viewer surface mid-drag, we start a host-side drag (reusing the remembered
// button-press serial), serve its data from our cache, and tell the slot to
// cancel its drag -- so the gesture completes exactly once, on the host.
// ---------------------------------------------------------------------------

const BTN_LEFT: u32 = 0x110;

#[derive(Debug, PartialEq)]
enum DndSockEvent {
    Watching {
        gen: u64,
    },
    DragStarted {
        gen: u64,
        mimes: Vec<String>,
    },
    DragEnded {
        gen: u64,
    },
    DataReady {
        gen: u64,
        mime: String,
        ok: bool,
    },
    Data {
        id: u64,
        mime: String,
        bytes: Vec<u8>,
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
        mime: String,
    },
    Pong,
}

fn json_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

fn json_u64(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(|v| v.as_u64()).unwrap_or_default()
}

/// Incremental parser for the slot socket's line + raw-byte framing.
#[derive(Default)]
struct DndSockParser {
    buf: Vec<u8>,
    pending_data: Option<(u64, String, usize)>,
}

impl DndSockParser {
    fn push(&mut self, chunk: &[u8]) -> Vec<DndSockEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            if let Some((id, mime, len)) = self.pending_data.take() {
                if self.buf.len() < len {
                    self.pending_data = Some((id, mime, len));
                    break;
                }
                let bytes = self.buf.drain(..len).collect::<Vec<u8>>();
                out.push(DndSockEvent::Data { id, mime, bytes });
                continue;
            }
            let Some(nl) = self.buf.iter().position(|b| *b == b'\n') else {
                break;
            };
            let line = self.buf.drain(..=nl).collect::<Vec<u8>>();
            let Ok(text) = std::str::from_utf8(&line) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
                continue;
            };
            match v.get("event").and_then(|e| e.as_str()).unwrap_or_default() {
                "watching" => out.push(DndSockEvent::Watching {
                    gen: json_u64(&v, "gen"),
                }),
                "drag_started" => {
                    let mimes = v
                        .get("mimes")
                        .and_then(|m| m.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|m| m.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    out.push(DndSockEvent::DragStarted {
                        gen: json_u64(&v, "gen"),
                        mimes,
                    });
                }
                "drag_ended" => out.push(DndSockEvent::DragEnded {
                    gen: json_u64(&v, "gen"),
                }),
                "data_ready" => out.push(DndSockEvent::DataReady {
                    gen: json_u64(&v, "gen"),
                    mime: json_str(&v, "mime"),
                    ok: v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false),
                }),
                "data" => {
                    self.pending_data = Some((
                        json_u64(&v, "id"),
                        json_str(&v, "mime"),
                        v.get("len").and_then(|l| l.as_u64()).unwrap_or_default() as usize,
                    ));
                }
                "nodata" => out.push(DndSockEvent::NoData {
                    id: json_u64(&v, "id"),
                    pending: v.get("pending").and_then(|p| p.as_bool()).unwrap_or(false),
                }),
                "drag_cancelled" => out.push(DndSockEvent::DragCancelled {
                    ok: v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false),
                }),
                "external_started" => out.push(DndSockEvent::ExternalStarted {
                    ok: v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false),
                }),
                "need_data" => out.push(DndSockEvent::NeedData {
                    mime: json_str(&v, "mime"),
                }),
                "pong" => out.push(DndSockEvent::Pong),
                _ => {}
            }
        }
        out
    }
}

#[derive(Default)]
struct SlotDrag {
    gen: u64,
    cache: HashMap<String, Vec<u8>>,
    awaiting: HashSet<String>,
    escaped: bool,
}

#[derive(Default)]
struct DndBridge {
    sock: Option<UnixStream>,
    /// Slot socket path, kept so a dead connection can be re-established.
    sock_path: Option<PathBuf>,
    /// Frames until the next reconnect attempt (0 = try now).
    retry_cooldown: u32,
    /// Locked writer clone: host-receive threads push slot-bound data here.
    writer: Option<Arc<Mutex<UnixStream>>>,
    /// Host payloads by MIME (`None` = receive failed), shared with threads.
    host_cache: Option<Arc<Mutex<HashMap<String, Option<Vec<u8>>>>>>,
    parser: DndSockParser,
    next_id: u64,
    outstanding: HashMap<u64, String>,
    drag: Option<SlotDrag>,
    escape_wanted: bool,
    pointer_inside: bool,
    button_held: bool,
    last_press_serial: Option<u32>,
    dd_manager: Option<WlDataDeviceManager>,
    dd: Option<WlDataDevice>,
    shm: Option<WlShm>,
    last_enter_serial: Option<u32>,
    head_num: u32,
    accent: [u8; 4],
    cursor: Option<CursorSurface>,
    source: Option<WlDataSource>,
    icon: Option<CursorSurface>,
    incoming_offer: Option<WlDataOffer>,
    incoming_mimes: Vec<String>,
    host_drag_active: bool,
    external_injected: bool,
}

/// Write through a non-blocking socket, waiting for POLLOUT when full.
fn write_all_nonblocking(sock: &UnixStream, mut buf: &[u8]) {
    let mut s = sock;
    while !buf.is_empty() {
        match s.write(buf) {
            Ok(0) => break,
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let mut pfd =
                    [nix::poll::PollFd::new(sock.as_fd(), nix::poll::PollFlags::POLLOUT)];
                let _ = nix::poll::poll(&mut pfd, nix::poll::PollTimeout::from(100u16));
            }
            Err(_) => break,
        }
    }
}

impl DndBridge {
    /// Main-thread commands go over the reader connection, because replies
    /// (watching/data/nodata/external_started) arrive on the same connection
    /// the request used -- and only the reader is pumped.
    fn send_cmd(&self, body: &str) {
        if let Some(sock) = &self.sock {
            let mut line = body.to_string();
            line.push('\n');
            write_all_nonblocking(sock, line.as_bytes());
        }
    }

    /// Push one host payload to the slot (header line + raw bytes).
    fn send_data_push(&self, mime: &str, bytes: &[u8]) {
        if let Some(sock) = &self.sock {
            let header =
                serde_json::json!({"cmd": "external_drag_data", "mime": mime, "len": bytes.len()})
                    .to_string()
                + "\n";
            write_all_nonblocking(sock, header.as_bytes());
            write_all_nonblocking(sock, bytes);
        }
    }

    fn request_data(&mut self, mime: &str) {
        self.next_id += 1;
        let id = self.next_id;
        self.outstanding.insert(id, mime.to_string());
        let cmd = serde_json::json!({"cmd": "get_data", "id": id, "mime": mime}).to_string();
        self.send_cmd(&cmd);
    }

    /// Drain the slot socket (non-blocking) and handle framing/events.
    /// A dead connection is re-established in the background: the slot
    /// replays any active drag on `watch`, so the bridge self-heals across
    /// slot restarts and transient I/O errors.
    fn pump(&mut self) {
        if self.sock.is_none() {
            if self.sock_path.is_none() {
                return;
            }
            if self.retry_cooldown > 0 {
                self.retry_cooldown -= 1;
                return;
            }
            self.retry_cooldown = 600;
            self.reconnect();
            return;
        }
        let mut dead = false;
        let mut events = Vec::new();
        let mut tmp = [0u8; 65536];
        loop {
            let read_result = self.sock.as_ref().unwrap().read(&mut tmp);
            match read_result {
                Ok(0) => {
                    dead = true;
                    break;
                }
                Ok(n) => events.extend(self.parser.push(&tmp[..n])),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                // Interrupted reads are transient (signals under load): retry
                // rather than condemning a healthy connection.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    dead = true;
                    break;
                }
            }
        }
        if dead {
            println!("[nori-view] DnD socket lost; will reconnect");
            self.sock = None;
            self.writer = None;
            self.drag = None;
            self.retry_cooldown = 0;
        }
        for event in events {
            self.handle_sock_event(event);
        }
    }

    /// Re-establish the slot connection after a loss. The slot replays any
    /// active drag on `watch`, so mid-gesture reconnects still see the drag.
    fn reconnect(&mut self) {
        let Some(path) = self.sock_path.clone() else {
            return;
        };
        let reader = UnixStream::connect(&path).ok().map(|sock| {
            let _ = sock.set_nonblocking(true);
            sock
        });
        let writer = UnixStream::connect(&path)
            .ok()
            .map(|s| Arc::new(Mutex::new(s)));
        if reader.is_none() || writer.is_none() {
            return;
        }
        self.sock = reader;
        self.writer = writer;
        if self.host_cache.is_none() {
            self.host_cache = Some(Arc::new(Mutex::new(HashMap::new())));
        }
        self.send_cmd(&serde_json::json!({"cmd": "watch"}).to_string());
        println!("[nori-view] DnD reconnected to {}", path.display());
    }

    fn handle_sock_event(&mut self, event: DndSockEvent) {
        match event {
            DndSockEvent::Watching { gen } => {
                println!("[nori-view] DnD watching slot (gen {gen})");
            }
            DndSockEvent::DragStarted { gen, mimes } => {
                println!("[nori-view] DnD slot drag started: gen={gen} mimes={mimes:?}");
                let mut drag = SlotDrag {
                    gen,
                    ..SlotDrag::default()
                };
                for mime in &mimes {
                    drag.awaiting.insert(mime.clone());
                    self.request_data(mime);
                }
                self.drag = Some(drag);
                self.escape_wanted = false;
            }
            DndSockEvent::DataReady { gen, mime, ok } => {
                let wanted = self
                    .drag
                    .as_ref()
                    .is_some_and(|drag| drag.gen == gen && drag.awaiting.contains(&mime));
                if wanted {
                    if ok {
                        self.request_data(&mime);
                    } else if let Some(drag) = self.drag.as_mut() {
                        drag.awaiting.remove(&mime);
                    }
                }
            }
            DndSockEvent::Data { id, mime, bytes } => {
                self.outstanding.remove(&id);
                if let Some(drag) = self.drag.as_mut() {
                    if drag.awaiting.remove(&mime) {
                        println!(
                            "[nori-view] DnD cached {mime}: {} bytes",
                            bytes.len()
                        );
                        drag.cache.insert(mime, bytes);
                    }
                }
            }
            DndSockEvent::NoData { id, pending } => {
                if let Some(mime) = self.outstanding.remove(&id) {
                    if !pending {
                        if let Some(drag) = self.drag.as_mut() {
                            drag.awaiting.remove(&mime);
                        }
                    }
                }
            }
            DndSockEvent::DragEnded { gen } => {
                if self.drag.as_ref().is_some_and(|drag| drag.gen == gen) {
                    println!("[nori-view] DnD slot drag ended (gen {gen})");
                    self.drag = None;
                }
                self.escape_wanted = false;
            }
            DndSockEvent::DragCancelled { ok } => {
                println!("[nori-view] DnD slot cancel confirmed: ok={ok}");
            }
            DndSockEvent::ExternalStarted { ok } => {
                println!("[nori-view] DnD slot injection confirmed: ok={ok}");
                self.external_injected = ok;
            }
            DndSockEvent::NeedData { mime } => {
                // The slot asks (again) for a payload. Serve from the host
                // cache when present; the receive threads push on arrival
                // otherwise, and the slot queues the send meanwhile.
                let cached = self
                    .host_cache
                    .as_ref()
                    .and_then(|cache| cache.lock().ok())
                    .and_then(|cache| cache.get(&mime).cloned());
                match cached {
                    Some(Some(bytes)) => self.send_data_push(&mime, &bytes),
                    Some(None) => self.send_cmd(
                        &serde_json::json!({"cmd": "external_drag_failed", "mime": mime})
                            .to_string(),
                    ),
                    None => {}
                }
            }
            DndSockEvent::Pong => {}
        }
    }
}

fn parse_slot_from_display(display: &str) -> Option<u32> {
    display.rsplit('/').next()?.rsplit('-').next()?.parse().ok()
}


/// One shm-backed cursor surface and its pixels. Used both for the viewer
/// cursor (badged per head) and the drag icon (file glyphs + accept state).
struct CursorSurface {
    surface: WlSurface,
    buffer: WlBuffer,
    pool: WlShmPool,
    file: File,
    spec: cursor::CursorSpec,
}

impl CursorSurface {
    fn create(
        compositor: &WlCompositor,
        shm: &WlShm,
        qh: &QueueHandle<HostState>,
        tag: &str,
        spec: cursor::CursorSpec,
    ) -> Option<Self> {
        let run = std::env::var("XDG_RUNTIME_DIR").ok()?;
        let path = format!("{run}/nori-cursor-{tag}-{}.bin", std::process::id());
        let pixels = cursor::compose_cursor(&spec);
        // NOTE: read+write is mandatory. The compositor mmaps this fd
        // PROT_READ|PROT_WRITE; a write-only fd fails mapping and the
        // compositor kills us with a wl_shm protocol error.
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .ok()?;
        file.write_all(&pixels).ok()?;
        let _ = std::fs::remove_file(&path);
        let pool = shm.create_pool(file.as_fd(), pixels.len() as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            cursor::ICON_SIZE,
            cursor::ICON_SIZE,
            cursor::ICON_STRIDE,
            wl_shm::Format::Argb8888,
            qh,
            usize::MAX,
        );
        let surface = compositor.create_surface(qh, ());
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, cursor::ICON_SIZE, cursor::ICON_SIZE);
        surface.commit();
        Some(Self {
            surface,
            buffer,
            pool,
            file,
            spec,
        })
    }

    /// Redraw only when the spec changed. Re-attaches the buffer on every
    /// redraw: damage-only commits on an already-attached shm buffer are
    /// silently ignored for cursor surfaces on some compositors (observed:
    /// niri never re-reads without a fresh attach), leaving a stale image.
    fn redraw(&mut self, spec: cursor::CursorSpec) {
        if self.spec == spec {
            return;
        }
        self.spec = spec;
        use std::io::Seek;
        let pixels = cursor::compose_cursor(&self.spec);
        if self.file.seek(std::io::SeekFrom::Start(0)).is_ok() {
            let _ = self.file.write_all(&pixels);
        }
        self.surface.attach(Some(&self.buffer), 0, 0);
        self.surface.damage_buffer(0, 0, cursor::ICON_SIZE, cursor::ICON_SIZE);
        self.surface.commit();
    }

    fn destroy(self) {
        self.surface.destroy();
        self.buffer.destroy();
        self.pool.destroy();
    }
}

impl Dispatch<WlRegistry, ()> for HostState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(6), qh, ()));
                }
                "xdg_wm_base" => {
                    state.xdg_wm_base = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "zwp_linux_dmabuf_v1" => {
                    state.linux_dmabuf = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "wl_seat" => {
                    state.seat = Some(registry.bind(name, version.min(7), qh, ()));
                }
                "zwp_keyboard_shortcuts_inhibit_manager_v1" => {
                    state.inhibit_manager = Some(registry.bind(name, 1, qh, ()));
                }
                "wp_viewporter" => {
                    state.viewporter = Some(registry.bind(name, 1, qh, ()));
                }
                "wl_data_device_manager" => {
                    state.dnd.dd_manager = Some(registry.bind(name, version.min(3), qh, ()));
                }
                "wl_shm" => {
                    state.dnd.shm = Some(registry.bind(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlCompositor, ()> for HostState {
    fn event(_: &mut Self, _: &WlCompositor, _: wayland_client::protocol::wl_compositor::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<XdgWmBase, ()> for HostState {
    fn event(_: &mut Self, wm_base: &XdgWmBase, event: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for HostState {
    fn event(state: &mut Self, surface: &XdgSurface, event: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            if let Some(vp) = &state.viewport {
                vp.set_destination(state.width as i32, state.height as i32);
            }
            state.configured = true;
        }
    }
}

impl Dispatch<XdgToplevel, ()> for HostState {
    fn event(state: &mut Self, _: &XdgToplevel, event: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            xdg_toplevel::Event::Configure { width, height, states } => {
                let activated = states.chunks_exact(4).any(|chunk| {
                    u32::from_ne_bytes(chunk.try_into().unwrap()) == wayland_protocols::xdg::shell::client::xdg_toplevel::State::Activated as u32
                });
                if state.is_focused != activated {
                    println!("[nori-view] Focus transition: is_focused={activated}");
                    state.is_focused = activated;
                    if activated {
                        state.unfocused_since = None;
                    } else {
                        state.unfocused_since = Some(std::time::Instant::now());
                    }
                }
                if width > 0 && height > 0 {
                    state.width = width as u32;
                    state.height = height as u32;
                    if let Some(vp) = &state.viewport {
                        vp.set_destination(width, height);
                    }
                    if let Some(surface) = &state.surface {
                        surface.commit();
                    }
                }
            }
            xdg_toplevel::Event::Close => {
                state.closed = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSurface, ()> for HostState {
    fn event(_: &mut Self, _: &WlSurface, _: wayland_client::protocol::wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlSeat, ()> for HostState {
    fn event(state: &mut Self, seat: &WlSeat, event: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            if let WEnum::Value(caps) = capabilities {
                if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                    state.pointer = Some(seat.get_pointer(qh, ()));
                }
                if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                    state.keyboard = Some(seat.get_keyboard(qh, ()));
                }
                if caps.contains(wl_seat::Capability::Touch) && state.touch.is_none() {
                    state.touch = Some(seat.get_touch(qh, ()));
                }
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for HostState {
    fn event(state: &mut Self, pointer: &WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            // NOTE: do NOT hide the host cursor here (set_cursor None) until
            // the slot renders its own cursor into screencopy frames. The
            // headless slot currently draws no cursor, so hiding the host
            // cursor leaves the viewer with no pointer at all (2026-10-19).
            wl_pointer::Event::Enter { serial, .. } => {
                state.dnd.pointer_inside = true;
                state.dnd.escape_wanted = false;
                state.dnd.last_enter_serial = Some(serial);
                // Apply the badged head cursor (arrow tip at the pointer).
                // Leaving the surface restores the compositor default.
                if let Some(cursor) = &state.dnd.cursor {
                    pointer.set_cursor(serial, Some(&cursor.surface), 6, 4);
                }
            }
            wl_pointer::Event::Leave { .. } => {
                // Leaving mid-drag is the escape signal: the main loop turns
                // it into a host-side drag once payload data is cached.
                state.dnd.pointer_inside = false;
                state.dnd.escape_wanted = true;
            }
            wl_pointer::Event::Motion { time, surface_x, surface_y } => {
                state.pointer_events.push(PointerEvent::MotionAbsolute {
                    time,
                    x: surface_x,
                    y: surface_y,
                });
            }
            wl_pointer::Event::Button { serial, time, button, state: bstate } => {
                if let WEnum::Value(bstate) = bstate {
                    // Track the press serial and button state for DnD escape:
                    // start_drag needs the serial of the press that began the
                    // gesture, and escape is only valid while held.
                    if button == BTN_LEFT {
                        if bstate == ButtonState::Pressed {
                            state.dnd.button_held = true;
                            state.dnd.last_press_serial = Some(serial);
                        } else {
                            state.dnd.button_held = false;
                            state.dnd.escape_wanted = false;
                        }
                    }
                    state.pointer_events.push(PointerEvent::Button {
                        time,
                        button,
                        state: bstate,
                    });
                }
            }
            wl_pointer::Event::AxisSource { axis_source } => {
                if let WEnum::Value(source) = axis_source {
                    state.pending_axis_source = source;
                }
            }
            wl_pointer::Event::Axis { time, axis, value } => {
                if let WEnum::Value(axis) = axis {
                    let idx = usize::from(matches!(axis, wl_pointer::Axis::HorizontalScroll));
                    state.last_axis_value[idx] = value;
                    state.pointer_events.push(PointerEvent::Axis {
                        time,
                        axis,
                        value,
                        source: state.pending_axis_source.clone(),
                    });
                }
            }
            wl_pointer::Event::AxisDiscrete { axis, discrete } => {
                if let WEnum::Value(axis) = axis {
                    let idx = usize::from(matches!(axis, wl_pointer::Axis::HorizontalScroll));
                    let value = state.last_axis_value[idx];
                    state.pointer_events.push(PointerEvent::AxisDiscrete {
                        axis,
                        value,
                        discrete,
                    });
                }
            }
            wl_pointer::Event::AxisStop { time, axis } => {
                if let WEnum::Value(axis) = axis {
                    state
                        .pointer_events
                        .push(PointerEvent::AxisStop { time, axis });
                }
            }
            wl_pointer::Event::Frame => {
                state.pointer_events.push(PointerEvent::Frame);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for HostState {
    fn event(state: &mut Self, _: &WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _qh: &QueueHandle<Self>) {
        match event {
            wl_keyboard::Event::Key { time, key, state: kstate, .. } => {
                if let WEnum::Value(kstate) = kstate {
                    // Key repeat is compositor-generated from the initial press
                    // and the slot repeats from the press on its own, so forward
                    // nothing here -- and never mistake a repeat for a release.
                    if kstate == KeyState::Repeated {
                        return;
                    }
                    let is_pressed = kstate == KeyState::Pressed;
                    let state_val = if is_pressed { 1 } else { 0 };

                    if is_pressed {
                        state.held_keys.insert(key);
                    } else {
                        state.held_keys.remove(&key);
                    }

                    // Direct keycode tracking for Super (125/126) and Shift (42/54)
                    if key == 125 || key == 126 {
                        state.mod_held = is_pressed;
                    } else if key == 42 || key == 54 {
                        state.shift_held = is_pressed;
                    }

                    // Mod+Shift+F is ONLY handled on host if shortcuts are NOT captive (!state.inhibitor_active).
                    // While captive in slot, all keys (including Mod+Shift+F) are forwarded straight to Head 1!
                    // To un-fullscreen host, user must take the path: Mod+Esc (unlock) -> Mod+Shift+F (un-fullscreen).
                    if !state.inhibitor_active {
                        if (((key == 33 || key == 41) && state.mod_held && state.shift_held) || key == 87) && is_pressed {
                            state.release_all_held_keys();
                            state.is_fullscreen = !state.is_fullscreen;
                            if let Some(toplevel) = &state.toplevel {
                                if state.is_fullscreen {
                                    toplevel.set_fullscreen(None);
                                    println!("[nori-view] Mod+Shift+F: Toggled Fullscreen ON (host navigation mode)");
                                } else {
                                    toplevel.unset_fullscreen();
                                    println!("[nori-view] Mod+Shift+F: Toggled Fullscreen OFF (host navigation mode)");
                                }
                                if let Some(surface) = &state.surface {
                                    surface.commit();
                                }
                            }
                            return;
                        }
                    }

                    // Forward key to target virtual keyboard
                    state.keyboard_events.push(KeyboardEvent::Key {
                        time,
                        key,
                        state: state_val,
                    });
                }
            }
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                state.shift_held = (mods_depressed & 0x01) != 0;
                state.mod_held = (mods_depressed & 0x40) != 0 || (mods_depressed & 0x08) != 0;

                // Auto-unstick modifiers: if host reports a modifier is no longer depressed,
                // immediately emit a release event for it to target slot so it never gets stuck.
                let mut stuck_keys = Vec::new();
                if (mods_depressed & 0x01) == 0 {
                    if state.held_keys.remove(&42) { stuck_keys.push(42); }
                    if state.held_keys.remove(&54) { stuck_keys.push(54); }
                }
                if (mods_depressed & 0x04) == 0 {
                    if state.held_keys.remove(&29) { stuck_keys.push(29); }
                    if state.held_keys.remove(&97) { stuck_keys.push(97); }
                }
                if (mods_depressed & 0x08) == 0 {
                    if state.held_keys.remove(&56) { stuck_keys.push(56); }
                    if state.held_keys.remove(&100) { stuck_keys.push(100); }
                }
                if (mods_depressed & 0x40) == 0 {
                    if state.held_keys.remove(&125) { stuck_keys.push(125); }
                    if state.held_keys.remove(&126) { stuck_keys.push(126); }
                }

                for key in stuck_keys {
                    println!("[nori-view] Auto-unsticking modifier key {}", key);
                    state.keyboard_events.push(KeyboardEvent::Key {
                        time: 0,
                        key,
                        state: 0,
                    });
                }

                state.keyboard_events.push(KeyboardEvent::Modifiers {
                    depressed: mods_depressed,
                    latched: mods_latched,
                    locked: mods_locked,
                    group,
                });
            }
            wl_keyboard::Event::Leave { .. } => {
                println!("[nori-view] Focus Leave: releasing all held keys");
                state.release_all_held_keys();
            }
            _ => {}
        }
    }
}

impl Dispatch<WlTouch, ()> for HostState {
    fn event(state: &mut Self, _: &WlTouch, event: wl_touch::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_touch::Event::Down { time, x, y, .. } => {
                state.pointer_events.push(PointerEvent::MotionAbsolute { time, x, y });
                state.pointer_events.push(PointerEvent::Button {
                    time,
                    button: 0x110, // BTN_LEFT
                    state: ButtonState::Pressed,
                });
                state.pointer_events.push(PointerEvent::Frame);
            }
            wl_touch::Event::Motion { time, x, y, .. } => {
                state.pointer_events.push(PointerEvent::MotionAbsolute { time, x, y });
                state.pointer_events.push(PointerEvent::Frame);
            }
            wl_touch::Event::Up { time, .. } => {
                state.pointer_events.push(PointerEvent::Button {
                    time,
                    button: 0x110,
                    state: ButtonState::Released,
                });
                state.pointer_events.push(PointerEvent::Frame);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataDeviceManager, ()> for HostState {
    fn event(_: &mut Self, _: &WlDataDeviceManager, _: wayland_client::protocol::wl_data_device_manager::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlShm, ()> for HostState {
    fn event(_: &mut Self, _: &WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlShmPool, ()> for HostState {
    fn event(_: &mut Self, _: &WlShmPool, _: wayland_client::protocol::wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlDataDevice, ()> for HostState {
    event_created_child!(HostState, WlDataDevice, [
        0 => (WlDataOffer, Default::default()),
    ]);

    fn event(state: &mut Self, _: &WlDataDevice, event: wl_data_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                if let Some(prev) = state.dnd.incoming_offer.take() {
                    prev.destroy();
                }
                state.dnd.incoming_offer = Some(id);
                state.dnd.incoming_mimes.clear();
            }
            wl_data_device::Event::Enter { serial, id, .. } => {
                if state.dnd.host_drag_active {
                    // Our own escaped drag hovering back over the viewer. We
                    // never accept, so releasing here cancels the host drag;
                    // the slot drag is already gone (cancelled at escape).
                    println!("[nori-view] DnD own drag re-entered viewer; will not accept");
                } else if let Some(offer) = id {
                    // External host drag: inject into the slot and pull every
                    // payload eagerly so slot targets never wait for bytes.
                    let mimes = state.dnd.incoming_mimes.clone();
                    println!("[nori-view] DnD host drag entered viewer: {mimes:?}");
                    if mimes.is_empty() {
                        return;
                    }
                    state.dnd.send_cmd(
                        &serde_json::json!({"cmd": "external_drag_started", "mimes": mimes})
                            .to_string(),
                    );
                    // Accept the first MIME so the host source sees a live
                    // destination (v1 always copies).
                    if let Some(first) = state.dnd.incoming_mimes.first().cloned() {
                        offer.accept(serial, Some(first));
                    }
                    let (Some(writer), Some(cache)) =
                        (state.dnd.writer.clone(), state.dnd.host_cache.clone())
                    else {
                        return;
                    };
                    cache.lock().map(|mut c| c.clear()).ok();
                    for mime in state.dnd.incoming_mimes.clone() {
                        spawn_host_receive(&offer, mime, writer.clone(), cache.clone());
                    }
                }
            }
            wl_data_device::Event::Leave => {
                if let Some(offer) = state.dnd.incoming_offer.take() {
                    offer.destroy();
                }
                state.dnd.incoming_mimes.clear();
                if state.dnd.external_injected {
                    state.dnd.external_injected = false;
                    state
                        .dnd
                        .send_cmd(&serde_json::json!({"cmd": "external_drag_ended"}).to_string());
                }
            }
            wl_data_device::Event::Drop => {
                // Payloads were pushed eagerly at Enter; the slot-side drop
                // itself flows through the forwarded button release.
                println!("[nori-view] DnD host drop on viewer");
                state.dnd.external_injected = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataOffer, ()> for HostState {
    fn event(state: &mut Self, _: &WlDataOffer, event: wl_data_offer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_data_offer::Event::Offer { mime_type } => {
                println!("[nori-view] DnD incoming offer mime: {mime_type}");
                if !state.dnd.incoming_mimes.contains(&mime_type) {
                    state.dnd.incoming_mimes.push(mime_type);
                }
            }
            _ => {}
        }
    }
}

/// Pull one host payload on a thread (blocking read would stall the frame
/// loop), then cache it and push it to the slot.
fn spawn_host_receive(
    offer: &WlDataOffer,
    mime: String,
    writer: Arc<Mutex<UnixStream>>,
    cache: Arc<Mutex<HashMap<String, Option<Vec<u8>>>>>,
) {
    let (read, write) = match nix::unistd::pipe() {
        Ok((read, write)) => (read, write),
        Err(err) => {
            println!("[nori-view] DnD pipe failed for {mime}: {err}");
            cache.lock().map(|mut c| c.insert(mime.clone(), None)).ok();
            push_failed(&writer, &mime);
            return;
        }
    };
    offer.receive(mime.clone(), write.as_fd());
    // The fd is passed to the compositor; our copy can close.
    drop(write);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let file = File::from(read);
        let ok = file
            .take(16 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .is_ok();
        println!(
            "[nori-view] DnD host received {mime}: {} bytes (ok={ok})",
            bytes.len()
        );
        cache
            .lock()
            .map(|mut c| c.insert(mime.clone(), ok.then(|| bytes.clone())))
            .ok();
        let Ok(mut sock) = writer.lock() else {
            return;
        };
        if ok {
            let header =
                serde_json::json!({"cmd": "external_drag_data", "mime": mime, "len": bytes.len()})
                    .to_string()
                + "\n";
            if sock.write_all(header.as_bytes()).is_ok() {
                let _ = sock.write_all(&bytes);
            }
        } else {
            let line =
                serde_json::json!({"cmd": "external_drag_failed", "mime": mime}).to_string()
                    + "\n";
            let _ = sock.write_all(line.as_bytes());
        }
    });
}

fn push_failed(writer: &Arc<Mutex<UnixStream>>, mime: &str) {
    let line =
        serde_json::json!({"cmd": "external_drag_failed", "mime": mime}).to_string() + "\n";
    if let Ok(mut sock) = writer.lock() {
        let _ = sock.write_all(line.as_bytes());
    }
}

impl Dispatch<WlDataSource, ()> for HostState {
    fn event(state: &mut Self, source: &WlDataSource, event: wl_data_source::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_data_source::Event::Target { mime_type } => {
                println!("[nori-view] DnD host target accepted: {mime_type:?}");
                // Badge follows acceptance: green check over a live target,
                // neutral plus elsewhere. File glyphs persist from the cache.
                let spec = state.icon_spec(mime_type.is_some());
                if let Some(icon) = state.dnd.icon.as_mut() {
                    icon.redraw(spec);
                }
            }
            wl_data_source::Event::Send { mime_type, fd } => {
                let payload = state
                    .dnd
                    .drag
                    .as_ref()
                    .and_then(|drag| drag.cache.get(&mime_type))
                    .cloned()
                    .unwrap_or_default();
                println!(
                    "[nori-view] DnD serving {mime_type} to host: {} bytes",
                    payload.len()
                );
                let mut file = File::from(fd);
                let _ = file.write_all(&payload);
                // fd closed on drop: end of data.
            }
            wl_data_source::Event::Cancelled => {
                println!("[nori-view] DnD host drag cancelled");
                source.destroy();
                state.dnd.source = None;
                state.dnd_destroy_icon();
                state.dnd.host_drag_active = false;
            }
            wl_data_source::Event::DndDropPerformed => {
                println!("[nori-view] DnD host drop performed");
            }
            wl_data_source::Event::DndFinished => {
                println!("[nori-view] DnD host drag finished");
                source.destroy();
                state.dnd.source = None;
                state.dnd_destroy_icon();
                state.dnd.host_drag_active = false;
            }
            wl_data_source::Event::Action { dnd_action } => {
                println!("[nori-view] DnD host chose action {dnd_action:?} (v1 always copies)");
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for HostState {
    fn event(_: &mut Self, _: &ZwpLinuxDmabufV1, _: wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for HostState {
    fn event(state: &mut Self, _: &ZwpLinuxDmabufFeedbackV1, event: zwp_linux_dmabuf_feedback_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        handle_feedback_event(&mut state.dmabuf_feedback, event);
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, usize> for HostState {
    fn event(_: &mut Self, _: &ZwpLinuxBufferParamsV1, _: zwp_linux_buffer_params_v1::Event, _: &usize, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlBuffer, usize> for HostState {
    fn event(state: &mut Self, _: &WlBuffer, event: wayland_client::protocol::wl_buffer::Event, id: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let wayland_client::protocol::wl_buffer::Event::Release = event {
            if *id < state.buffer_busy.len() {
                state.buffer_busy[*id] = false;
            }
        }
    }
}

impl Dispatch<WlCallback, ()> for HostState {
    fn event(state: &mut Self, _: &WlCallback, event: wayland_client::protocol::wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wayland_client::protocol::wl_callback::Event::Done { .. } = event {
            state.frame_done = true;
        }
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitManagerV1, ()> for HostState {
    fn event(_: &mut Self, _: &ZwpKeyboardShortcutsInhibitManagerV1, _: wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for HostState {
    fn event(state: &mut Self, _: &ZwpKeyboardShortcutsInhibitorV1, event: zwp_keyboard_shortcuts_inhibitor_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => {
                state.inhibitor_active = true;
                println!("[nori-view] Shortcuts inhibitor: ACTIVE (captive to slot)");
            }
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => {
                state.inhibitor_active = false;
                state.release_all_held_keys();
                println!("[nori-view] Shortcuts inhibitor: INACTIVE (released to host)");
            }
            _ => {}
        }
    }
}

impl Dispatch<WpViewporter, ()> for HostState {
    fn event(_: &mut Self, _: &WpViewporter, _: wayland_protocols::wp::viewporter::client::wp_viewporter::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WpViewport, ()> for HostState {
    fn event(_: &mut Self, _: &WpViewport, _: wayland_protocols::wp::viewporter::client::wp_viewport::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

// ---------------------------------------------------------------------------
// Target State & Handlers
// ---------------------------------------------------------------------------

struct TargetOutput {
    output: WlOutput,
    name: Option<String>,
    width: u32,
    height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DamageRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// One preference tranche from zwp_linux_dmabuf_feedback_v1, kept in the
/// descending preference order the compositor sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FeedbackTranche {
    scanout: bool,
    /// (fourcc, modifier) pairs in format-table order.
    formats: Vec<(u32, u64)>,
}

/// Assembled default feedback from one compositor.
#[derive(Clone, Debug, Default)]
struct DmabufFeedback {
    tranches: Vec<FeedbackTranche>,
    done: bool,
}

/// Incremental collector for feedback events on one connection.
#[derive(Clone, Debug, Default)]
struct FeedbackCollector {
    table: Vec<(u32, u64)>,
    tranches: Vec<FeedbackTranche>,
    pending_scanout: bool,
    pending_formats: Vec<(u32, u64)>,
    done: bool,
}

impl FeedbackCollector {
    fn push_table(&mut self, entries: Vec<(u32, u64)>) {
        self.table = entries;
    }

    fn push_tranche_flags(&mut self, scanout: bool) {
        self.pending_scanout = scanout;
    }

    fn push_tranche_formats(&mut self, indices: &[u16]) {
        for idx in indices {
            if let Some(entry) = self.table.get(*idx as usize) {
                self.pending_formats.push(*entry);
            }
        }
    }

    fn end_tranche(&mut self) {
        self.tranches.push(FeedbackTranche {
            scanout: self.pending_scanout,
            formats: std::mem::take(&mut self.pending_formats),
        });
        self.pending_scanout = false;
    }

    fn finish(&mut self) {
        self.done = true;
    }

    fn snapshot(&self) -> DmabufFeedback {
        DmabufFeedback { tranches: self.tranches.clone(), done: self.done }
    }
}

fn handle_feedback_event(collector: &mut FeedbackCollector, event: zwp_linux_dmabuf_feedback_v1::Event) {
    match event {
        zwp_linux_dmabuf_feedback_v1::Event::FormatTable { fd, .. } => {
            collector.push_table(read_format_table_fd(fd));
        }
        zwp_linux_dmabuf_feedback_v1::Event::TrancheFlags { flags } => {
            let scanout = matches!(flags, WEnum::Value(f) if f.contains(zwp_linux_dmabuf_feedback_v1::TrancheFlags::Scanout));
            collector.push_tranche_flags(scanout);
        }
        zwp_linux_dmabuf_feedback_v1::Event::TrancheFormats { indices } => {
            collector.push_tranche_formats(&parse_tranche_indices(&indices));
        }
        zwp_linux_dmabuf_feedback_v1::Event::TrancheDone => collector.end_tranche(),
        zwp_linux_dmabuf_feedback_v1::Event::Done => collector.finish(),
        _ => {}
    }
}

struct TargetState {
    seat: Option<WlSeat>,
    outputs: Vec<TargetOutput>,
    output_name: String,
    selected: Option<usize>,
    screencopy_manager: Option<ZwlrScreencopyManagerV1>,
    linux_dmabuf: Option<ZwpLinuxDmabufV1>,
    dmabuf_feedback: FeedbackCollector,
    dmabuf_feedback_obj: Option<ZwpLinuxDmabufFeedbackV1>,
    virtual_pointer_mgr: Option<ZwlrVirtualPointerManagerV1>,
    virtual_pointer: Option<ZwlrVirtualPointerV1>,
    virtual_keyboard_mgr: Option<ZwpVirtualKeyboardManagerV1>,
    virtual_keyboard: Option<ZwpVirtualKeyboardV1>,

    width: u32,
    height: u32,
    /// Damage rects accumulated per in-flight capture (keyed by buffer index).
    frame_damage: HashMap<usize, Vec<DamageRect>>,
    /// Captures that completed since the last drain: (buffer index, damage).
    completed: Vec<(usize, Vec<DamageRect>)>,
    /// Buffer indexes whose capture failed since the last drain.
    failed: Vec<usize>,
}

impl Dispatch<WlRegistry, ()> for TargetState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_seat" => {
                    let seat = registry.bind(name, version.min(7), qh, ());
                    state.seat = Some(seat);
                }
                "wl_output" => {
                    let idx = state.outputs.len();
                    let out = registry.bind(name, version.min(4), qh, idx);
                    state.outputs.push(TargetOutput {
                        output: out,
                        name: None,
                        width: 0,
                        height: 0,
                    });
                }
                "zwlr_screencopy_manager_v1" => {
                    state.screencopy_manager = Some(registry.bind(name, version.min(3), qh, ()));
                }
                "zwp_linux_dmabuf_v1" => {
                    state.linux_dmabuf = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    let mgr: ZwlrVirtualPointerManagerV1 = registry.bind(name, version.min(2), qh, ());
                    state.virtual_pointer = Some(mgr.create_virtual_pointer(None, qh, ()));
                    state.virtual_pointer_mgr = Some(mgr);
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    let mgr: ZwpVirtualKeyboardManagerV1 = registry.bind(name, 1, qh, ());
                    state.virtual_keyboard_mgr = Some(mgr);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlOutput, usize> for TargetState {
    fn event(state: &mut Self, _: &WlOutput, event: wl_output::Event, idx: &usize, _: &Connection, _: &QueueHandle<Self>) {
        let Some(entry) = state.outputs.get_mut(*idx) else {
            return;
        };
        match event {
            wl_output::Event::Mode { width, height, flags, .. } => {
                if let WEnum::Value(flags) = flags {
                    if flags.contains(wl_output::Mode::Current) || entry.width == 0 {
                        entry.width = width as u32;
                        entry.height = height as u32;
                    }
                }
            }
            wl_output::Event::Name { name } => {
                entry.name = Some(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for TargetState {
    fn event(_: &mut Self, _: &ZwlrScreencopyManagerV1, _: wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrScreencopyFrameV1, usize> for TargetState {
    fn event(state: &mut Self, frame: &ZwlrScreencopyFrameV1, event: zwlr_screencopy_frame_v1::Event, buffer_idx: &usize, _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwlr_screencopy_frame_v1::Event::Damage { x, y, width, height } => {
                state
                    .frame_damage
                    .entry(*buffer_idx)
                    .or_default()
                    .push(DamageRect {
                        x: x as i32,
                        y: y as i32,
                        w: width as i32,
                        h: height as i32,
                    });
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                let damage = state.frame_damage.remove(buffer_idx).unwrap_or_default();
                state.completed.push((*buffer_idx, damage));
                frame.destroy();
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.frame_damage.remove(buffer_idx);
                state.failed.push(*buffer_idx);
                frame.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for TargetState {
    fn event(_: &mut Self, _: &ZwpLinuxDmabufV1, _: wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for TargetState {
    fn event(state: &mut Self, _: &ZwpLinuxDmabufFeedbackV1, event: zwp_linux_dmabuf_feedback_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        handle_feedback_event(&mut state.dmabuf_feedback, event);
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, usize> for TargetState {
    fn event(_: &mut Self, _: &ZwpLinuxBufferParamsV1, _: zwp_linux_buffer_params_v1::Event, _: &usize, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlBuffer, usize> for TargetState {
    fn event(_: &mut Self, _: &WlBuffer, _: wayland_client::protocol::wl_buffer::Event, _: &usize, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for TargetState {
    fn event(_: &mut Self, _: &ZwlrVirtualPointerManagerV1, _: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for TargetState {
    fn event(_: &mut Self, _: &ZwlrVirtualPointerV1, _: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for TargetState {
    fn event(_: &mut Self, _: &ZwpVirtualKeyboardManagerV1, _: wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for TargetState {
    fn event(_: &mut Self, _: &ZwpVirtualKeyboardV1, _: wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlSeat, ()> for TargetState {
    fn event(_: &mut Self, _: &WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn resolve_socket_path(name_or_path: &str) -> PathBuf {
    if name_or_path.starts_with('/') {
        PathBuf::from(name_or_path)
    } else {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{}", nix::unistd::getuid()));
        Path::new(&runtime_dir).join(name_or_path)
    }
}

/// Pick the target output index for `--output`.
///
/// Prefers an exact name match. Falls back to the lone output only when its
/// name has not been advertised yet, so a typo never silently captures the
/// wrong head on a multi-output slot.
fn select_output_index(names: &[Option<String>], wanted: &str) -> Option<usize> {
    if let Some(idx) = names.iter().position(|n| n.as_deref() == Some(wanted)) {
        return Some(idx);
    }
    if names.len() == 1 && names[0].is_none() {
        return Some(0);
    }
    None
}

/// Pick the next capture buffer, skipping buffers the host still holds and
/// buffers with a capture already in flight. `is_busy` covers both.
fn pick_free_buffer(num_buffers: usize, next: usize, is_busy: impl Fn(usize) -> bool) -> Option<usize> {
    if num_buffers == 0 {
        return None;
    }
    for i in 0..num_buffers {
        let candidate = (next + i) % num_buffers;
        if !is_busy(candidate) {
            return Some(candidate);
        }
    }
    None
}

enum DamagePlan<'a> {
    Full,
    Rects(&'a [DamageRect]),
}

/// Decide how to forward screencopy damage to the host surface.
///
/// Unknown damage (no rects) must assume full damage. Too many rects, or
/// damage covering most of the buffer, is cheaper as one full damage.
fn coalesce_damage(damage: &[DamageRect], buffer_width: u32, buffer_height: u32) -> DamagePlan<'_> {
    const MAX_DAMAGE_RECTS: usize = 16;
    if damage.is_empty() || damage.len() > MAX_DAMAGE_RECTS {
        return DamagePlan::Full;
    }
    let total: u64 = damage.iter().map(|r| r.w.max(0) as u64 * r.h.max(0) as u64).sum();
    let full = buffer_width as u64 * buffer_height as u64;
    if full > 0 && total * 4 >= full * 3 {
        DamagePlan::Full
    } else {
        DamagePlan::Rects(damage)
    }
}

/// Parse a linux-dmabuf format-table blob: tightly packed 16-byte entries of
/// u32 fourcc (native endian), 4 bytes padding, u64 modifier (native endian).
fn parse_format_table(bytes: &[u8]) -> Vec<(u32, u64)> {
    bytes
        .chunks_exact(16)
        .map(|c| {
            let fourcc = u32::from_ne_bytes(c[0..4].try_into().unwrap());
            let modifier = u64::from_ne_bytes(c[8..16].try_into().unwrap());
            (fourcc, modifier)
        })
        .collect()
}

/// Parse a tranche_formats index array: tightly packed u16 indexes (native
/// endian) into the format table.
fn parse_tranche_indices(bytes: &[u8]) -> Vec<u16> {
    bytes.chunks_exact(2).map(|c| u16::from_ne_bytes(c.try_into().unwrap())).collect()
}

fn read_format_table_fd(fd: std::os::fd::OwnedFd) -> Vec<(u32, u64)> {
    use std::io::Read;
    use std::io::Seek;
    let mut file = std::fs::File::from(fd);
    // The table fd shares its file offset with the compositor's memfd across
    // SCM_RIGHTS (same open file description), and any reader leaves it at
    // EOF. Always rewind first; the spec's mmap approach is immune, read is
    // not.
    if file.rewind().is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_ok() {
        parse_format_table(&bytes)
    } else {
        Vec::new()
    }
}

/// Modifier value for a linear (untiled) layout.
const MOD_LINEAR: u64 = 0;
/// Wildcard modifier: advertised to mean "any layout", never allocatable.
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Map a negotiated fourcc back to the GBM format used for allocation.
/// Only Xrgb8888 is considered: the screencopy server hard-requires it for
/// dmabuf capture buffers and rejects anything else as invalid.
fn gbm_format_from_fourcc(fourcc: u32) -> Option<Format> {
    if fourcc == Format::Xrgb8888 as u32 {
        Some(Format::Xrgb8888)
    } else {
        None
    }
}

fn fourcc_name(fourcc: u32) -> String {
    String::from_utf8_lossy(&fourcc.to_le_bytes()).into_owned()
}

/// Legacy LINEAR allocation, kept as the fallback when feedback negotiation
/// yields nothing usable.
fn linear_buffer(gbm_dev: &Device<File>, width: u32, height: u32) -> Result<BufferObject<()>> {
    gbm_dev
        .create_buffer_object::<()>(
            width,
            height,
            Format::Xrgb8888,
            BufferObjectFlags::RENDERING | BufferObjectFlags::LINEAR,
        )
        .context("error creating GBM buffer object")
}

/// Pick the (fourcc, modifier) pair for the shared capture buffers.
///
/// Only Xrgb8888 pairs common to both sides are eligible. Preference passes,
/// each walking the target's tranches in compositor order:
/// 1. a pair from a scanout-flagged tranche (direct scanout / overlay path),
/// 2. otherwise the first pair with a non-linear modifier (tiled GPU layout),
/// 3. otherwise the first common pair (may be LINEAR).
/// The passes exist because renderers commonly list LINEAR first even though
/// it is the slowest layout; a plain ordered walk would never improve on it.
fn negotiate_buffer_format(target: &DmabufFeedback, host: &DmabufFeedback) -> Option<(u32, u64)> {
    if !target.done || !host.done {
        return None;
    }
    let host_pairs: HashSet<(u32, u64)> =
        host.tranches.iter().flat_map(|t| t.formats.iter().copied()).collect();
    let eligible = |fourcc: u32, modifier: u64| {
        modifier != MOD_INVALID
            && gbm_format_from_fourcc(fourcc).is_some()
            && host_pairs.contains(&(fourcc, modifier))
    };
    for tranche in &target.tranches {
        if !tranche.scanout {
            continue;
        }
        for (fourcc, modifier) in &tranche.formats {
            if eligible(*fourcc, *modifier) {
                return Some((*fourcc, *modifier));
            }
        }
    }
    for tranche in &target.tranches {
        for (fourcc, modifier) in &tranche.formats {
            if *modifier != MOD_LINEAR && eligible(*fourcc, *modifier) {
                return Some((*fourcc, *modifier));
            }
        }
    }
    for tranche in &target.tranches {
        for (fourcc, modifier) in &tranche.formats {
            if eligible(*fourcc, *modifier) {
                return Some((*fourcc, *modifier));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Main Orchestration
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let args = Args::parse();

    let target_display = if let Some(target) = args.target {
        target
    } else if let Some(slot) = args.slot {
        format!("wayland-nori-{}", slot)
    } else {
        "wayland-nori-1".to_string()
    };

    let host_display = args.host.unwrap_or_else(|| {
        std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-1".to_string())
    });

    let target_socket_path = resolve_socket_path(&target_display);
    let host_socket_path = resolve_socket_path(&host_display);

    println!("[nori-view] Initializing Zero-Copy DMA-BUF Wayland Bridge");
    println!("[nori-view] Target: {:?}", target_socket_path);
    println!("[nori-view] Host:   {:?}", host_socket_path);

    // Initialize DRM / GBM Device
    let drm_file = File::options()
        .read(true)
        .write(true)
        .open(&args.drm_node)
        .context("error opening DRM render node")?;
    let gbm_dev = Device::new(drm_file).context("error creating GBM device")?;

    // Connect to Target
    let target_stream = UnixStream::connect(&target_socket_path)
        .with_context(|| format!("error connecting to target socket {:?}", target_socket_path))?;
    let target_conn = Connection::from_socket(target_stream)?;
    let mut target_queue: EventQueue<TargetState> = target_conn.new_event_queue();
    let target_qh = target_queue.handle();

    let mut target_state = TargetState {
        seat: None,
        outputs: Vec::new(),
        output_name: args.output.clone(),
        selected: None,
        screencopy_manager: None,
        linux_dmabuf: None,
        dmabuf_feedback: FeedbackCollector::default(),
        dmabuf_feedback_obj: None,
        virtual_pointer_mgr: None,
        virtual_pointer: None,
        virtual_keyboard_mgr: None,
        virtual_keyboard: None,
        width: 1920,
        height: 1080,
        frame_damage: HashMap::new(),
        completed: Vec::new(),
        failed: Vec::new(),
    };

    let _target_registry = target_conn.display().get_registry(&target_qh, ());
    target_queue.roundtrip(&mut target_state)?;
    target_queue.roundtrip(&mut target_state)?;

    // Request dmabuf feedback (v4+) so allocation can use the compositor's
    // preferred format/modifier instead of LINEAR.
    let target_dmabuf_proto = target_state.linux_dmabuf.clone();
    if let Some(dmabuf) = &target_dmabuf_proto {
        if dmabuf.version() >= 4 {
            target_state.dmabuf_feedback_obj = Some(dmabuf.get_default_feedback(&target_qh, ()));
        }
    }
    target_queue.roundtrip(&mut target_state)?;

    let discovered: Vec<Option<String>> = target_state.outputs.iter().map(|o| o.name.clone()).collect();
    let selected = select_output_index(&discovered, &target_state.output_name).with_context(|| {
        let names: Vec<String> = discovered.iter().map(|n| n.clone().unwrap_or_else(|| "(unnamed)".to_string())).collect();
        format!("output '{}' not found on target (discovered: [{}])", target_state.output_name, names.join(", "))
    })?;
    {
        let entry = &target_state.outputs[selected];
        target_state.width = entry.width;
        target_state.height = entry.height;
    }
    target_state.selected = Some(selected);
    println!("[nori-view] Selected target output '{}' ({}x{})", target_state.output_name, target_state.width, target_state.height);

    if target_state.virtual_keyboard.is_none() {
        if let (Some(mgr), Some(seat)) = (&target_state.virtual_keyboard_mgr, &target_state.seat) {
            target_state.virtual_keyboard = Some(mgr.create_virtual_keyboard(seat, &target_qh, ()));
            println!("[nori-view] Successfully initialized Target Virtual Keyboard for slot input");
        }
    }

    // Connect to Host
    let host_stream = UnixStream::connect(&host_socket_path)
        .with_context(|| format!("error connecting to host socket {:?}", host_socket_path))?;
    let host_conn = Connection::from_socket(host_stream)?;
    let mut host_queue: EventQueue<HostState> = host_conn.new_event_queue();
    let host_qh = host_queue.handle();

    let mut host_state = HostState {
        compositor: None,
        xdg_wm_base: None,
        linux_dmabuf: None,
        dmabuf_feedback: FeedbackCollector::default(),
        dmabuf_feedback_obj: None,
        seat: None,
        inhibit_manager: None,
        inhibitor: None,
        viewporter: None,
        viewport: None,
        surface: None,
        xdg_surface: None,
        toplevel: None,
        pointer: None,
        keyboard: None,
        touch: None,
        configured: false,
        width: target_state.width,
        height: target_state.height,
        pointer_events: Vec::new(),
        keyboard_events: Vec::new(),
        pending_axis_source: wl_pointer::AxisSource::Wheel,
        last_axis_value: [0.0; 2],
        held_keys: HashSet::new(),
        inhibitor_active: true,
        mod_held: false,
        shift_held: false,
        buffer_busy: Vec::new(),
        frame_done: true,
        is_fullscreen: false,
        is_focused: true,
        last_unfocused_capture: None,
        unfocused_since: None,
        capture_halted: false,
        closed: false,
        dnd: DndBridge::default(),
    };

    let _host_registry = host_conn.display().get_registry(&host_qh, ());
    host_queue.roundtrip(&mut host_state)?;
    host_queue.roundtrip(&mut host_state)?;

    let host_dmabuf_proto = host_state.linux_dmabuf.clone();
    if let Some(dmabuf) = &host_dmabuf_proto {
        if dmabuf.version() >= 4 {
            host_state.dmabuf_feedback_obj = Some(dmabuf.get_default_feedback(&host_qh, ()));
        }
    }
    host_queue.roundtrip(&mut host_state)?;

    // Setup Host Surface & Toplevel
    let compositor = host_state.compositor.as_ref().context("host lacks wl_compositor")?;
    let xdg_wm_base = host_state.xdg_wm_base.as_ref().context("host lacks xdg_wm_base")?;
    let host_surface = compositor.create_surface(&host_qh, ());

    if let Some(viewporter) = &host_state.viewporter {
        let vp = viewporter.get_viewport(&host_surface, &host_qh, ());
        vp.set_destination(host_state.width as i32, host_state.height as i32);
        host_state.viewport = Some(vp);
    }

    let xdg_surface = xdg_wm_base.get_xdg_surface(&host_surface, &host_qh, ());
    let toplevel = xdg_surface.get_toplevel(&host_qh, ());

    let window_title = args.title.unwrap_or_else(|| format!("nori-viewer:{}", target_display));
    let app_id = args.app_id.unwrap_or_else(|| format!("nori-viewer-{}", target_display));

    toplevel.set_title(window_title);
    toplevel.set_app_id(app_id);

    let should_fullscreen = args.fullscreen && !args.windowed;
    if should_fullscreen {
        toplevel.set_fullscreen(None);
        host_state.is_fullscreen = true;
    }

    // Inhibit host keyboard shortcuts when focused so Mod hotkeys reach slot
    if let (Some(inhibit_mgr), Some(seat)) = (&host_state.inhibit_manager, &host_state.seat) {
        host_state.inhibitor = Some(inhibit_mgr.inhibit_shortcuts(&host_surface, seat, &host_qh, ()));
    }

    // DnD bridge: host data device for drag injection, slot socket for export.
    if let (Some(mgr), Some(seat)) = (host_state.dnd.dd_manager.clone(), host_state.seat.clone()) {
        host_state.dnd.dd = Some(mgr.get_data_device(&seat, &host_qh, ()));
    }
    // Badged head cursor: numeral = head, accent = node (overridable).
    let head_num = args.head_num.or(args.slot).unwrap_or(0);
    let node = args.node.clone().unwrap_or_else(|| "local".to_string());
    let accent = args
        .accent
        .as_deref()
        .and_then(cursor::parse_accent)
        .unwrap_or_else(|| cursor::accent_for_node(&node));
    host_state.dnd.head_num = head_num;
    host_state.dnd.accent = accent;
    if let (Some(shm), Some(compositor)) =
        (host_state.dnd.shm.clone(), host_state.compositor.clone())
    {
        let spec = cursor::CursorSpec::idle(head_num, accent);
        host_state.dnd.cursor =
            CursorSurface::create(&compositor, &shm, &host_qh, "cursor", spec);
        println!("[nori-view] Head cursor ready: head={head_num} node={node}");
    } else {
        println!("[nori-view] Head cursor unavailable (no shm/compositor)");
    }
    let slot_no = args.slot.or_else(|| parse_slot_from_display(&target_display));
    if let Some(slot) = slot_no {
        let path = std::env::var("XDG_RUNTIME_DIR")
            .map(|run| format!("{run}/nori-dnd-{slot}.sock"))
            .unwrap_or_default();
        match UnixStream::connect(&path) {
            Ok(sock) => {
                let _ = sock.set_nonblocking(true);
                // Separate blocking writer connection: a dup'd clone would
                // share O_NONBLOCK with the reader.
                let writer = UnixStream::connect(&path)
                    .ok()
                    .map(|s| Arc::new(Mutex::new(s)));
                host_state.dnd.sock_path = Some(PathBuf::from(&path));
                host_state.dnd.writer = writer;
                host_state.dnd.host_cache = Some(Arc::new(Mutex::new(HashMap::new())));
                host_state.dnd.sock = Some(sock);
                host_state.dnd.send_cmd(&serde_json::json!({"cmd": "watch"}).to_string());
                println!("[nori-view] DnD watching {path}");
            }
            Err(err) => {
                println!("[nori-view] DnD unavailable ({path}): {err}");
            }
        }
    } else {
        println!("[nori-view] DnD disabled: cannot determine slot number");
    }

    host_surface.commit();
    host_conn.flush()?;

    host_state.surface = Some(host_surface.clone());
    host_state.xdg_surface = Some(xdg_surface);
    host_state.toplevel = Some(toplevel);

    // Wait for host configure event
    while !host_state.configured {
        host_queue.blocking_dispatch(&mut host_state)?;
    }

    println!(
        "[nori-view] Target output resolution: {}x{}",
        target_state.width, target_state.height
    );

    // Negotiate a shared (format, modifier) from both compositors' dmabuf
    // feedback so buffers use tiled/scanout layouts instead of LINEAR.
    let target_dmabuf = target_state.linux_dmabuf.as_ref().context("target lacks zwp_linux_dmabuf_v1")?;
    let host_dmabuf = host_state.linux_dmabuf.as_ref().context("host lacks zwp_linux_dmabuf_v1")?;
    let negotiated = negotiate_buffer_format(
        &target_state.dmabuf_feedback.snapshot(),
        &host_state.dmabuf_feedback.snapshot(),
    );
    let mut try_modifiers: Option<(Format, u32, u64)> = None;
    match negotiated {
        Some((fourcc, modifier)) => {
            if let Some(format) = gbm_format_from_fourcc(fourcc) {
                println!(
                    "[nori-view] Negotiated dmabuf {} modifier 0x{:016x} ({} target tranches, {} host tranches)",
                    fourcc_name(fourcc),
                    modifier,
                    target_state.dmabuf_feedback.tranches.len(),
                    host_state.dmabuf_feedback.tranches.len(),
                );
                try_modifiers = Some((format, fourcc, modifier));
            }
        }
        None => {
            println!("[nori-view] No common dmabuf format/modifier in feedback; falling back to LINEAR");
        }
    }

    // Allocate 4 Zero-Copy DMA-BUF Buffers (Quad Buffering with Host Release Tracking)
    let buffer_width = target_state.width;
    let buffer_height = target_state.height;

    let mut buffers: Vec<DmaBuffer> = Vec::new();
    const NUM_BUFFERS: usize = 4;
    host_state.buffer_busy = vec![false; NUM_BUFFERS];

    let mut warned_fallback = false;
    for id in 0..NUM_BUFFERS {
        // Try the negotiated tiled modifier first; fall back to LINEAR when
        // allocation fails so an exotic tranche can never break startup.
        let (bo, fourcc) = match try_modifiers {
            Some((format, fourcc, modifier)) => {
                match gbm_dev.create_buffer_object_with_modifiers2::<()>(
                    buffer_width,
                    buffer_height,
                    format,
                    std::iter::once(Modifier::from(modifier)),
                    BufferObjectFlags::RENDERING | BufferObjectFlags::SCANOUT,
                ) {
                    Ok(bo) => (bo, fourcc),
                    Err(err) => {
                        if !warned_fallback {
                            println!("[nori-view] Modifier allocation failed ({err:#}); falling back to LINEAR");
                            warned_fallback = true;
                        }
                        (linear_buffer(&gbm_dev, buffer_width, buffer_height)?, Format::Xrgb8888 as u32)
                    }
                }
            }
            None => (linear_buffer(&gbm_dev, buffer_width, buffer_height)?, Format::Xrgb8888 as u32),
        };

        let modifier = u64::from(bo.modifier());
        let modifier_hi = (modifier >> 32) as u32;
        let modifier_lo = (modifier & 0xFFFFFFFF) as u32;
        let plane_count = bo.plane_count();

        // Target buffer (one fresh prime fd per plane)
        let target_params = target_dmabuf.create_params(&target_qh, id);
        for plane in 0..plane_count {
            let fd = bo.fd_for_plane(plane as i32).map_err(|e| anyhow::anyhow!("error exporting BO plane fd: {e:?}"))?;
            target_params.add(fd.as_fd(), plane, bo.offset(plane as i32), bo.stride_for_plane(plane as i32), modifier_hi, modifier_lo);
        }
        let target_wl_buf = target_params.create_immed(
            buffer_width as i32,
            buffer_height as i32,
            fourcc,
            zwp_linux_buffer_params_v1::Flags::empty(),
            &target_qh,
            id,
        );

        // Host buffer (fresh fds again so each compositor owns its descriptors)
        let host_params = host_dmabuf.create_params(&host_qh, id);
        for plane in 0..plane_count {
            let fd = bo.fd_for_plane(plane as i32).map_err(|e| anyhow::anyhow!("error exporting BO plane fd: {e:?}"))?;
            host_params.add(fd.as_fd(), plane, bo.offset(plane as i32), bo.stride_for_plane(plane as i32), modifier_hi, modifier_lo);
        }
        let host_wl_buf = host_params.create_immed(
            buffer_width as i32,
            buffer_height as i32,
            fourcc,
            zwp_linux_buffer_params_v1::Flags::empty(),
            &host_qh,
            id,
        );

        buffers.push(DmaBuffer {
            _bo: bo,
            target_buffer: target_wl_buf,
            host_buffer: host_wl_buf,
        });
    }

    target_queue.roundtrip(&mut target_state)?;
    host_queue.roundtrip(&mut host_state)?;

    println!(
        "[nori-view] Successfully initialized {} Zero-Copy DMA-BUF buffers in VRAM",
        buffers.len()
    );

    let screencopy_mgr = target_state.screencopy_manager.clone().context("target lacks zwlr_screencopy_manager_v1")?;
    let selected_idx = target_state.selected.context("no target output selected")?;
    let target_output = target_state.outputs[selected_idx].output.clone();
    let virtual_pointer = target_state.virtual_pointer.clone();
    let virtual_keyboard = target_state.virtual_keyboard.clone();

    const MAX_IN_FLIGHT: usize = 2;
    /// Unfocused duration after which captures halt entirely so the slot
    /// compositor's cast deadline expires (true standby). Mirrors the
    /// pixos watcher's SLOW->FROZEN horizon semantics (shorter: the viewer
    /// leads, the watcher follows with deck freeze + capture detach).
    const CAPTURE_HALT_AFTER: Duration = Duration::from_secs(30);
    let mut in_flight: HashMap<usize, ZwlrScreencopyFrameV1> = HashMap::new();
    let mut next_buffer_idx = 0;

    // Zero-Copy Streaming Loop
    while !host_state.closed {
        // Dispatch host events (user input, window resizes)
        host_queue.dispatch_pending(&mut host_state)?;

        // DnD bridge: pump the slot socket, then escalate any drag that left
        // the viewer surface into a host-side drag.
        host_state.dnd.pump();
        host_state.dnd_maybe_escape(&host_qh);
        host_state.sync_cursor();

        // Forward pointer events from host to target
        if let Some(vp) = &virtual_pointer {
            for event in host_state.pointer_events.drain(..) {
                match event {
                    PointerEvent::MotionAbsolute { time, x, y } => {
                        let norm_x = (x.max(0.0) / host_state.width as f64).min(1.0) * buffer_width as f64;
                        let norm_y = (y.max(0.0) / host_state.height as f64).min(1.0) * buffer_height as f64;
                        vp.motion_absolute(time, norm_x as u32, norm_y as u32, buffer_width, buffer_height);
                    }
                    PointerEvent::Button { time, button, state } => {
                        vp.button(time, button, state);
                    }
                    // Axis contract: forward values and source unchanged. The host
                    // libinput already applied the user's natural-scroll
                    // preference; the headless slot has no libinput devices, so
                    // it interprets these raw values as-is. Never negate here.
                    PointerEvent::Axis {
                        time,
                        axis,
                        value,
                        source,
                    } => {
                        vp.axis_source(source);
                        vp.axis(time, axis, value);
                    }
                    PointerEvent::AxisDiscrete {
                        axis,
                        value,
                        discrete,
                    } => {
                        vp.axis_discrete(0, axis, value, discrete);
                    }
                    PointerEvent::AxisStop { time, axis } => {
                        vp.axis_stop(time, axis);
                    }
                    PointerEvent::Frame => {
                        vp.frame();
                    }
                }
            }
            target_conn.flush()?;
        }

        // Forward keyboard events from host to target
        if let Some(vk) = &virtual_keyboard {
            for event in host_state.keyboard_events.drain(..) {
                match event {
                    KeyboardEvent::Key { time, key, state } => {
                        vk.key(time, key, state);
                    }
                    KeyboardEvent::Modifiers { depressed, latched, locked, group } => {
                        vk.modifiers(depressed, latched, locked, group);
                    }
                }
            }
            target_conn.flush()?;
        }

        // Determine whether to initiate a screencopy frame capture:
        // - When focused: capture continuously at full speed (60 FPS)
        // - When unfocused < CAPTURE_HALT_AFTER: throttle to standby rate
        //   (5 FPS = every 200ms) to let the slot sleep
        // - When unfocused past the halt horizon: stop capturing entirely.
        //   Zero in-flight frames lets the slot compositor's cast deadline
        //   expire, clearing has_active_cast() so the slot drops to its
        //   true standby clock instead of rendering 60Hz for a dark viewer.
        //   Refocus resumes immediately (unfocused_since clears on focus).
        let now = std::time::Instant::now();
        let past_halt = host_state
            .unfocused_since
            .is_some_and(|t0| now.duration_since(t0) >= CAPTURE_HALT_AFTER);
        if past_halt && !host_state.capture_halted {
            println!("[nori-view] Unfocused past halt horizon: stopping captures (slot may standby)");
            host_state.capture_halted = true;
        } else if !past_halt && host_state.capture_halted {
            println!("[nori-view] Focus regained: resuming captures");
            host_state.capture_halted = false;
        }
        let should_capture = if host_state.is_focused {
            true
        } else if past_halt {
            false
        } else {
            match host_state.last_unfocused_capture {
                Some(last) => now.duration_since(last) >= Duration::from_millis(200),
                None => true,
            }
        };

        // Pace captures on the host frame callback so we never render frames
        // the host compositor is not ready to consume.
        if should_capture && host_state.frame_done && in_flight.len() < MAX_IN_FLIGHT {
            let free_idx = pick_free_buffer(buffers.len(), next_buffer_idx, |idx| {
                host_state.buffer_busy[idx] || in_flight.contains_key(&idx)
            });

            if let Some(buf_idx) = free_idx {
                next_buffer_idx = (buf_idx + 1) % buffers.len();
                let target_buf = &buffers[buf_idx].target_buffer;
                let frame = screencopy_mgr.capture_output(0, &target_output, &target_qh, buf_idx);
                frame.copy_with_damage(target_buf);
                in_flight.insert(buf_idx, frame);
                target_conn.flush()?;
                if !host_state.is_focused {
                    host_state.last_unfocused_capture = Some(now);
                }
            }
        }

        // Flush pending requests to both compositors
        host_conn.flush()?;
        target_conn.flush()?;

        let host_guard = host_conn.prepare_read();
        let target_guard = target_conn.prepare_read();

        if let (Some(hg), Some(tg)) = (host_guard, target_guard) {
            let mut poll_fds = [
                nix::poll::PollFd::new(hg.connection_fd(), nix::poll::PollFlags::POLLIN),
                nix::poll::PollFd::new(tg.connection_fd(), nix::poll::PollFlags::POLLIN),
            ];
            let poll_timeout = if host_state.is_focused { 16u16 } else { 100u16 };
            let _ = nix::poll::poll(&mut poll_fds, nix::poll::PollTimeout::from(poll_timeout));
            let host_revents = poll_fds[0].revents().unwrap_or(nix::poll::PollFlags::empty());
            let target_revents = poll_fds[1].revents().unwrap_or(nix::poll::PollFlags::empty());
            drop(poll_fds);

            if host_revents.intersects(nix::poll::PollFlags::POLLIN | nix::poll::PollFlags::POLLHUP | nix::poll::PollFlags::POLLERR) {
                let _ = hg.read();
            }
            if target_revents.intersects(nix::poll::PollFlags::POLLIN | nix::poll::PollFlags::POLLHUP | nix::poll::PollFlags::POLLERR) {
                let _ = tg.read();
            }
        }

        // Dispatch target events (e.g. screencopy frame ready)
        target_queue.dispatch_pending(&mut target_state)?;

        let failures: Vec<usize> = target_state.failed.drain(..).collect();
        for failed_idx in &failures {
            in_flight.remove(failed_idx);
        }

        let completions: Vec<(usize, Vec<DamageRect>)> = target_state.completed.drain(..).collect();
        for (buf_idx, damage) in &completions {
            in_flight.remove(buf_idx);
            let host_buf = &buffers[*buf_idx].host_buffer;
            host_surface.attach(Some(host_buf), 0, 0);
            // Viewport destination persists; it is only updated on host
            // configure (resize), never per frame.
            match coalesce_damage(damage, buffer_width, buffer_height) {
                DamagePlan::Full => {
                    host_surface.damage_buffer(0, 0, buffer_width as i32, buffer_height as i32);
                }
                DamagePlan::Rects(rects) => {
                    for rect in rects {
                        host_surface.damage_buffer(rect.x, rect.y, rect.w, rect.h);
                    }
                }
            }
            host_surface.frame(&host_qh, ());
            host_surface.commit();
            host_state.buffer_busy[*buf_idx] = true;
            host_state.frame_done = false;
            host_conn.flush()?;
        }

        if !failures.is_empty() && completions.is_empty() {
            std::thread::sleep(Duration::from_millis(16));
        }
    }

    println!("[nori-view] Shutting down Zero-Copy DMA-BUF Bridge");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<Option<String>> {
        list.iter().map(|s| Some(s.to_string())).collect()
    }

    #[test]
    fn dnd_parser_splits_lines_and_data_frames() {
        let mut parser = DndSockParser::default();
        // Two JSON lines in one chunk.
        let events = parser.push(
            b"{\"event\":\"watching\",\"gen\":1}\n{\"event\":\"drag_started\",\"gen\":2,\"mimes\":[\"text/plain\"]}\n",
        );
        assert_eq!(
            events,
            vec![
                DndSockEvent::Watching { gen: 1 },
                DndSockEvent::DragStarted {
                    gen: 2,
                    mimes: vec!["text/plain".to_string()],
                },
            ]
        );
        // Data header split across chunks, then split payload.
        let events = parser.push(b"{\"event\":\"data\",\"id\":7,\"mime\":\"text/plain\",\"len\":10}\nhello");
        assert!(events.is_empty());
        let events = parser.push(b" worl\n{\"event\":\"pong\"}\n");
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            DndSockEvent::Data {
                id: 7,
                mime: "text/plain".to_string(),
                bytes: b"hello worl".to_vec(),
            }
        );
        assert_eq!(events[1], DndSockEvent::Pong);
    }

    #[test]
    fn dnd_parser_external_events() {
        let mut parser = DndSockParser::default();
        let events = parser.push(
            b"{\"event\":\"external_started\",\"ok\":true}\n{\"event\":\"need_data\",\"mime\":\"text/html\"}\n",
        );
        assert_eq!(
            events,
            vec![
                DndSockEvent::ExternalStarted { ok: true },
                DndSockEvent::NeedData {
                    mime: "text/html".to_string(),
                },
            ]
        );
    }

    #[test]
    fn dnd_parser_skips_garbage_lines() {
        let mut parser = DndSockParser::default();
        let events = parser.push(b"not json\n{\"event\":\"pong\"}\n");
        assert_eq!(events, vec![DndSockEvent::Pong]);
    }


    fn cursor_pixel(buf: &[u8], x: i32, y: i32) -> [u8; 4] {
        let px = &buf[(y * cursor::ICON_STRIDE + x * 4) as usize..][..4];
        [px[0], px[1], px[2], px[3]]
    }

    #[test]
    fn cursor_composer_idle_has_arrow_and_numeral() {
        let spec = cursor::CursorSpec::idle(1, cursor::ACCENTS[0].1);
        let buf = cursor::compose_cursor(&spec);
        assert_eq!(buf.len(), (cursor::ICON_SIZE * cursor::ICON_STRIDE) as usize);
        assert_eq!(cursor_pixel(&buf, 63, 0), [0, 0, 0, 0]);
        assert_eq!(cursor_pixel(&buf, 8, 12), [255, 255, 255, 255]);
        // Numeral badge rim is accent; digit stroke is white.
        assert_eq!(cursor_pixel(&buf, 50, 4), cursor::ACCENTS[0].1);
        assert_eq!(cursor_pixel(&buf, 50, 12), [255, 255, 255, 255]);
        // Different heads differ.
        let other = cursor::compose_cursor(&cursor::CursorSpec::idle(2, cursor::ACCENTS[0].1));
        assert_ne!(buf, other);
    }

    #[test]
    fn cursor_composer_file_drag_shows_glyph_and_badges() {
        let spec = cursor::CursorSpec {
            numeral: 1,
            accent: cursor::ACCENTS[0].1,
            files: vec![cursor::FileClass::Image],
            file_count: 1,
            drag: Some(cursor::DragBadge::Neutral),
        };
        let buf = cursor::compose_cursor(&spec);
        // Page body (below the band, away from symbols).
        assert_eq!(cursor_pixel(&buf, 12, 30), [255, 255, 255, 255]);
        // State badge present.
        assert_eq!(cursor_pixel(&buf, 52, 52), [255, 130, 70, 255]);
        // Numeral still visible.
        assert_eq!(cursor_pixel(&buf, 50, 4), cursor::ACCENTS[0].1);
    }

    #[test]
    fn uri_classifier_maps_common_types() {
        use cursor::{classify_uri, FileClass};
        assert_eq!(classify_uri("file:///tmp/photo.PNG"), FileClass::Image);
        assert_eq!(classify_uri("file:///v/clip.mkv"), FileClass::Video);
        assert_eq!(classify_uri("file:///m/song.flac"), FileClass::Audio);
        assert_eq!(classify_uri("file:///d/paper.pdf"), FileClass::Pdf);
        assert_eq!(classify_uri("file:///d/notes.md"), FileClass::Doc);
        assert_eq!(classify_uri("file:///d/a.tar.gz"), FileClass::Archive);
        assert_eq!(classify_uri("file:///s/main.rs"), FileClass::Code);
        assert_eq!(classify_uri("file:///home/u/"), FileClass::Folder);
        assert_eq!(classify_uri("https://example.com/x"), FileClass::Link);
        assert_eq!(classify_uri("file:///tmp/README"), FileClass::Generic);
        let (classes, count) = cursor::classify_uris(
            "file:///a.png\n# comment\n\nfile:///b.pdf\nfile:///c/\nfile:///d.mp3\n",
        );
        assert_eq!(count, 4);
        assert_eq!(
            classes,
            vec![FileClass::Image, FileClass::Pdf, FileClass::Folder]
        );
    }

    #[test]
    fn accent_selection_prefers_explicit_then_node() {
        assert_eq!(
            cursor::parse_accent("green"),
            Some(cursor::ACCENTS[1].1)
        );
        assert_eq!(cursor::parse_accent("#ff0000"), Some([0, 0, 255, 255]));
        assert_eq!(cursor::parse_accent("bogus"), None);
        assert_eq!(cursor::accent_for_node("local"), cursor::ACCENTS[0].1);
        // Unknown nodes are stable across calls.
        assert_eq!(
            cursor::accent_for_node("node-east"),
            cursor::accent_for_node("node-east")
        );
    }

    #[test]
    fn cursor_spec_tracks_drag_files() {
        let mut state = HostState {
            compositor: None,
            xdg_wm_base: None,
            linux_dmabuf: None,
            dmabuf_feedback: FeedbackCollector::default(),
            dmabuf_feedback_obj: None,
            seat: None,
            inhibit_manager: None,
            inhibitor: None,
            viewporter: None,
            viewport: None,
            surface: None,
            xdg_surface: None,
            toplevel: None,
            pointer: None,
            keyboard: None,
            touch: None,
            configured: false,
            width: 0,
            height: 0,
            pointer_events: Vec::new(),
            keyboard_events: Vec::new(),
            pending_axis_source: wl_pointer::AxisSource::Wheel,
            last_axis_value: [0.0; 2],
            held_keys: HashSet::new(),
            inhibitor_active: false,
            mod_held: false,
            shift_held: false,
            buffer_busy: Vec::new(),
            frame_done: true,
            is_fullscreen: false,
            is_focused: true,
            last_unfocused_capture: None,
            unfocused_since: None,
            capture_halted: false,
            closed: false,
            dnd: DndBridge::default(),
        };
        state.dnd.head_num = 1;
        state.dnd.accent = cursor::ACCENTS[0].1;
        assert_eq!(state.cursor_spec().drag, None);
        assert!(state.cursor_spec().files.is_empty());
        // Live internal URI drag -> file composition.
        state.dnd.pointer_inside = true;
        state.dnd.button_held = true;
        let mut cache = HashMap::new();
        cache.insert(
            "text/uri-list".to_string(),
            b"file:///tmp/a.png\n".to_vec(),
        );
        state.dnd.drag = Some(SlotDrag {
            cache,
            ..SlotDrag::default()
        });
        let spec = state.cursor_spec();
        assert_eq!(spec.drag, Some(cursor::DragBadge::Neutral));
        assert_eq!(spec.files, vec![cursor::FileClass::Image]);
        // Escaped: cursor returns to idle (the icon carries the drag).
        state.dnd.drag.as_mut().unwrap().escaped = true;
        let spec = state.cursor_spec();
        assert_eq!(spec.drag, None);
        assert!(spec.files.is_empty());
    }

    #[test]
    fn pump_reconnects_dead_socket() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("nori-dnd-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("nori-dnd-9.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        // Bridge with no connection but a known path: first pump connects.
        let mut bridge = DndBridge::default();
        bridge.sock_path = Some(path.clone());
        bridge.pump();
        assert!(bridge.sock.is_some());
        assert!(bridge.writer.is_some());
        // The slot side would accept two connections (reader + writer).
        listener.set_nonblocking(true).unwrap();
        let mut accepted = 0;
        for _ in 0..50 {
            match listener.accept() {
                Ok((stream, _)) => {
                    accepted += 1;
                    drop(stream);
                    if accepted == 2 {
                        break;
                    }
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        assert_eq!(accepted, 2);
        // A dead connection schedules a retry instead of disabling forever.
        bridge.sock = None;
        bridge.retry_cooldown = 0;
        bridge.pump();
        assert!(bridge.sock.is_some());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parse_slot_from_display_handles_paths() {
        assert_eq!(parse_slot_from_display("wayland-nori-1"), Some(1));
        assert_eq!(
            parse_slot_from_display("/run/user/1000/wayland-nori-2"),
            Some(2)
        );
        assert_eq!(parse_slot_from_display("wayland-0"), Some(0));
        assert_eq!(parse_slot_from_display("nope"), None);
    }

    #[test]
    fn select_output_prefers_exact_match() {
        let list = names(&["headless-1", "headless-2"]);
        assert_eq!(select_output_index(&list, "headless-2"), Some(1));
        assert_eq!(select_output_index(&list, "headless-1"), Some(0));
    }

    #[test]
    fn select_output_rejects_unknown_on_multi_output_slot() {
        let list = names(&["headless-1", "headless-2"]);
        assert_eq!(select_output_index(&list, "headless-3"), None);
        assert_eq!(select_output_index(&list, ""), None);
    }

    #[test]
    fn select_output_falls_back_to_lone_unnamed_output() {
        assert_eq!(select_output_index(&[None], "headless-1"), Some(0));
    }

    #[test]
    fn select_output_rejects_mismatch_on_named_single_output() {
        let list = names(&["headless-1"]);
        assert_eq!(select_output_index(&list, "headless-2"), None);
        assert_eq!(select_output_index(&[], "headless-1"), None);
    }

    #[test]
    fn pick_free_buffer_rotates_and_skips_busy() {
        let busy = [false, true, false, true];
        let in_flight: HashSet<usize> = [2].into_iter().collect();
        // next=0 -> 0 is free.
        assert_eq!(pick_free_buffer(4, 0, |i| busy[i] || in_flight.contains(&i)), Some(0));
        // next=1 -> 1 host-busy, 2 in-flight, 3 host-busy, wraps to 0.
        assert_eq!(pick_free_buffer(4, 1, |i| busy[i] || in_flight.contains(&i)), Some(0));
    }

    #[test]
    fn pick_free_buffer_returns_none_when_all_busy() {
        let busy = [true, true];
        let in_flight: HashSet<usize> = HashSet::new();
        assert_eq!(pick_free_buffer(2, 0, |i| busy[i] || in_flight.contains(&i)), None);
        assert_eq!(pick_free_buffer(0, 0, |_| false), None);
    }

    #[test]
    fn coalesce_damage_forwards_small_rects() {
        let damage = vec![
            DamageRect { x: 0, y: 0, w: 100, h: 100 },
            DamageRect { x: 500, y: 500, w: 64, h: 64 },
        ];
        match coalesce_damage(&damage, 1920, 1200) {
            DamagePlan::Rects(rects) => assert_eq!(rects.len(), 2),
            DamagePlan::Full => panic!("small damage should forward rects"),
        }
    }

    #[test]
    fn coalesce_damage_assumes_full_on_unknown_damage() {
        match coalesce_damage(&[], 1920, 1200) {
            DamagePlan::Full => (),
            DamagePlan::Rects(_) => panic!("empty damage must assume full"),
        }
    }

    #[test]
    fn coalesce_damage_collapses_large_or_many_rects() {
        let full = vec![DamageRect { x: 0, y: 0, w: 1920, h: 1200 }];
        assert!(matches!(coalesce_damage(&full, 1920, 1200), DamagePlan::Full));

        let many: Vec<DamageRect> = (0..17).map(|i| DamageRect { x: i * 10, y: 0, w: 4, h: 4 }).collect();
        assert!(matches!(coalesce_damage(&many, 1920, 1200), DamagePlan::Full));
    }

    fn table_bytes(entries: &[(u32, u64)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (fourcc, modifier) in entries {
            bytes.extend_from_slice(&fourcc.to_ne_bytes());
            bytes.extend_from_slice(&[0u8; 4]);
            bytes.extend_from_slice(&modifier.to_ne_bytes());
        }
        bytes
    }

    #[test]
    fn parse_format_table_reads_entries_and_skips_padding() {
        let bytes = table_bytes(&[(0x34325258, 0), (0x34325241, 0x0100000000000001)]);
        assert_eq!(
            parse_format_table(&bytes),
            vec![(0x34325258, 0), (0x34325241, 0x0100000000000001)]
        );
        assert!(parse_format_table(&[]).is_empty());
        assert!(parse_format_table(&[1u8; 15]).is_empty());
    }

    #[test]
    fn parse_tranche_indices_reads_u16_list() {
        assert_eq!(parse_tranche_indices(&[0u8, 0, 3, 0, 44, 2]), vec![0, 3, 556]);
        assert!(parse_tranche_indices(&[]).is_empty());
    }

    fn feedback(pairs: &[Vec<(u32, u64)>]) -> DmabufFeedback {
        DmabufFeedback {
            tranches: pairs
                .iter()
                .map(|formats| FeedbackTranche { scanout: false, formats: formats.clone() })
                .collect(),
            done: true,
        }
    }

    #[test]
    fn negotiate_prefers_scanout_then_tiled_over_linear() {
        let xrgb = Format::Xrgb8888 as u32;
        let host = feedback(&[vec![(xrgb, 0), (xrgb, 9), (xrgb, 3)]]);
        // LINEAR listed first still loses to a tiled modifier.
        let target = feedback(&[vec![(xrgb, 0), (xrgb, 9)]]);
        assert_eq!(negotiate_buffer_format(&target, &host), Some((xrgb, 9)));
        // A scanout tranche wins even when ordered later.
        let target_scanout = DmabufFeedback {
            tranches: vec![
                FeedbackTranche { scanout: false, formats: vec![(xrgb, 9)] },
                FeedbackTranche { scanout: true, formats: vec![(xrgb, 3)] },
            ],
            done: true,
        };
        assert_eq!(negotiate_buffer_format(&target_scanout, &host), Some((xrgb, 3)));
        // LINEAR is the last resort, not a failure.
        let target_linear = feedback(&[vec![(xrgb, 0)]]);
        let host_linear = feedback(&[vec![(xrgb, 0)]]);
        assert_eq!(negotiate_buffer_format(&target_linear, &host_linear), Some((xrgb, 0)));
    }

    #[test]
    fn negotiate_skips_unallocatable_and_host_missing_pairs() {
        let xrgb = Format::Xrgb8888 as u32;
        let argb = Format::Argb8888 as u32;
        // ARGB8888 is not allocatable: the screencopy server requires XRGB8888.
        let target = feedback(&[vec![(argb, 1), (xrgb, 5)]]);
        let host = feedback(&[vec![(argb, 1)]]);
        assert_eq!(negotiate_buffer_format(&target, &host), None);

        let host2 = feedback(&[vec![(xrgb, 5)]]);
        assert_eq!(negotiate_buffer_format(&target, &host2), Some((xrgb, 5)));
    }

    #[test]
    fn negotiate_skips_invalid_wildcard_modifier() {
        let xrgb = Format::Xrgb8888 as u32;
        let target = feedback(&[vec![(xrgb, MOD_INVALID)]]);
        let host = feedback(&[vec![(xrgb, MOD_INVALID), (xrgb, 0)]]);
        assert_eq!(negotiate_buffer_format(&target, &host), None);
    }

    #[test]
    fn negotiate_requires_finished_feedback_on_both_sides() {
        let xrgb = Format::Xrgb8888 as u32;
        let done = feedback(&[vec![(xrgb, 0)]]);
        let pending = DmabufFeedback { tranches: done.tranches.clone(), done: false };
        assert_eq!(negotiate_buffer_format(&done, &done), Some((xrgb, 0)));
        assert_eq!(negotiate_buffer_format(&pending, &done), None);
        assert_eq!(negotiate_buffer_format(&done, &pending), None);
    }

    #[test]
    fn collector_assembles_tranches_from_events() {
        let mut collector = FeedbackCollector::default();
        collector.push_table(vec![(0x34325258, 0), (0x34325258, 8)]);
        collector.push_tranche_flags(true);
        collector.push_tranche_formats(&[1, 0, 9]);
        collector.end_tranche();
        collector.finish();
        let snapshot = collector.snapshot();
        assert!(snapshot.done);
        assert_eq!(snapshot.tranches.len(), 1);
        assert!(snapshot.tranches[0].scanout);
        // Out-of-range index 9 is dropped.
        assert_eq!(snapshot.tranches[0].formats, vec![(0x34325258, 8), (0x34325258, 0)]);
    }
}
