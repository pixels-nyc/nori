//! Headless backend for tests and persistent multi-tenant session spaces.

use std::collections::HashMap;
use std::mem;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use niri_config::OutputName;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::drm::DrmNode;
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::element::RenderElementStates;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{ImportDma, ImportEgl};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::utils::{DeviceFd, Size};
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal};
use smithay::wayland::presentation::Refresh;

use super::{IpcOutputMap, OutputId, RenderResult};
use crate::handlers::image_copy_capture;
use crate::niri::{Niri, RedrawState};
use crate::render_helpers::{resources, shaders};
use crate::utils::{get_monotonic_time, logical_output};

fn detect_physical_displays() -> Vec<(u16, u16)> {
    // Check manual override first: NORI_HEADS or NORI_HEADLESS_HEADS
    if let Ok(heads_str) = std::env::var("NORI_HEADS").or_else(|_| std::env::var("NORI_HEADLESS_HEADS")) {
        if let Ok(count) = heads_str.trim().parse::<usize>() {
            if count > 0 {
                return vec![(1920, 1080); count];
            }
        }
    }

    let mut displays = Vec::new();
    let drm_path = std::path::Path::new("/sys/class/drm");
    if let Ok(entries) = std::fs::read_dir(drm_path) {
        let mut conn_entries: Vec<std::path::PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                name.starts_with("card") && name.contains('-')
            })
            .collect();
        conn_entries.sort();

        for conn_dir in conn_entries {
            let conn_name = conn_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");

            // Filter out Virtual and Writeback connectors (from vkms or faux drivers)
            if conn_name.contains("Virtual") || conn_name.contains("Writeback") {
                continue;
            }

            // Check if underlying device is vkms or faux
            if let Ok(dev_target) = std::fs::read_link(conn_dir.join("device")) {
                let dev_str = dev_target.to_string_lossy();
                if dev_str.contains("vkms") || dev_str.contains("faux") {
                    continue;
                }
            }

            // Check if status is "connected"
            let status_path = conn_dir.join("status");
            if let Ok(status) = std::fs::read_to_string(status_path) {
                if status.trim() == "connected" {
                    let mut res = (1920, 1080);
                    let modes_path = conn_dir.join("modes");
                    if let Ok(modes_str) = std::fs::read_to_string(modes_path) {
                        if let Some(first_mode) = modes_str.lines().next() {
                            let parts: Vec<&str> = first_mode.trim().split('x').collect();
                            if parts.len() == 2 {
                                if let (Ok(w), Ok(h)) = (parts[0].parse::<u16>(), parts[1].parse::<u16>()) {
                                    res = (w, h);
                                }
                            }
                        }
                    }
                    displays.push(res);
                }
            }
        }
    }

    if displays.is_empty() {
        displays.push((1920, 1080));
    }
    displays
}

/// Standby drain rate for unviewed headless outputs.
fn standby_fps() -> u64 {
    std::env::var("NORI_STANDBY_FPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5)
}

/// How long a file-trigger read stays cached. The redraw pump
/// evaluates demand per iteration; without a TTL a missing file costs
/// an openat per iteration (~3k/s storm, 2026-10-23). 250ms keeps
/// burst/away reaction time under one standby tick at 5Hz.
const TRIGGER_CACHE_TTL: Duration = Duration::from_millis(250);

#[derive(Debug, PartialEq, Eq)]
enum BurstState {
    Live(Option<u64>),
    Expired,
    Invalid,
}

/// Parses an agent burst trigger file: "<expires_epoch_millis> [target_fps]".
fn parse_burst_trigger(content: &str, now_ms: u128) -> BurstState {
    let mut parts = content.split_whitespace();
    let Some(exp_str) = parts.next() else {
        return BurstState::Invalid;
    };
    let Ok(exp_ms) = exp_str.parse::<u128>() else {
        return BurstState::Invalid;
    };
    if now_ms >= exp_ms {
        return BurstState::Expired;
    }
    let fps = parts.next().and_then(|s| s.parse::<u64>().ok());
    BurstState::Live(fps)
}

/// Slot identity for the trigger/hold files, e.g. "1".
fn slot_str() -> Option<String> {
    std::env::var("NORI_HEADLESS_SLOT")
        .or_else(|_| std::env::var("PIXOS_HEAD"))
        .ok()
}

/// Runtime dir for the trigger/hold files. Tests override via
/// `NORI_TEST_RUNTIME_DIR` so they never touch the live session.
fn trigger_runtime_dir() -> String {
    if let Ok(dir) = std::env::var("NORI_TEST_RUNTIME_DIR") {
        return dir;
    }
    std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }))
}

fn trigger_path(kind: &str, slot: &str) -> String {
    format!("{}/nori-{}-{}", trigger_runtime_dir(), kind, slot)
}

/// Cached file-trigger read: at most one disk read per `ttl`, so the
/// redraw pump (which evaluates demand per iteration) never turns a
/// missing file into an openat storm. No latch: once `ttl` elapses the
/// file is re-read, so a newly written trigger takes effect within
/// `ttl` and a removed one clears within `ttl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CachedRead {
    /// Monotonic instant of the last disk read, if any.
    at: Option<std::time::Instant>,
    /// Last read outcome: file content, or None when missing/invalid.
    content: Option<TriggerContent>,
}

/// A trigger file's parsed payload: expiry + optional fps override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TriggerContent {
    expires_ms: u128,
    fps: Option<u64>,
}

impl CachedRead {
    fn new() -> Self {
        Self {
            at: None,
            content: None,
        }
    }

    /// Returns the live-or-cached content for `path`. `parse` maps
    /// file bytes to `Some(payload)` (live) or `None` (expired /
    /// invalid — and the file is removed when expired).
    fn get(
        &mut self,
        path: &str,
        now: std::time::Instant,
        now_ms: u128,
        ttl: Duration,
        parse: impl FnOnce(&str, u128) -> BurstState,
        on_expired: impl FnOnce(),
    ) -> Option<TriggerContent> {
        let fresh = self
            .at
            .is_some_and(|at| now.duration_since(at) < ttl);
        if !fresh {
            let content = std::fs::read_to_string(path).ok().and_then(|text| {
                match parse(&text, now_ms) {
                    BurstState::Live(fps) => text
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<u128>().ok())
                        .map(|expires_ms| TriggerContent { expires_ms, fps }),
                    BurstState::Expired => {
                        on_expired();
                        None
                    }
                    BurstState::Invalid => None,
                }
            });
            self.at = Some(now);
            self.content = content;
        }
        // The cached expiry is authoritative inside the TTL window: a
        // hold that expired since the read must not linger.
        self.content.filter(|c| now_ms < c.expires_ms)
    }
}

/// Reads the agent burst trigger file, removing it once expired.
///
/// Returns (burst active, fps override). `cache`/`ttl` bound the disk
/// read cadence; production callers pass the `Headless` caches with
/// `TRIGGER_CACHE_TTL`.
fn burst_trigger_inner(cache: &mut CachedRead, ttl: Duration) -> (bool, Option<u64>) {
    let Some(slot) = slot_str() else {
        return (false, None);
    };
    let path = trigger_path("burst", &slot);
    let now = std::time::Instant::now();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    match cache.get(&path, now, now_ms, ttl, parse_burst_trigger, || {
        let _ = std::fs::remove_file(&path);
    }) {
        Some(c) => (true, c.fps),
        None => (false, None),
    }
}

/// Reads the viewer-away hold file (inverse of the burst trigger).
///
/// Present + unexpired = force standby regardless of open casts (the
/// VNC viewer is connected but backgrounded — WO-NORI-002). Absent /
/// expired / invalid = current behavior. Expired files are removed.
/// No latch: evaluated per redraw like the burst check.
fn away_hold_active_inner(cache: &mut CachedRead, ttl: Duration) -> bool {
    let Some(slot) = slot_str() else {
        return false;
    };
    let path = trigger_path("away", &slot);
    let now = std::time::Instant::now();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    cache
        .get(&path, now, now_ms, ttl, parse_burst_trigger, || {
            let _ = std::fs::remove_file(&path);
        })
        .is_some()
}

pub struct Headless {
    renderer: Option<GlesRenderer>,
    render_node: Option<DrmNode>,
    dmabuf_global: Option<DmabufGlobal>,
    #[cfg(feature = "xdp-gnome-screencast")]
    gbm_device: Option<GbmDevice<DeviceFd>>,
    ipc_outputs: Arc<Mutex<IpcOutputMap>>,
    last_render_time: Option<Duration>,
    forced_active: bool,
    standby_last_send: HashMap<Output, Duration>,
    /// Cached file-trigger reads. The redraw pump evaluates demand
    /// per iteration; uncached reads turned a missing burst file into
    /// a ~3k/s openat storm on viewed-idle slots (2026-10-23). TTL
    /// keeps trigger latency bounded while killing the storm.
    burst_cache: CachedRead,
    away_cache: CachedRead,
}

impl Headless {
    pub fn new() -> Self {
        Self {
            renderer: None,
            render_node: None,
            dmabuf_global: None,
            #[cfg(feature = "xdp-gnome-screencast")]
            gbm_device: None,
            ipc_outputs: Default::default(),
            last_render_time: None,
            forced_active: false,
            standby_last_send: HashMap::new(),
            burst_cache: CachedRead::new(),
            away_cache: CachedRead::new(),
        }
    }

    pub fn init(&mut self, niri: &mut Niri) {
        if let Err(err) = self.add_renderer() {
            warn!("Headless add_renderer error: {err:#}");
        }

        if let Some(renderer) = &mut self.renderer {
            if let Err(err) = renderer.bind_wl_display(&niri.display_handle) {
                trace!("error binding legacy EGL to wl_display: {err}");
            }
        }

        if let Some(renderer) = &self.renderer {
            let default_feedback = || {
                let node = self.render_node.as_ref().context("no render node available")?;
                let primary_formats = renderer.dmabuf_formats();
                DmabufFeedbackBuilder::new(node.dev_id(), primary_formats)
                    .build()
                    .context("error building dmabuf feedback")
            };

            let dmabuf_global = match default_feedback() {
                Ok(feedback) => niri
                    .dmabuf_state
                    .create_global_with_default_feedback::<crate::niri::State>(&niri.display_handle, &feedback),
                Err(err) => {
                    debug!("failed building default dmabuf feedback, falling back to v3: {err:?}");
                    let primary_formats = renderer.dmabuf_formats();
                    niri.dmabuf_state
                        .create_global::<crate::niri::State>(&niri.display_handle, primary_formats)
                }
            };
            self.dmabuf_global = Some(dmabuf_global);
            info!("Headless nori successfully registered zwp_linux_dmabuf_v1 global");
        }

        // Dynamically auto-sense physical display topology from DRM KMS
        // Fallback to NORI_HEADS / NORI_HEADLESS_HEADS env if provided
        let displays = detect_physical_displays();
        info!(
            "Headless nori auto-detected {} physical display(s): {:?}",
            displays.len(),
            displays
        );
        for (idx, size) in displays.into_iter().enumerate() {
            let n = (idx + 1) as u8;
            self.add_output(niri, n, size);
        }
    }

    pub fn add_renderer(&mut self) -> anyhow::Result<()> {
        if self.renderer.is_some() {
            error!("add_renderer: renderer must not already exist");
            return Ok(());
        }

        // Try to bind the primary hardware DRM render node for zero-copy Linux-DMABUF
        let gbm_res = (|| -> anyhow::Result<(GbmDevice<DeviceFd>, DrmNode)> {
            let candidates = ["/dev/dri/renderD128", "/dev/dri/renderD129"];
            for path in candidates {
                if let Ok(file) = std::fs::File::options().read(true).write(true).open(path) {
                    if let Ok(node) = DrmNode::from_file(&file) {
                        let dev = GbmDevice::new(DeviceFd::from(OwnedFd::from(file)))?;
                        info!("Headless nori successfully bound hardware render node at {path}");
                        return Ok((dev, node));
                    }
                }
            }
            anyhow::bail!("no accessible hardware DRM render node found");
        })();

        let (mut renderer, render_node, _gbm_dev) = match gbm_res {
            Ok((dev, node)) => unsafe {
                let display = EGLDisplay::new(dev.clone()).context("error creating GBM EGL display")?;
                let context = EGLContext::new(&display).context("error creating EGL context from GBM")?;
                let gles = GlesRenderer::new(context).context("error creating GLES renderer from GBM")?;
                (gles, Some(node), Some(dev))
            },
            Err(err) => unsafe {
                warn!("Falling back to EGLSurfacelessDisplay for headless nori: {err:#}");
                let display = EGLDisplay::new(EGLSurfacelessDisplay).context("error creating EGL display")?;
                let context = EGLContext::new(&display).context("error creating EGL context")?;
                let gles = GlesRenderer::new(context).context("error creating renderer")?;
                (gles, None, None)
            },
        };

        resources::init(&mut renderer);
        shaders::init(&mut renderer);

        self.renderer = Some(renderer);
        self.render_node = render_node;
        #[cfg(feature = "xdp-gnome-screencast")]
        {
            self.gbm_device = _gbm_dev;
        }
        Ok(())
    }

    pub fn add_output(&mut self, niri: &mut Niri, n: u8, size: (u16, u16)) {
        let connector = format!("headless-{n}");
        let make = "niri".to_string();
        let model = "headless".to_string();
        let serial = n.to_string();

        let output = Output::new(
            connector.clone(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: make.clone(),
                model: model.clone(),
                serial_number: serial.clone(),
            },
        );

        let mode = Mode {
            size: Size::from((i32::from(size.0), i32::from(size.1))),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, None);
        output.set_preferred(mode);

        output.user_data().insert_if_missing(|| OutputName {
            connector,
            make: Some(make),
            model: Some(model),
            serial: Some(serial),
        });

        let physical_properties = output.physical_properties();
        self.ipc_outputs.lock().unwrap().insert(
            OutputId::next(),
            niri_ipc::Output {
                name: output.name(),
                make: physical_properties.make,
                model: physical_properties.model,
                serial: None,
                physical_size: None,
                modes: vec![niri_ipc::Mode {
                    width: size.0,
                    height: size.1,
                    refresh_rate: 60_000,
                    is_preferred: true,
                }],
                current_mode: Some(0),
                is_custom_mode: true,
                vrr_supported: false,
                vrr_enabled: false,
                logical: Some(logical_output(&output)),
                max_bpc: None,
            },
        );

        niri.add_output(output, None, false);
    }

    pub fn seat_name(&self) -> String {
        "headless".to_owned()
    }

    pub fn with_primary_renderer<T>(
        &mut self,
        f: impl FnOnce(&mut GlesRenderer) -> T,
    ) -> Option<T> {
        self.renderer.as_mut().map(f)
    }

    pub fn primary_render_node(&mut self) -> Option<DrmNode> {
        self.render_node.clone()
    }

    pub fn render(&mut self, niri: &mut Niri, output: &Output) -> RenderResult {
        let now = get_monotonic_time();

        // Dual-State Adaptive Frame Clock:
        // Standby = Default 1 Hz software tick (NORI_STANDBY_FPS) to ensure <0.1% CPU and bounded VRAM
        // Active = Default 60 Hz (NORI_ACTIVE_FPS) when viewer/screencopy (nori-view, waypipe, wayvnc) or agent burst is active
        let standby_fps: u64 = std::env::var("NORI_STANDBY_FPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        let mut active_fps: u64 = std::env::var("NORI_ACTIVE_FPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60);

        // Agent burst hold forces full rate (cached: the redraw pump
        // evaluates demand per iteration).
        let (burst_active, burst_fps) =
            burst_trigger_inner(&mut self.burst_cache, TRIGGER_CACHE_TTL);
        if burst_active {
            if let Some(fps) = burst_fps {
                active_fps = fps.max(1);
            }
        }

        let standby_interval = Duration::from_millis(1000 / standby_fps.max(1));
        let active_interval = Duration::from_millis(1000 / active_fps.max(1));

        // Active whenever THIS output has presentation demand — any capture
        // type (wlr-screencopy, ext-image-copy, PipeWire), burst, or forced.
        // Must match try_standby_redraw's gate: the old screencopy-only
        // check paced image-copy viewers (wayvnc) at the standby rate
        // while still running full scene passes — 1 fps at full CPU.
        let is_viewer_attached = self.output_has_viewer_demand(niri, output);
        let target_interval = if is_viewer_attached {
            active_interval
        } else {
            standby_interval
        };

        if let Some(last_time) = self.last_render_time {
            let elapsed = now.saturating_sub(last_time);
            if elapsed < target_interval {
                let remaining = target_interval - elapsed;
                let output_clone = output.clone();
                let output_state = niri.output_state.get_mut(output).unwrap();

                // If a redraw was queued within the pacing throttle window, pace it via an estimated VBlank timer.
                //
                // Viewed-idle sleep (2026-10-23): without this arm, a
                // viewed-but-quiet output re-enters render() on every
                // pump iteration at full scene-pass cost (~16%/core
                // busy-spin behind epoll_pwait(0)). Sleeping for the
                // remainder of the target interval restores the frame
                // clock the standby path already enjoys.
                if matches!(
                    output_state.redraw_state,
                    RedrawState::Queued
                        | RedrawState::WaitingForEstimatedVBlankAndQueued(_)
                ) {
                    let timer = Timer::from_duration(remaining);
                    let token = niri.event_loop.insert_source(timer, move |_, _, data| {
                        data.niri.queue_redraw(&output_clone);
                        TimeoutAction::Drop
                    }).ok();
                    if let Some(token) = token {
                        // Drop the superseded timer source (if any) so
                        // dead timers cannot accumulate on the loop.
                        if let RedrawState::WaitingForEstimatedVBlankAndQueued(old)
                        | RedrawState::WaitingForEstimatedVBlank(old) =
                            mem::replace(
                                &mut output_state.redraw_state,
                                RedrawState::WaitingForEstimatedVBlank(token),
                            )
                        {
                            niri.event_loop.remove(old);
                        }
                    }
                    return RenderResult::NoDamage;
                }
            }
        }

        self.last_render_time = Some(now);

        let states = RenderElementStates::default();
        let mut presentation_feedbacks = niri.take_presentation_feedbacks(output, &states);
        presentation_feedbacks.presented::<_, smithay::utils::Monotonic>(
            now,
            Refresh::Unknown,
            0,
            wp_presentation_feedback::Kind::empty(),
        );

        let output_state = niri.output_state.get_mut(output).unwrap();
        output_state.frame_clock.presented(now);

        match mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
            RedrawState::Idle => (),
            RedrawState::Queued => (),
            RedrawState::WaitingForVBlank { .. } => (),
            RedrawState::WaitingForEstimatedVBlank(_) => (),
            RedrawState::WaitingForEstimatedVBlankAndQueued(_) => {
                output_state.redraw_state = RedrawState::Queued;
            }
        }

        output_state.frame_callback_sequence = output_state.frame_callback_sequence.wrapping_add(1);

        // Animation pump: headless has no VBlank source to sustain animation
        // frames (compare tty's re-queue on VBlank), so re-arm a frame timer
        // while animations remain. A pending redraw re-evaluates this on its
        // own pass. Unviewed outputs never reach here (standby handles their
        // redraws), so detached slots stay idle through animations.
        if output_state.unfinished_animations_remain
            && matches!(output_state.redraw_state, RedrawState::Idle)
        {
            let timer = Timer::from_duration(Duration::from_millis(1000 / active_fps.max(1)));
            let output_clone = output.clone();
            let token = niri
                .event_loop
                .insert_source(timer, move |_, _, data| {
                    data.niri.queue_redraw(&output_clone);
                    TimeoutAction::Drop
                })
                .ok();
            if let Some(token) = token {
                output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
            }
        }

        RenderResult::Submitted
    }

    /// Whether anything needs full-rate frames from a headless output:
    /// capture clients, PipeWire screencasts, or an agent burst hold —
    /// unless a viewer-away hold vetoes (WO-NORI-002: connected but
    /// backgrounded VNC renders at standby, not full rate).
    fn output_has_viewer_demand(&mut self, niri: &Niri, output: &Output) -> bool {
        if away_hold_active_inner(&mut self.away_cache, TRIGGER_CACHE_TTL) {
            return false;
        }
        if self.forced_active {
            return true;
        }
        if burst_trigger_inner(&mut self.burst_cache, TRIGGER_CACHE_TTL).0 {
            return true;
        }
        if niri.screencopy_state.has_active_cast_for_output(output) {
            return true;
        }
        if niri.image_copy_sessions.iter().any(|s| {
            s.pending_frame.is_some()
                && image_copy_capture::source_output(&s.session.source())
                    .is_some_and(|src| src == *output)
        }) {
            return true;
        }
        if niri.image_copy_cursor_sessions.iter().any(|s| {
            s.pending_frame.is_some()
                && image_copy_capture::source_output(&s.session.source())
                    .is_some_and(|src| src == *output)
        }) {
            return true;
        }
        #[cfg(feature = "xdp-gnome-screencast")]
        {
            let weak = output.downgrade();
            if niri
                .casting
                .casts
                .iter()
                .any(|cast| cast.is_active() && cast.target.matches_output(&weak))
            {
                return true;
            }
        }
        false
    }

    /// Runs a standby drain instead of a full redraw when a headless output
    /// has no presentation demand.
    ///
    /// Client commits between standby ticks coalesce on the re-armed timer,
    /// so an aggressive background client costs one cheap drain per tick
    /// instead of a full scene pass per commit. Returns true when the redraw
    /// was handled here; false means the caller must run the full path now
    /// (demand appeared, e.g. a viewer just attached).
    pub fn try_standby_redraw(&mut self, niri: &mut Niri, output: &Output) -> bool {
        if self.output_has_viewer_demand(niri, output) {
            return false;
        }

        let interval = Duration::from_millis(1000 / standby_fps().max(1));
        let now = get_monotonic_time();

        // Pace frame callbacks to the standby rate. Without this, callback
        // driven clients would ping-pong a redraw per commit at full rate.
        let due = self
            .standby_last_send
            .get(output)
            .map(|last| now - *last >= interval)
            .unwrap_or(true);
        if due {
            {
                let output_state = niri.output_state.get_mut(output).unwrap();
                output_state.frame_callback_sequence =
                    output_state.frame_callback_sequence.wrapping_add(1);
            }
            niri.send_frame_callbacks(output);
            // Nothing is presented in standby; report discarded honestly.
            let states = RenderElementStates::default();
            let mut feedbacks = niri.take_presentation_feedbacks(output, &states);
            feedbacks.discarded();
            self.standby_last_send.insert(output.clone(), now);
        }

        // Consume the queued state, preserving any live standby timer so
        // continuous commits cannot starve the tick by re-arming it.
        let output_state = niri.output_state.get_mut(output).unwrap();
        match mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
            RedrawState::Queued => {
                let timer = Timer::from_duration(interval);
                let output_clone = output.clone();
                let token = niri
                    .event_loop
                    .insert_source(timer, move |_, _, data| {
                        data.niri.queue_redraw(&output_clone);
                        TimeoutAction::Drop
                    })
                    .ok();
                if let Some(token) = token {
                    output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
                }
            }
            RedrawState::WaitingForEstimatedVBlankAndQueued(token) => {
                output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
            }
            other => {
                output_state.redraw_state = other;
            }
        }

        true
    }

    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        if let Some(renderer) = &mut self.renderer {
            renderer.import_dmabuf(dmabuf, None).is_ok()
        } else {
            false
        }
    }

    pub fn ipc_outputs(&self) -> Arc<Mutex<IpcOutputMap>> {
        self.ipc_outputs.clone()
    }

    /// Apply a transient/configured mode to a headless output.
    ///
    /// Called from `on_output_config_changed`: headless outputs have no DRM
    /// connector, so mode/custom-mode requests (e.g. `nori msg output
    /// headless-1 custom-mode 1280x800@60.000` from the z-axis fit worker)
    /// would otherwise be silently ignored. Applies the configured size to
    /// the Smithay output, refreshes the single-entry IPC mode list, and
    /// drives the standard resize path (layout + layers + backdrop).
    pub fn on_output_config_changed(&mut self, niri: &mut Niri) {
        // Two-phase: collect resize decisions under the shared borrow, then
        // apply them under &mut (the outputs iterator borrows niri).
        let planned: Vec<(Output, u16, u16, i32)> = {
            let config = niri.config.borrow().outputs.clone();
            niri.global_space
                .outputs()
                .filter_map(|output| {
                    let name = output.user_data().get::<OutputName>()?;
                    let entry = config.find(name)?;
                    let want = entry.mode?;
                    let (w, h) = (want.mode.width, want.mode.height);
                    if w == 0 || h == 0 {
                        return None;
                    }
                    let current = output.current_mode().map(|m| m.size);
                    if current == Some((i32::from(w), i32::from(h)).into()) {
                        return None;
                    }
                    let refresh = want
                        .mode
                        .refresh
                        .map(|r| (r * 1000.0).round() as i32)
                        .unwrap_or(60_000)
                        .max(1);
                    Some((output.clone(), w, h, refresh))
                })
                .collect()
        };
        for (output, w, h, refresh) in planned {
            let mode = Mode {
                size: (i32::from(w), i32::from(h)).into(),
                refresh,
            };
            output.change_current_state(Some(mode), None, None, None);
            output.set_preferred(mode);

            let physical = output.physical_properties();
            let mut ipc_outputs = self.ipc_outputs.lock().unwrap();
            if let Some(ipc) = ipc_outputs
                .values_mut()
                .find(|o| o.name == output.name())
            {
                ipc.modes = vec![niri_ipc::Mode {
                    width: w,
                    height: h,
                    refresh_rate: refresh as u32,
                    is_preferred: true,
                }];
                ipc.current_mode = Some(0);
                ipc.is_custom_mode = true;
                ipc.make = physical.make;
                ipc.model = physical.model;
            }
            drop(ipc_outputs);
            niri.output_resized(&output);
            niri.ipc_outputs_changed = true;
            niri.queue_redraw(&output);
        }
    }

    #[cfg(feature = "xdp-gnome-screencast")]
    pub fn gbm_device(
        &self,
    ) -> Option<smithay::backend::allocator::gbm::GbmDevice<smithay::utils::DeviceFd>> {
        self.gbm_device.clone()
    }

    pub fn set_active(&mut self, active: bool) {
        self.forced_active = active;
    }

    pub fn is_active(&self) -> bool {
        self.forced_active
    }
}

impl Default for Headless {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::parse_burst_trigger;
    use super::BurstState;
    use super::CachedRead;
    use super::TriggerContent;

    #[test]
    fn burst_live_with_fps_override() {
        assert_eq!(
            parse_burst_trigger("2000000000000 30\n", 1000),
            BurstState::Live(Some(30))
        );
    }

    #[test]
    fn burst_live_without_fps_keeps_default() {
        assert_eq!(
            parse_burst_trigger("2000000000000\n", 1000),
            BurstState::Live(None)
        );
        assert_eq!(
            parse_burst_trigger("2000000000000 fast\n", 1000),
            BurstState::Live(None)
        );
    }

    #[test]
    fn burst_expired_at_deadline() {
        assert_eq!(
            parse_burst_trigger("1000 60\n", 1000),
            BurstState::Expired
        );
        assert_eq!(parse_burst_trigger("999\n", 1000), BurstState::Expired);
    }

    #[test]
    fn burst_garbage_is_invalid() {
        assert_eq!(parse_burst_trigger("", 1000), BurstState::Invalid);
        assert_eq!(parse_burst_trigger("soon 60\n", 1000), BurstState::Invalid);
    }

    fn write_trigger(dir: &std::path::Path, name: &str, content: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn live_file(dir: &std::path::Path, name: &str) -> String {
        // Far-future expiry: live for any test clock.
        write_trigger(dir, name, "2000000000000 60\n")
    }

    #[test]
    fn cached_read_serves_live_content() {
        let dir = std::env::temp_dir().join(format!("nori-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = live_file(&dir, "nori-burst-1");

        let mut cache = CachedRead::new();
        let now = Instant::now();
        let got = cache.get(&path, now, 1000, Duration::from_secs(60), parse_burst_trigger, || {
            panic!("live file must not expire")
        });
        assert_eq!(
            got,
            Some(TriggerContent {
                expires_ms: 2000000000000,
                fps: Some(60),
            })
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cached_read_missing_file_stays_negative_within_ttl() {
        // A missing file reads None once, then serves the negative
        // result without touching disk until the TTL elapses — this is
        // what kills the ~3k/s openat storm on trigger-less slots.
        let dir = std::env::temp_dir().join(format!("nori-cache-neg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nori-burst-9").to_string_lossy().into_owned();

        let mut cache = CachedRead::new();
        let t0 = Instant::now();
        let ttl = Duration::from_secs(60);
        assert_eq!(
            cache.get(&path, t0, 1000, ttl, parse_burst_trigger, || {}),
            None
        );
        // File appears inside the TTL window: still None (stale, bounded).
        live_file(&dir, "nori-burst-9");
        assert_eq!(
            cache.get(&path, t0, 1000, ttl, parse_burst_trigger, || {}),
            None
        );
        // Past the TTL the re-read picks it up.
        let got = cache.get(
            &path,
            t0 + ttl,
            1000,
            ttl,
            parse_burst_trigger,
            || panic!("live file must not expire"),
        );
        assert!(got.is_some_and(|c| c.fps == Some(60)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cached_read_expired_hold_never_lingers() {
        // Expiry is authoritative inside the TTL window: a hold that
        // dies between reads must clear on the next evaluation, and
        // the expired file is removed on re-read.
        let dir = std::env::temp_dir().join(format!("nori-cache-exp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_trigger(&dir, "nori-away-1", "5000 60\n");

        let mut cache = CachedRead::new();
        let t0 = Instant::now();
        let ttl = Duration::from_secs(60);
        assert!(cache
            .get(&path, t0, 1000, ttl, parse_burst_trigger, || {})
            .is_some());
        // Same TTL window, clock past expiry: cleared without a re-read.
        assert_eq!(
            cache.get(&path, t0, 6000, ttl, parse_burst_trigger, || {
                panic!("must not re-read inside TTL")
            }),
            None
        );
        // Past the TTL the re-read observes expiry and removes the file.
        assert_eq!(
            cache.get(&path, t0 + ttl, 6000, ttl, parse_burst_trigger, || {
                let _ = std::fs::remove_file(&path);
            }),
            None
        );
        assert!(!std::path::Path::new(&path).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cached_read_invalid_file_is_negative() {
        let dir = std::env::temp_dir().join(format!("nori-cache-inv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_trigger(&dir, "nori-away-1", "soon 60\n");

        let mut cache = CachedRead::new();
        assert_eq!(
            cache.get(
                &path,
                Instant::now(),
                1000,
                Duration::from_secs(60),
                parse_burst_trigger,
                || {},
            ),
            None
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
