//! The prefix chord: `<prefix>` then `d` (quit) or `h` (minimize).
//!
//! Modelled on tmux's prefix. Every key, INCLUDING the prefix, is forwarded
//! to the remote; only the completing `d`/`h` is swallowed. That keeps input
//! latency and ordering untouched, at the cost of the remote occasionally
//! seeing a stray `b`.

// X11 keysyms for the modifiers we recognise as part of a prefix chord.
const CONTROL_L: u32 = 0xffe3;
const CONTROL_R: u32 = 0xffe4;
const ALT_L: u32 = 0xffe9;
const ALT_R: u32 = 0xffea;
const SUPER_L: u32 = 0xffeb;
const SUPER_R: u32 = 0xffec;

const KEY_B: u32 = 0x62;
const KEY_D: u32 = 0x64;
const KEY_H: u32 = 0x68;

/// Which prefix chord arms the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefix {
    CtrlAltB,
    SuperB,
}

/// What the caller should do with a key event after feeding it to [`Chord`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordAction {
    /// Forward the key event to the remote unchanged.
    Forward,
    /// Swallow this key event and quit the client.
    Quit,
    /// Swallow this key event and minimize the window.
    Minimize,
}

/// Tracks held modifiers and whether the prefix has been struck, so that
/// `d`/`h` immediately after complete the chord.
pub struct Chord {
    prefix: Prefix,
    // Held state of each modifier we care about, tracked individually so
    // that releasing one (or both) after the prefix key doesn't matter.
    ctrl_held: bool,
    alt_held: bool,
    super_held: bool,
    /// Set when the prefix combination has just been struck; cleared by any
    /// key press other than the completing `d`/`h`.
    armed: bool,
}

impl Chord {
    pub fn new(prefix: Prefix) -> Self {
        Self {
            prefix,
            ctrl_held: false,
            alt_held: false,
            super_held: false,
            armed: false,
        }
    }

    /// Feed one key event (press or release) into the state machine.
    ///
    /// Returns the action the caller should take: forward the event as
    /// normal, or swallow it because it completed the chord.
    pub fn on_key(&mut self, keysym: u32, down: bool) -> ChordAction {
        match keysym {
            CONTROL_L | CONTROL_R => {
                self.ctrl_held = down;
                return ChordAction::Forward;
            }
            ALT_L | ALT_R => {
                self.alt_held = down;
                return ChordAction::Forward;
            }
            SUPER_L | SUPER_R => {
                self.super_held = down;
                return ChordAction::Forward;
            }
            _ => {}
        }

        if !down {
            // Only presses act on / complete the chord; a release never
            // arms, completes, or (by itself) disarms it.
            return ChordAction::Forward;
        }

        if self.prefix_struck(keysym) {
            self.armed = true;
            return ChordAction::Forward;
        }

        if self.armed {
            self.armed = false;
            match keysym {
                KEY_D => return ChordAction::Quit,
                KEY_H => return ChordAction::Minimize,
                _ => {}
            }
        }

        ChordAction::Forward
    }

    /// Whether this press is the final key of the configured prefix
    /// (assuming the required modifiers are currently held).
    fn prefix_struck(&self, keysym: u32) -> bool {
        if keysym != KEY_B {
            return false;
        }
        match self.prefix {
            Prefix::CtrlAltB => self.ctrl_held && self.alt_held,
            Prefix::SuperB => self.super_held,
        }
    }
}
