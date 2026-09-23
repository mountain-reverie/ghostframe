//! Tests for `route_window_event`: the pure decision the render/event loop
//! makes for one backend `WindowEvent`. The loop itself needs a real
//! display and can't be unit-tested here -- this is the part of it most
//! likely to be wrong, and it doesn't need one.

use ghostframe_cli::chord::{Chord, Prefix};
use ghostframe_cli::commands::{route_window_event, InputToSend, LoopAction};
use ghostframe_cli::geometry::Placement;
use ghostframe_cli::window::WindowEvent;

// X11 keysyms, same as tests/chord.rs.
const CTRL_L: u32 = 0xffe3;
const ALT_L: u32 = 0xffe9;
const B: u32 = 0x62;
const D: u32 = 0x64;
const H: u32 = 0x68;
const X: u32 = 0x78;

fn ctrl_alt_b() -> Chord {
    Chord::new(Prefix::CtrlAltB)
}

fn some_placement() -> Placement {
    // A 1920x1080 remote centred in a 2560x1440 output: origin (320, 180).
    Placement::centre(1920, 1080, 2560, 1440)
}

#[test]
fn a_plain_key_forwards() {
    let mut chord = ctrl_alt_b();
    let placement = some_placement();
    let action = route_window_event(
        &WindowEvent::Key {
            keysym: X,
            down: true,
        },
        &mut chord,
        &placement,
    );
    assert_eq!(
        action,
        LoopAction::Forward(InputToSend::Key {
            keysym: X,
            down: true
        })
    );
}

#[test]
fn a_completed_quit_chord_yields_quit() {
    let mut chord = ctrl_alt_b();
    let placement = some_placement();
    // Arm: Ctrl, Alt, b -- all forwarded, chord state tracked internally.
    route_window_event(
        &WindowEvent::Key {
            keysym: CTRL_L,
            down: true,
        },
        &mut chord,
        &placement,
    );
    route_window_event(
        &WindowEvent::Key {
            keysym: ALT_L,
            down: true,
        },
        &mut chord,
        &placement,
    );
    route_window_event(
        &WindowEvent::Key {
            keysym: B,
            down: true,
        },
        &mut chord,
        &placement,
    );
    let action = route_window_event(
        &WindowEvent::Key {
            keysym: D,
            down: true,
        },
        &mut chord,
        &placement,
    );
    assert_eq!(action, LoopAction::Quit);
}

#[test]
fn a_completed_minimize_chord_yields_minimize() {
    let mut chord = ctrl_alt_b();
    let placement = some_placement();
    route_window_event(
        &WindowEvent::Key {
            keysym: CTRL_L,
            down: true,
        },
        &mut chord,
        &placement,
    );
    route_window_event(
        &WindowEvent::Key {
            keysym: ALT_L,
            down: true,
        },
        &mut chord,
        &placement,
    );
    route_window_event(
        &WindowEvent::Key {
            keysym: B,
            down: true,
        },
        &mut chord,
        &placement,
    );
    let action = route_window_event(
        &WindowEvent::Key {
            keysym: H,
            down: true,
        },
        &mut chord,
        &placement,
    );
    assert_eq!(action, LoopAction::Minimize);
}

#[test]
fn pointer_motion_inside_the_image_maps_to_the_expected_remote_coordinate() {
    let mut chord = ctrl_alt_b();
    let placement = some_placement();
    let action = route_window_event(
        &WindowEvent::PointerMotion { x: 1000, y: 700 },
        &mut chord,
        &placement,
    );
    assert_eq!(
        action,
        LoopAction::Forward(InputToSend::PointerMotion { x: 680, y: 520 })
    );
}

#[test]
fn pointer_motion_in_the_black_surround_clamps() {
    let mut chord = ctrl_alt_b();
    let placement = some_placement();
    // Top-left corner of the 2560x1440 output, well outside the centred
    // 1920x1080 image -- must clamp to (0, 0), not go negative.
    let action = route_window_event(
        &WindowEvent::PointerMotion { x: 0, y: 0 },
        &mut chord,
        &placement,
    );
    assert_eq!(
        action,
        LoopAction::Forward(InputToSend::PointerMotion { x: 0, y: 0 })
    );
}

#[test]
fn pointer_button_maps_through_placement_too() {
    let mut chord = ctrl_alt_b();
    let placement = some_placement();
    let action = route_window_event(
        &WindowEvent::PointerButton {
            x: 320,
            y: 180,
            button: 1,
            down: true,
        },
        &mut chord,
        &placement,
    );
    assert_eq!(
        action,
        LoopAction::Forward(InputToSend::PointerButton {
            x: 0,
            y: 0,
            button: 1,
            down: true
        })
    );
}

#[test]
fn wheel_forwards_unchanged() {
    let mut chord = ctrl_alt_b();
    let placement = some_placement();
    let action = route_window_event(
        &WindowEvent::Wheel { dx: 0, dy: -3 },
        &mut chord,
        &placement,
    );
    assert_eq!(
        action,
        LoopAction::Forward(InputToSend::Wheel { dx: 0, dy: -3 })
    );
}

#[test]
fn close_requested_yields_quit() {
    let mut chord = ctrl_alt_b();
    let placement = some_placement();
    let action = route_window_event(&WindowEvent::CloseRequested, &mut chord, &placement);
    assert_eq!(action, LoopAction::Quit);
}

#[test]
fn resized_yields_recompute() {
    let mut chord = ctrl_alt_b();
    let placement = some_placement();
    let action = route_window_event(
        &WindowEvent::Resized {
            width: 3840,
            height: 2160,
        },
        &mut chord,
        &placement,
    );
    assert_eq!(action, LoopAction::Recompute);
}
