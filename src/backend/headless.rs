//! Headless backend for tests and persistent multi-tenant session spaces.

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

pub struct Headless {
    renderer: Option<GlesRenderer>,
    render_node: Option<DrmNode>,
    dmabuf_global: Option<DmabufGlobal>,
    #[cfg(feature = "xdp-gnome-screencast")]
    gbm_device: Option<GbmDevice<DeviceFd>>,
    ipc_outputs: Arc<Mutex<IpcOutputMap>>,
    last_render_time: Option<Duration>,
    forced_active: bool,
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

        // Check for agent burst trigger file /run/user/{uid}/nori-burst-{slot}
        let mut burst_active = false;
        if let Ok(slot_str) = std::env::var("NORI_HEADLESS_SLOT").or_else(|_| std::env::var("PIXOS_HEAD")) {
            let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
                .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
            let burst_path = format!("{}/nori-burst-{}", runtime_dir, slot_str);
            if let Ok(content) = std::fs::read_to_string(&burst_path) {
                // Format: "<expires_epoch_millis> [target_fps]"
                let parts: Vec<&str> = content.split_whitespace().collect();
                if let Some(exp_str) = parts.first() {
                    if let Ok(exp_ms) = exp_str.parse::<u128>() {
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis();
                        if now_ms < exp_ms {
                            burst_active = true;
                            if let Some(fps_str) = parts.get(1) {
                                if let Ok(f) = fps_str.parse::<u64>() {
                                    active_fps = f.max(1);
                                }
                            }
                        } else {
                            let _ = std::fs::remove_file(&burst_path);
                        }
                    }
                }
            }
        }

        let standby_interval = Duration::from_millis(1000 / standby_fps.max(1));
        let active_interval = Duration::from_millis(1000 / active_fps.max(1));

        let is_viewer_attached = self.forced_active || burst_active || niri.screencopy_state.has_active_cast();
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
                if matches!(output_state.redraw_state, RedrawState::Queued) {
                    let timer = Timer::from_duration(remaining);
                    let token = niri.event_loop.insert_source(timer, move |_, _, data| {
                        data.niri.queue_redraw(&output_clone);
                        TimeoutAction::Drop
                    }).ok();
                    if let Some(token) = token {
                        output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
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

        RenderResult::Submitted
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
