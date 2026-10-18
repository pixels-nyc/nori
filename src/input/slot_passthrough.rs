//! Z-Axis Slot Gesture Passthrough & Inhibit Control
//!
//! Provides intelligent locking and passthrough for touchpad and touchscreen gestures
//! when focused on nori slot viewers (e.g. nori-viewer / wlvncc), nested nori sessions,
//! or shortcut-inhibiting surfaces.

use std::collections::HashMap;
use smithay::backend::input::TouchSlot;
use smithay::utils::{Logical, Point};

#[derive(Debug, Clone)]
pub struct TargetSlotInfo {
    pub slot: u32,
    pub node: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GuestGestureSwipe {
    pub slot: u32,
    pub node: Option<String>,
    pub delta: (f64, f64),
    pub fingers: u32,
}

#[derive(Debug, Clone)]
pub struct GuestTouchGesture {
    pub slot: u32,
    pub node: Option<String>,
    pub start_touches: HashMap<TouchSlot, Point<f64, Logical>>,
    pub last_touches: HashMap<TouchSlot, Point<f64, Logical>>,
    pub fingers: usize,
}

/// Parses target slot and remote node name from a window's app_id or title.
pub fn parse_slot_info(app_id: Option<&str>, title: &str) -> Option<TargetSlotInfo> {
    // Priority 1: app_id (e.g. "nori-viewer-s1-h1", "nori-viewer-s1-inspiron-h1")
    if let Some(app_id) = app_id {
        if let Some(rest) = app_id.strip_prefix("nori-viewer-s") {
            let mut parts = rest.split('-');
            if let Some(slot_str) = parts.next() {
                if let Ok(slot) = slot_str.parse::<u32>() {
                    let mut node = None;
                    for p in parts {
                        // Check if this component is NOT a head specifier (e.g. not "h1", "h2")
                        if !(p.starts_with('h') && p.len() > 1 && p[1..].chars().all(|c| c.is_ascii_digit())) {
                            node = Some(p.to_string());
                        }
                    }
                    return Some(TargetSlotInfo { slot, node });
                }
            }
        }
    }

    // Priority 2: title (e.g. "nori-viewer:head1:slot1")
    if title.contains("nori-viewer") {
        if let Some(idx) = title.find("slot") {
            let after = &title[idx + 4..];
            let num: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(slot) = num.parse::<u32>() {
                return Some(TargetSlotInfo { slot, node: None });
            }
        }
    }

    None
}

/// Dispatches an action asynchronously to the target nori slot compositor.
pub fn send_slot_action(slot: u32, node: Option<&str>, action: &str) {
    let action = action.to_string();
    let node = node.map(|s| s.to_string());

    std::thread::spawn(move || {
        if let Some(node) = node {
            // Remote node: send action over SSH
            let cmd = format!(
                "NIRI_SOCKET=$(ls -t /run/user/1000/niri.wayland-nori-{slot}.*.sock 2>/dev/null | head -1) niri msg action {action}"
            );
            let _ = std::process::Command::new("ssh")
                .args(["-o", "ConnectTimeout=2", &node, &cmd])
                .output();
        } else {
            // Local node: connect directly to slot IPC socket
            if let Some(sock) = find_slot_socket(slot) {
                if let Ok(mut client) = niri_ipc::socket::Socket::connect_to(sock) {
                    let req = match action.as_str() {
                        "focus-column-right" => Some(niri_ipc::Request::Action(niri_ipc::Action::FocusColumnRight {})),
                        "focus-column-left" => Some(niri_ipc::Request::Action(niri_ipc::Action::FocusColumnLeft {})),
                        "focus-workspace-down" => Some(niri_ipc::Request::Action(niri_ipc::Action::FocusWorkspaceDown {})),
                        "focus-workspace-up" => Some(niri_ipc::Request::Action(niri_ipc::Action::FocusWorkspaceUp {})),
                        "toggle-overview" => Some(niri_ipc::Request::Action(niri_ipc::Action::ToggleOverview {})),
                        _ => None,
                    };
                    if let Some(req) = req {
                        let _ = client.send(req);
                    }
                }
            }
        }
    });
}

/// Locates the most recent niri IPC socket for the specified slot.
fn find_slot_socket(slot: u32) -> Option<std::path::PathBuf> {
    let uid = unsafe { libc::getuid() };
    let run_user = format!("/run/user/{uid}");
    let prefix = format!("niri.wayland-nori-{slot}.");
    let mut matching = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&run_user) {
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with(&prefix) && name_str.ends_with(".sock") {
                if let Ok(meta) = entry.metadata() {
                    if let Ok(mtime) = meta.modified() {
                        matching.push((mtime, entry.path()));
                    }
                }
            }
        }
    }
    matching.sort_by_key(|&(mtime, _)| mtime);
    matching.pop().map(|(_, path)| path)
}
