use ghostframe_cli::chord::{Chord, ChordAction, Prefix};

// X11 keysyms: what the wire carries and what xkbcommon yields.
const CTRL_L: u32 = 0xffe3;
const CTRL_R: u32 = 0xffe4;
const ALT_L: u32 = 0xffe9;
const SUPER_L: u32 = 0xffeb;
const B: u32 = 0x62;
const D: u32 = 0x64;
const H: u32 = 0x68;
const X: u32 = 0x78;

fn ctrl_alt_b() -> Chord {
    Chord::new(Prefix::CtrlAltB)
}

#[test]
fn a_bare_key_is_forwarded_and_does_nothing() {
    let mut c = ctrl_alt_b();
    assert_eq!(c.on_key(X, true), ChordAction::Forward);
    assert_eq!(c.on_key(X, false), ChordAction::Forward);
}

#[test]
fn the_prefix_itself_is_still_forwarded() {
    // The remote must not lose the keystroke: we arm, and forward anyway.
    let mut c = ctrl_alt_b();
    c.on_key(CTRL_L, true);
    c.on_key(ALT_L, true);
    assert_eq!(c.on_key(B, true), ChordAction::Forward);
}

#[test]
fn prefix_then_d_quits_and_swallows_only_the_d() {
    let mut c = ctrl_alt_b();
    c.on_key(CTRL_L, true);
    c.on_key(ALT_L, true);
    assert_eq!(c.on_key(B, true), ChordAction::Forward);
    assert_eq!(c.on_key(D, true), ChordAction::Quit);
}

#[test]
fn prefix_then_h_minimizes() {
    let mut c = ctrl_alt_b();
    c.on_key(CTRL_L, true);
    c.on_key(ALT_L, true);
    c.on_key(B, true);
    assert_eq!(c.on_key(H, true), ChordAction::Minimize);
}

#[test]
fn an_unrelated_key_disarms_and_is_forwarded() {
    let mut c = ctrl_alt_b();
    c.on_key(CTRL_L, true);
    c.on_key(ALT_L, true);
    c.on_key(B, true);
    assert_eq!(c.on_key(X, true), ChordAction::Forward);
    // Disarmed: a later `d` must NOT quit.
    assert_eq!(c.on_key(D, true), ChordAction::Forward);
}

#[test]
fn b_without_the_modifiers_does_not_arm() {
    let mut c = ctrl_alt_b();
    assert_eq!(c.on_key(B, true), ChordAction::Forward);
    assert_eq!(c.on_key(D, true), ChordAction::Forward);
}

#[test]
fn releasing_the_modifiers_before_the_second_key_still_completes() {
    // Nobody holds Ctrl+Alt while pressing d. If the release disarmed us the
    // chord would be nearly untriggerable in practice, and would feel flaky
    // rather than broken -- the worst kind of bug to report.
    let mut c = ctrl_alt_b();
    c.on_key(CTRL_L, true);
    c.on_key(ALT_L, true);
    c.on_key(B, true);
    c.on_key(ALT_L, false);
    c.on_key(CTRL_L, false);
    assert_eq!(c.on_key(D, true), ChordAction::Quit);
}

#[test]
fn key_releases_do_not_complete_the_chord() {
    // Only presses act; otherwise the release of `d` fires a second time.
    let mut c = ctrl_alt_b();
    c.on_key(CTRL_L, true);
    c.on_key(ALT_L, true);
    c.on_key(B, true);
    assert_eq!(c.on_key(D, false), ChordAction::Forward);
}

#[test]
fn the_super_prefix_variant_works_the_same() {
    let mut c = Chord::new(Prefix::SuperB);
    c.on_key(SUPER_L, true);
    assert_eq!(c.on_key(B, true), ChordAction::Forward);
    assert_eq!(c.on_key(H, true), ChordAction::Minimize);
}

#[test]
fn the_right_hand_control_modifier_arms_the_chord_too() {
    let mut c = ctrl_alt_b();
    c.on_key(CTRL_R, true);
    c.on_key(ALT_L, true);
    c.on_key(B, true);
    assert_eq!(c.on_key(D, true), ChordAction::Quit);
}
