use std::time::Duration;

use niri_config::{Config, StrutBehavior};
use smithay::backend::input::{InputEvent, InputTime, KeyState};
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;
use smithay::reexports::wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::Anchor;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::ClientId;
use super::test_input_backend::{TestInputBackend, TestKeyboardKeyEvent};
use super::*;
use crate::niri::Niri;
use crate::tests::client::LayerConfigureProps;

fn set_up(behavior: StrutBehavior) -> Fixture {
    let mut config = Config::default();
    config.layout.strut_behavior = behavior;

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    f
}

fn map_window(f: &mut Fixture, id: ClientId) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    surface
}

fn map_left_strut(f: &mut Fixture, id: ClientId, width: i32) -> WlSurface {
    let layer = f.client(id).create_layer(None, Layer::Top, "");
    let surface = layer.surface.clone();
    layer.set_configure_props(LayerConfigureProps {
        anchor: Some(Anchor::Left | Anchor::Top | Anchor::Bottom),
        size: Some((width as u32, 0)),
        exclusive_zone: Some(width),
        ..Default::default()
    });
    layer.commit();
    f.roundtrip(id);

    let layer = f.client(id).layer(&surface);
    layer.attach_new_buffer();
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    surface
}

fn unmap_layer(f: &mut Fixture, id: ClientId, surface: &WlSurface) {
    let layer = f.client(id).layer(surface);
    layer.attach_null();
    layer.commit();
    f.double_roundtrip(id);
}

fn view_target(f: &mut Fixture) -> f64 {
    f.niri()
        .layout
        .active_workspace()
        .unwrap()
        .scrolling()
        .view_offset_target()
}

fn tile_x(f: &mut Fixture) -> f64 {
    let niri = f.niri();
    let ws = niri.layout.active_workspace().unwrap();
    let tiles: Vec<_> = ws.tiles_with_render_positions().collect();
    assert_eq!(tiles.len(), 1);
    tiles[0].1.x
}

fn press_keys(f: &mut Fixture, id: ClientId, names: &[&str]) {
    let state = f.niri_state();
    let keyboard = state.niri.seat.get_keyboard().unwrap();
    let codes: Vec<_> = keyboard.with_xkb_state(state, |xkb| {
        let xkb = xkb.xkb().lock().unwrap();
        let keymap = unsafe { xkb.keymap() };
        names
            .iter()
            .map(|name| {
                keymap
                    .key_by_name(name)
                    .unwrap_or_else(|| panic!("unknown key {name}"))
            })
            .collect()
    });

    let send = |f: &mut Fixture, key_state| {
        for code in &codes {
            f.niri_state()
                .process_input_event(InputEvent::<TestInputBackend>::Keyboard {
                    event: TestKeyboardKeyEvent {
                        time: InputTime::from_micros(0),
                        code: *code,
                        state: key_state,
                        count: 1,
                    },
                });
        }
    };

    send(f, KeyState::Pressed);
    send(f, KeyState::Released);
    f.roundtrip(id);
}

fn set_time(niri: &mut Niri, time: Duration) {
    // Same clock dance as the animation tests: reset, then set the time.
    let now = niri.clock.now();
    niri.clock.set_unadjusted(now);
    let _ = niri.clock.now();
    niri.clock.set_unadjusted(Duration::ZERO);
    niri.clock.set_rate(1.0);
    let _ = niri.clock.now();

    niri.clock.set_unadjusted(time);
    let _ = niri.clock.now();

    // Freeze the clock so the niri loop callback doesn't replace it.
    niri.clock.set_rate(0.0);
}

#[test]
fn left_strut_slides_ribbon_without_resizing() {
    let mut f = set_up(StrutBehavior::NudgeRibbon);
    let id = f.add_client();
    let surface = map_window(&mut f, id);

    let window = f.client(id).window(&surface);
    let sizes: Vec<_> = window.recent_configures().map(|c| c.size).collect();
    let size = *sizes.last().unwrap();
    assert_ne!(size, (0, 0));

    map_left_strut(&mut f, id, 52);

    // The ribbon slides aside; the window must see zero resize configures.
    let window = f.client(id).window(&surface);
    for configure in window.recent_configures() {
        assert_eq!(
            configure.size, size,
            "left strut must not resize windows in nudge mode"
        );
    }
}

#[test]
fn left_strut_squash_resizes_window() {
    let mut f = set_up(StrutBehavior::SquashColumns);
    let id = f.add_client();
    let surface = map_window(&mut f, id);

    let window = f.client(id).window(&surface);
    let sizes: Vec<_> = window.recent_configures().map(|c| c.size).collect();
    let size = *sizes.last().unwrap();
    assert_ne!(size, (0, 0));

    map_left_strut(&mut f, id, 52);

    // Legacy behavior: the window is squeezed narrower by the strut.
    let window = f.client(id).window(&surface);
    let new_sizes: Vec<_> = window.recent_configures().map(|c| c.size).collect();
    assert!(
        new_sizes.iter().any(|s| *s != size),
        "squash mode must resize the window, got {new_sizes:?} (was {size:?})"
    );
}

#[test]
fn removing_strut_restores_size_without_resize() {
    let mut f = set_up(StrutBehavior::NudgeRibbon);
    let id = f.add_client();
    let surface = map_window(&mut f, id);

    let window = f.client(id).window(&surface);
    let sizes: Vec<_> = window.recent_configures().map(|c| c.size).collect();
    let size = *sizes.last().unwrap();

    let strut = map_left_strut(&mut f, id, 52);

    // Drain any configures from the slide-aside.
    let window = f.client(id).window(&surface);
    for configure in window.recent_configures() {
        assert_eq!(configure.size, size);
    }

    unmap_layer(&mut f, id, &strut);

    // Sliding back must not resize either.
    let window = f.client(id).window(&surface);
    for configure in window.recent_configures() {
        assert_eq!(
            configure.size, size,
            "removing the strut must not resize windows in nudge mode"
        );
    }
}

#[test]
fn keybind_shift_view_offset_slides_smoothly_without_resizing() {
    let config = Config::parse_mem(
        "
        layout { strut-behavior \"nudge-ribbon\"; }
        binds { Mod+Shift+N { shift-view-offset 52; }; }
        ",
    )
    .unwrap();

    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id);

    let window = f.client(id).window(&surface);
    let sizes: Vec<_> = window.recent_configures().map(|c| c.size).collect();
    let size = *sizes.last().unwrap();
    assert_ne!(size, (0, 0));
    let t0 = view_target(&mut f);

    map_left_strut(&mut f, id, 52);
    let window = f.client(id).window(&surface);
    for configure in window.recent_configures() {
        assert_eq!(configure.size, size);
    }
    let t1 = view_target(&mut f);
    assert_eq!(t1, t0 - 52., "strut slides content right 52px");

    // Settle the strut slide before the keybind phase, moving the clock
    // forward (never rewinding past the animation start).
    let now = f.niri().clock.now();
    let base = now + Duration::from_millis(2000);
    set_time(f.niri(), base);
    f.niri().advance_animations();

    // Drive the action through a real keybind press.
    let x_before = tile_x(&mut f);
    press_keys(&mut f, id, &["LWIN", "LFSH", "LatN"]);

    let t2 = view_target(&mut f);
    assert_eq!(t2, t1 - 52., "keybind shifts content right another 52px");

    let window = f.client(id).window(&surface);
    for configure in window.recent_configures() {
        assert_eq!(
            configure.size, size,
            "keybind-driven shift must not resize windows"
        );
    }

    // Sample the slide at ~60fps for half a second.
    let mut prev = x_before;
    let mut max_jump = 0.0f64;
    let mut moved_first_frame = false;
    for (i, ms) in (16..=500).step_by(16).enumerate() {
        set_time(f.niri(), base + Duration::from_millis(ms));
        f.niri().advance_animations();
        let x = tile_x(&mut f);
        assert!(x.is_finite(), "frame {i} produced a non-finite position");

        let step = (x - prev).abs();
        max_jump = max_jump.max(step);
        if i == 0 {
            moved_first_frame = step > 0.0;
        }
        prev = x;
    }

    assert!(moved_first_frame, "slide must start moving promptly");
    assert!(
        max_jump < 52.,
        "no teleport frames, max single-frame jump {max_jump}"
    );

    let x_after = tile_x(&mut f);
    assert!(
        (x_after - (x_before + 52.)).abs() < 1.0,
        "tile slides exactly +52px: {x_before} -> {x_after}"
    );

    // Sizes still stable after every animation frame.
    let window = f.client(id).window(&surface);
    for configure in window.recent_configures() {
        assert_eq!(configure.size, size);
    }
}
