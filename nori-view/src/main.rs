use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use gbm::{BufferObject, BufferObjectFlags, Device, Format, Modifier};

use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_callback::WlCallback,
    wl_compositor::WlCompositor,
    wl_keyboard::{self, KeyState, WlKeyboard},
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
    held_keys: HashSet<u32>,
    inhibitor_active: bool,
    mod_held: bool,
    shift_held: bool,
    buffer_busy: Vec<bool>,
    frame_done: bool,
    is_fullscreen: bool,
    is_focused: bool,
    last_unfocused_capture: Option<std::time::Instant>,
    closed: bool,
}

impl HostState {
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
    Axis { time: u32, axis: wl_pointer::Axis, value: f64 },
    Frame,
}

#[derive(Clone, Debug)]
enum KeyboardEvent {
    Key { time: u32, key: u32, state: u32 },
    Modifiers { depressed: u32, latched: u32, locked: u32, group: u32 },
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
    fn event(state: &mut Self, _: &WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_pointer::Event::Motion { time, surface_x, surface_y } => {
                state.pointer_events.push(PointerEvent::MotionAbsolute {
                    time,
                    x: surface_x,
                    y: surface_y,
                });
            }
            wl_pointer::Event::Button { time, button, state: bstate, .. } => {
                if let WEnum::Value(bstate) = bstate {
                    state.pointer_events.push(PointerEvent::Button {
                        time,
                        button,
                        state: bstate,
                    });
                }
            }
            wl_pointer::Event::Axis { time, axis, value } => {
                if let WEnum::Value(axis) = axis {
                    state.pointer_events.push(PointerEvent::Axis { time, axis, value });
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
        held_keys: HashSet::new(),
        inhibitor_active: true,
        mod_held: false,
        shift_held: false,
        buffer_busy: Vec::new(),
        frame_done: true,
        is_fullscreen: false,
        is_focused: true,
        last_unfocused_capture: None,
        closed: false,
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
    let mut in_flight: HashMap<usize, ZwlrScreencopyFrameV1> = HashMap::new();
    let mut next_buffer_idx = 0;

    // Zero-Copy Streaming Loop
    while !host_state.closed {
        // Dispatch host events (user input, window resizes)
        host_queue.dispatch_pending(&mut host_state)?;

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
                    PointerEvent::Axis { time, axis, value } => {
                        vp.axis_source(wayland_client::protocol::wl_pointer::AxisSource::Wheel);
                        vp.axis(time, axis, value);
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
        // - When unfocused: throttle capture to standby rate (5 FPS = every 200ms) to let slot sleep
        let now = std::time::Instant::now();
        let should_capture = if host_state.is_focused {
            true
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
