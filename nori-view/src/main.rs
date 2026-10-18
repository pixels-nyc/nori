use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use gbm::{BufferObject, BufferObjectFlags, Device, Format};

use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_compositor::WlCompositor,
    wl_keyboard::{self, KeyState, WlKeyboard},
    wl_output::{self, WlOutput},
    wl_pointer::{self, ButtonState, WlPointer},
    wl_registry::{self, WlRegistry},
    wl_seat::{self, WlSeat},
    wl_surface::WlSurface,
    wl_touch::{self, WlTouch},
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};

use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
    zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
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

    /// Fullscreen mode on host
    #[arg(short, long)]
    fullscreen: bool,

    /// DRM render node path
    #[arg(long, default_value = "/dev/dri/renderD128")]
    drm_node: PathBuf,
}

struct DmaBuffer {
    id: usize,
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
    inhibitor_active: bool,
    mod_held: bool,
    shift_held: bool,
    is_fullscreen: bool,
    closed: bool,
}

#[derive(Clone, Debug)]
enum PointerEvent {
    MotionAbsolute { time: u32, x: f64, y: f64 },
    Button { time: u32, button: u32, state: ButtonState },
    Axis { time: u32, axis: wl_pointer::Axis, value: f64 },
    Frame,
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
            xdg_toplevel::Event::Configure { width, height, .. } => {
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
    fn event(state: &mut Self, _: &WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match event {
            wl_keyboard::Event::Key { key, state: kstate, .. } => {
                if let WEnum::Value(kstate) = kstate {
                    let is_pressed = kstate == KeyState::Pressed;

                    // Direct keycode tracking for Super (125/126) and Shift (42/54)
                    if key == 125 || key == 126 {
                        state.mod_held = is_pressed;
                    } else if key == 42 || key == 54 {
                        state.shift_held = is_pressed;
                    }

                    // Key 33 (F) or Key 87 (F11): Mod+Shift+F toggle fullscreen
                    if (((key == 33 || key == 41) && state.mod_held && state.shift_held) || key == 87) && is_pressed {
                        state.is_fullscreen = !state.is_fullscreen;
                        if let Some(toplevel) = &state.toplevel {
                            if state.is_fullscreen {
                                toplevel.set_fullscreen(None);
                                println!("[nori-view] Mod+Shift+F: Toggled Fullscreen ON");
                            } else {
                                toplevel.unset_fullscreen();
                                println!("[nori-view] Mod+Shift+F: Toggled Fullscreen OFF");
                            }
                            if let Some(surface) = &state.surface {
                                surface.commit();
                            }
                        }
                        return;
                    }

                    // Key 1 is KEY_ESC: Mod+Esc escape chord: toggle shortcuts inhibitor
                    if key == 1 && state.mod_held && is_pressed {
                        state.inhibitor_active = !state.inhibitor_active;
                        if let Some(inhibitor) = &state.inhibitor {
                            if !state.inhibitor_active {
                                inhibitor.destroy();
                                state.inhibitor = None;
                                println!("[nori-view] Focus Escape: Host navigation unlocked (shortcuts enabled)");
                            }
                        } else if state.inhibitor_active {
                            if let (Some(inhibit_mgr), Some(seat), Some(surface)) = (&state.inhibit_manager, &state.seat, &state.surface) {
                                state.inhibitor = Some(inhibit_mgr.inhibit_shortcuts(surface, seat, qh, ()));
                                println!("[nori-view] Focus Lock: Shortcuts inhibited for slot");
                            }
                        }
                        return;
                    }
                }
            }
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                state.shift_held = (mods_depressed & 0x01) != 0;
                state.mod_held = (mods_depressed & 0x40) != 0 || (mods_depressed & 0x08) != 0;
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

impl Dispatch<ZwpLinuxBufferParamsV1, usize> for HostState {
    fn event(_: &mut Self, _: &ZwpLinuxBufferParamsV1, _: zwp_linux_buffer_params_v1::Event, _: &usize, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlBuffer, usize> for HostState {
    fn event(_: &mut Self, _: &WlBuffer, _: wayland_client::protocol::wl_buffer::Event, _: &usize, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpKeyboardShortcutsInhibitManagerV1, ()> for HostState {
    fn event(_: &mut Self, _: &ZwpKeyboardShortcutsInhibitManagerV1, _: wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for HostState {
    fn event(_: &mut Self, _: &ZwpKeyboardShortcutsInhibitorV1, _: wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
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

struct TargetState {
    output: Option<WlOutput>,
    output_name: String,
    screencopy_manager: Option<ZwlrScreencopyManagerV1>,
    linux_dmabuf: Option<ZwpLinuxDmabufV1>,
    virtual_pointer_mgr: Option<ZwlrVirtualPointerManagerV1>,
    virtual_pointer: Option<ZwlrVirtualPointerV1>,
    virtual_keyboard_mgr: Option<ZwpVirtualKeyboardManagerV1>,
    virtual_keyboard: Option<ZwpVirtualKeyboardV1>,

    width: u32,
    height: u32,
    ready_to_capture: bool,
    frame_ready: bool,
    frame_failed: bool,
    active_frame_buffer_idx: Option<usize>,
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
                "wl_output" => {
                    let out = registry.bind(name, version.min(4), qh, ());
                    if state.output.is_none() {
                        state.output = Some(out);
                    }
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

impl Dispatch<WlOutput, ()> for TargetState {
    fn event(state: &mut Self, _: &WlOutput, event: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_output::Event::Mode { width, height, flags, .. } => {
                if let WEnum::Value(flags) = flags {
                    if flags.contains(wl_output::Mode::Current) || state.width == 0 {
                        state.width = width as u32;
                        state.height = height as u32;
                        state.ready_to_capture = true;
                    }
                }
            }
            wl_output::Event::Name { name } => {
                if name == state.output_name {
                    // Matched desired output
                }
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
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.frame_ready = true;
                state.active_frame_buffer_idx = Some(*buffer_idx);
                frame.destroy();
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.frame_failed = true;
                frame.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for TargetState {
    fn event(_: &mut Self, _: &ZwpLinuxDmabufV1, _: wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
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
        output: None,
        output_name: args.output.clone(),
        screencopy_manager: None,
        linux_dmabuf: None,
        virtual_pointer_mgr: None,
        virtual_pointer: None,
        virtual_keyboard_mgr: None,
        virtual_keyboard: None,
        width: 1920,
        height: 1080,
        ready_to_capture: false,
        frame_ready: false,
        frame_failed: false,
        active_frame_buffer_idx: None,
    };

    let _target_registry = target_conn.display().get_registry(&target_qh, ());
    target_queue.roundtrip(&mut target_state)?;
    target_queue.roundtrip(&mut target_state)?;

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
        width: (target_state.width as f64 / 1.5).round() as u32,
        height: (target_state.height as f64 / 1.5).round() as u32,
        pointer_events: Vec::new(),
        inhibitor_active: true,
        mod_held: false,
        shift_held: false,
        is_fullscreen: false,
        closed: false,
    };

    let _host_registry = host_conn.display().get_registry(&host_qh, ());
    host_queue.roundtrip(&mut host_state)?;
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

    if args.fullscreen {
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

    // Allocate 3 Zero-Copy DMA-BUF Buffers (Triple Buffering)
    let buffer_width = target_state.width;
    let buffer_height = target_state.height;
    let target_dmabuf = target_state.linux_dmabuf.as_ref().context("target lacks zwp_linux_dmabuf_v1")?;
    let host_dmabuf = host_state.linux_dmabuf.as_ref().context("host lacks zwp_linux_dmabuf_v1")?;

    let mut buffers: Vec<DmaBuffer> = Vec::new();

    for id in 0..3 {
        let bo = gbm_dev
            .create_buffer_object::<()>(
                buffer_width,
                buffer_height,
                Format::Xrgb8888,
                BufferObjectFlags::RENDERING | BufferObjectFlags::LINEAR,
            )
            .context("error creating GBM buffer object")?;

        let prime_fd = bo.fd().context("error exporting BO fd")?;
        let stride = bo.stride();
        let modifier = u64::from(bo.modifier());
        let modifier_hi = (modifier >> 32) as u32;
        let modifier_lo = (modifier & 0xFFFFFFFF) as u32;
        let fourcc = Format::Xrgb8888 as u32;

        // Target buffer
        let target_params = target_dmabuf.create_params(&target_qh, id);
        target_params.add(prime_fd.as_fd(), 0, 0, stride, modifier_hi, modifier_lo);
        let target_wl_buf = target_params.create_immed(
            buffer_width as i32,
            buffer_height as i32,
            fourcc,
            zwp_linux_buffer_params_v1::Flags::empty(),
            &target_qh,
            id,
        );

        // Host buffer (dup the fd so each compositor owns its descriptor)
        let dup_fd = nix::unistd::dup(&prime_fd)
            .context("error duplicating prime fd")?;
        let host_params = host_dmabuf.create_params(&host_qh, id);
        host_params.add(dup_fd.as_fd(), 0, 0, stride, modifier_hi, modifier_lo);
        let host_wl_buf = host_params.create_immed(
            buffer_width as i32,
            buffer_height as i32,
            fourcc,
            zwp_linux_buffer_params_v1::Flags::empty(),
            &host_qh,
            id,
        );

        buffers.push(DmaBuffer {
            id,
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
    let target_output = target_state.output.clone().context("target lacks output")?;
    let virtual_pointer = target_state.virtual_pointer.clone();

    let mut current_frame: Option<ZwlrScreencopyFrameV1> = None;
    let mut next_buffer_idx = 0;

    // Zero-Copy Streaming Loop
    while !host_state.closed {
        // Dispatch host events (user input, window resizes)
        host_queue.dispatch_pending(&mut host_state)?;

        // Forward input events from host to target
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
                        vp.axis(time, axis, value);
                    }
                    PointerEvent::Frame => {
                        vp.frame();
                    }
                }
            }
            target_conn.flush()?;
        }

        // If no capture is currently inflight, request a new frame using the next available DMA-BUF
        if current_frame.is_none() {
            let buf_idx = next_buffer_idx;
            next_buffer_idx = (next_buffer_idx + 1) % buffers.len();

            let target_buf = &buffers[buf_idx].target_buffer;
            let frame = screencopy_mgr.capture_output(0, &target_output, &target_qh, buf_idx);
            frame.copy_with_damage(target_buf);
            current_frame = Some(frame);
            target_conn.flush()?;
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
            let _ = nix::poll::poll(&mut poll_fds, nix::poll::PollTimeout::from(16u16));
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

        if target_state.frame_ready {
            target_state.frame_ready = false;
            current_frame = None;

            if let Some(buf_idx) = target_state.active_frame_buffer_idx.take() {
                let host_buf = &buffers[buf_idx].host_buffer;
                host_surface.attach(Some(host_buf), 0, 0);
                if let Some(vp) = &host_state.viewport {
                    vp.set_destination(host_state.width as i32, host_state.height as i32);
                }
                host_surface.damage_buffer(0, 0, buffer_width as i32, buffer_height as i32);
                host_surface.commit();
                host_conn.flush()?;
            }
        } else if target_state.frame_failed {
            target_state.frame_failed = false;
            current_frame = None;
            std::thread::sleep(Duration::from_millis(16));
        }
    }

    println!("[nori-view] Shutting down Zero-Copy DMA-BUF Bridge");
    Ok(())
}
