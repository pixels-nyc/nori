use super::*;

fn output_by_name(f: &mut Fixture, name: &str) -> smithay::output::Output {
    // NB: index-based niri_output() is unreliable here — State::new runs
    // Headless::init, which auto-detects the HOST's DRM topology and may
    // pre-create headless-N outputs (e.g. headless-1 @ 1920x1200 from eDP-1
    // on this laptop) before the test adds its own. Resolve by connector
    // name instead.
    f.niri()
        .global_space
        .outputs()
        .find(|o| o.name() == name)
        .unwrap()
        .clone()
}

#[test]
fn headless_custom_mode_resizes_output() {
    // `niri msg output headless-1 custom-mode WxH@R` must resize headless
    // outputs (the z-axis fit worker's headless-fit path). Before the
    // Headless::on_output_config_changed implementation, transient mode
    // changes were silently ignored for headless outputs (Backend::Headless
    // arm was a no-op) and fit reported verify-mismatch.
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    // Normalize: the auto-detected output (if any) shares the name, so
    // first assert at least one headless-1 exists, then resize by name.
    let _ = output_by_name(&mut f, "headless-1");

    f.niri_state().apply_transient_output_config(
        "headless-1",
        niri_ipc::OutputAction::CustomMode {
            mode: niri_ipc::ConfiguredMode {
                width: 1280,
                height: 800,
                refresh: Some(60.0),
            },
        },
    );

    // Every headless-1 output now carries the fitted mode.
    let sizes: Vec<_> = f
        .niri()
        .global_space
        .outputs()
        .filter(|o| o.name() == "headless-1")
        .map(|o| o.current_mode().unwrap())
        .collect();
    assert!(!sizes.is_empty());
    for mode in &sizes {
        assert_eq!(mode.size, (1280, 800).into());
        assert_eq!(mode.refresh, 60_000);
    }

    // IPC mode list follows the resize (single-entry headless list).
    let ipc = f.niri_state().backend.ipc_outputs();
    let outputs = ipc.lock().unwrap();
    let entry = outputs.values().find(|o| o.name == "headless-1").unwrap();
    assert_eq!(entry.modes.len(), 1);
    assert_eq!((entry.modes[0].width, entry.modes[0].height), (1280, 800));
    assert_eq!(entry.current_mode, Some(0));
    assert!(entry.is_custom_mode);
}

#[test]
fn headless_mode_request_is_idempotent() {
    // Re-applying the fitted size performs no further resize: apply twice,
    // assert the mode list still has exactly one entry (no duplicates).
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let action = || niri_ipc::OutputAction::CustomMode {
        mode: niri_ipc::ConfiguredMode {
            width: 1280,
            height: 800,
            refresh: Some(60.0),
        },
    };
    f.niri_state()
        .apply_transient_output_config("headless-1", action());
    f.niri_state()
        .apply_transient_output_config("headless-1", action());

    let ipc = f.niri_state().backend.ipc_outputs();
    let outputs = ipc.lock().unwrap();
    let entry = outputs.values().find(|o| o.name == "headless-1").unwrap();
    assert_eq!(entry.modes.len(), 1);
    assert_eq!((entry.modes[0].width, entry.modes[0].height), (1280, 800));
}

#[test]
fn headless_resize_leaves_other_outputs_alone() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.add_output(2, (1920, 1080));

    f.niri_state().apply_transient_output_config(
        "headless-2",
        niri_ipc::OutputAction::CustomMode {
            mode: niri_ipc::ConfiguredMode {
                width: 1280,
                height: 800,
                refresh: None,
            },
        },
    );

    for output in f.niri().global_space.outputs() {
        let size = output.current_mode().unwrap().size;
        if output.name() == "headless-2" {
            assert_eq!(size, (1280, 800).into());
        } else if output.name() == "headless-1" {
            // headless-1 keeps whatever it had (1920x1080 test output or
            // the host auto-detected size) — just never the fitted size
            // unless it already was.
            assert_ne!(size, (1280, 800).into());
        }
    }
}
