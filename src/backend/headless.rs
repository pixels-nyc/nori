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
use smithay::backend::renderer::ImportDma;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::utils::{DeviceFd, Size};
use smithay::wayland::presentation::Refresh;

use super::{IpcOutputMap, OutputId, RenderResult};
use crate::niri::{Niri, RedrawState};
use crate::render_helpers::{resources, shaders};
use crate::utils::{get_monotonic_time, logical_output};

pub struct Headless {
    renderer: Option<GlesRenderer>,
    render_node: Option<DrmNode>,
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
            #[cfg(feature = "xdp-gnome-screencast")]
            gbm_device: None,
            ipc_outputs: Default::default(),
            last_render_time: None,
            forced_active: false,
        }
    }

    pub fn init(&mut self, _niri: &mut Niri) {}

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
        // Detached = 5 Hz standby software tick (200ms) to ensure <1% CPU and bounded VRAM
        // Active = 60 Hz (16.66ms) when screencopy (waypipe, wayvnc, nori-0) is actively reading frames
        let is_viewer_attached = self.forced_active || niri.screencopy_state.has_active_cast();
        let target_interval = if is_viewer_attached {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(200)
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
            RedrawState::Idle => unreachable!(),
            RedrawState::Queued => (),
            RedrawState::WaitingForVBlank { .. } => unreachable!(),
            RedrawState::WaitingForEstimatedVBlank(_) => unreachable!(),
            RedrawState::WaitingForEstimatedVBlankAndQueued(_) => unreachable!(),
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
