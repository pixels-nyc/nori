//! Virtual keyboard that feeds the normal input pipeline (binds included).
//!
//! This replaces Smithay's default `VirtualKeyboardManagerState` dispatch,
//! which delivers keys straight to the focused client and skips the seat
//! `keyboard.input()` filter — meaning compositor binds never fire for
//! VNC-injected keys ("as if you have zero binds" over VNC).
//!
//! Here every key is routed through `process_input_event`, so remote keys
//! behave exactly like physical keys: binds, `suppressed_keys`, key repeat,
//! and idle/monitor activation all apply uniformly.
//!
//! Two deliberate simplifications vs. the Smithay default:
//!
//! - Routed keys are interpreted with the *seat* keymap, not the keymap the
//!   virtual-keyboard client submits (accepted and ignored). This matches
//!   physical keyboards. A remote client on a different layout will observe
//!   seat-layout syms.
//! - The `modifiers` request is ignored: the seat XKB state is derived from
//!   the routed press/release keycodes, and the seat emits its own modifier
//!   events to clients from that state.

use std::collections::HashSet;
use std::sync::Mutex;

use smithay::backend::input::{
    Device, DeviceCapability, Event, InputBackend, InputTime, KeyState, KeyboardKeyEvent,
    UnusedEvent,
};
use smithay::reexports::wayland_protocols_misc::zwp_virtual_keyboard_v1::server::{
    zwp_virtual_keyboard_manager_v1::{self, ZwpVirtualKeyboardManagerV1},
    zwp_virtual_keyboard_v1::{self, ZwpVirtualKeyboardV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::{Dispatch2, GlobalDispatch2};

use crate::protocols::EmptyData;

const VERSION: u32 = 1;

pub struct VirtualKeyboardManagerState;

pub struct VirtualKeyboardManagerGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

pub struct VirtualKeyboardInputBackend;

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct VirtualKeyboard {
    keyboard: ZwpVirtualKeyboardV1,
}

#[derive(Debug, Default)]
pub struct VirtualKeyboardUserData {
    pressed: Mutex<HashSet<u32>>,
}

impl Device for VirtualKeyboard {
    fn id(&self) -> String {
        format!("virtual keyboard {}", self.keyboard.id())
    }

    fn name(&self) -> String {
        String::from("virtual keyboard")
    }

    fn has_capability(&self, capability: DeviceCapability) -> bool {
        matches!(capability, DeviceCapability::Keyboard)
    }

    fn usb_id(&self) -> Option<(u32, u32)> {
        None
    }

    fn syspath(&self) -> Option<std::path::PathBuf> {
        None
    }
}

pub struct VirtualKeyboardKeyEvent {
    keyboard: VirtualKeyboard,
    time: u32,
    /// Evdev keycode, as carried by the protocol.
    key: u32,
    state: KeyState,
    /// Keys currently held on this virtual keyboard.
    count: u32,
}

/// Convert the protocol's evdev keycode to the XKB keycode the seat
/// expects (same +8 as the libinput and winit backends).
fn xkb_keycode(evdev: u32) -> smithay::backend::input::Keycode {
    (evdev + 8).into()
}

impl Event<VirtualKeyboardInputBackend> for VirtualKeyboardKeyEvent {
    fn time(&self) -> InputTime {
        InputTime::from_millis(self.time)
    }

    fn device(&self) -> VirtualKeyboard {
        self.keyboard.clone()
    }
}

impl KeyboardKeyEvent<VirtualKeyboardInputBackend> for VirtualKeyboardKeyEvent {
    fn key_code(&self) -> smithay::backend::input::Keycode {
        xkb_keycode(self.key)
    }

    fn state(&self) -> KeyState {
        self.state
    }

    fn count(&self) -> u32 {
        self.count
    }
}

impl InputBackend for VirtualKeyboardInputBackend {
    type Device = VirtualKeyboard;

    type KeyboardKeyEvent = VirtualKeyboardKeyEvent;
    type PointerAxisEvent = UnusedEvent;
    type PointerButtonEvent = UnusedEvent;
    type PointerMotionEvent = UnusedEvent;
    type PointerMotionAbsoluteEvent = UnusedEvent;

    type GestureSwipeBeginEvent = UnusedEvent;
    type GestureSwipeUpdateEvent = UnusedEvent;
    type GestureSwipeEndEvent = UnusedEvent;
    type GesturePinchBeginEvent = UnusedEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = UnusedEvent;
    type GestureHoldEndEvent = UnusedEvent;

    type TouchDownEvent = UnusedEvent;
    type TouchUpEvent = UnusedEvent;
    type TouchMotionEvent = UnusedEvent;
    type TouchCancelEvent = UnusedEvent;
    type TouchFrameEvent = UnusedEvent;
    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = UnusedEvent;

    type SwitchToggleEvent = UnusedEvent;

    type SpecialEvent = UnusedEvent;
}

pub trait VirtualKeyboardHandler {
    fn virtual_keyboard_manager_state(&mut self) -> &mut VirtualKeyboardManagerState;

    fn create_virtual_keyboard(&mut self, keyboard: VirtualKeyboard) {
        let _ = keyboard;
    }
    fn destroy_virtual_keyboard(&mut self, keyboard: VirtualKeyboard) {
        let _ = keyboard;
    }

    fn on_virtual_keyboard_key(&mut self, event: VirtualKeyboardKeyEvent);
}

impl VirtualKeyboardManagerState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ZwpVirtualKeyboardManagerV1, VirtualKeyboardManagerGlobalData>,
        D: VirtualKeyboardHandler,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = VirtualKeyboardManagerGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, ZwpVirtualKeyboardManagerV1, _>(VERSION, global_data);

        Self
    }
}

impl<D> GlobalDispatch2<ZwpVirtualKeyboardManagerV1, D> for VirtualKeyboardManagerGlobalData
where
    D: Dispatch<ZwpVirtualKeyboardManagerV1, EmptyData>,
    D: 'static,
{
    fn bind(
        &self,
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        manager: New<ZwpVirtualKeyboardManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(manager, EmptyData);
    }

    fn can_view(&self, client: &Client) -> bool {
        (self.filter)(client)
    }
}

impl<D> Dispatch2<ZwpVirtualKeyboardManagerV1, D> for EmptyData
where
    D: Dispatch<ZwpVirtualKeyboardV1, VirtualKeyboardUserData>,
    D: VirtualKeyboardHandler,
    D: 'static,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        _resource: &ZwpVirtualKeyboardManagerV1,
        request: <ZwpVirtualKeyboardManagerV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        let zwp_virtual_keyboard_manager_v1::Request::CreateVirtualKeyboard { id, .. } = request
        else {
            unreachable!()
        };

        let keyboard = data_init.init(id, VirtualKeyboardUserData::default());
        state.create_virtual_keyboard(VirtualKeyboard { keyboard });
    }
}

impl<D> Dispatch2<ZwpVirtualKeyboardV1, D> for VirtualKeyboardUserData
where
    D: VirtualKeyboardHandler,
    D: 'static,
{
    fn request(
        &self,
        handler: &mut D,
        _client: &Client,
        resource: &ZwpVirtualKeyboardV1,
        request: <ZwpVirtualKeyboardV1 as Resource>::Request,
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let keyboard = VirtualKeyboard {
            keyboard: resource.clone(),
        };
        match request {
            zwp_virtual_keyboard_v1::Request::Keymap { .. } => {
                // Accepted and ignored: routed keys use the seat keymap.
            }
            zwp_virtual_keyboard_v1::Request::Key { time, key, state } => {
                trace!(key, state, "nori virtual-keyboard key");
                let pressed = state == 1;
                let state = if pressed {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                };
                let mut pressed_keys = self.pressed.lock().unwrap();
                if pressed {
                    pressed_keys.insert(key);
                } else {
                    pressed_keys.remove(&key);
                }
                let count = pressed_keys.len() as u32;
                drop(pressed_keys);
                handler.on_virtual_keyboard_key(VirtualKeyboardKeyEvent {
                    keyboard,
                    time,
                    key,
                    state,
                    count,
                });
            }
            zwp_virtual_keyboard_v1::Request::Modifiers { .. } => {
                // Ignored: the seat XKB state derives from the routed keycodes.
            }
            zwp_virtual_keyboard_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(
        &self,
        handler: &mut D,
        _client: wayland_backend::server::ClientId,
        resource: &ZwpVirtualKeyboardV1,
    ) {
        // The client is gone and will never send releases for keys it held.
        // Route synthetic releases for every still-pressed key, otherwise the
        // seat keeps the key down forever (stuck modifiers after viewer detach).
        let stale: Vec<u32> = self.pressed.lock().unwrap().drain().collect();
        for key in stale {
            handler.on_virtual_keyboard_key(VirtualKeyboardKeyEvent {
                keyboard: VirtualKeyboard {
                    keyboard: resource.clone(),
                },
                time: 0,
                key,
                state: KeyState::Released,
                count: 0,
            });
        }
        let keyboard = VirtualKeyboard {
            keyboard: resource.clone(),
        };
        handler.destroy_virtual_keyboard(keyboard);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keycode_conversion_adds_eight() {
        // The protocol carries evdev codes; the seat wants XKB (evdev + 8).
        // KEY_Q = 16 -> 24, KEY_A = 30 -> 38, KEY_LEFTALT = 56 -> 64.
        let new = smithay::backend::input::Keycode::new;
        assert_eq!(xkb_keycode(16), new(24));
        assert_eq!(xkb_keycode(30), new(38));
        assert_eq!(xkb_keycode(56), new(64));
    }
}
