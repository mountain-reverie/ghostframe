//! X11 input injection via XTest extension.
//!
//! Implements `ghostframe_lib::transport::input_inject::InputInjector`
//! against the local X server (DISPLAY=:1 by default). One protocol
//! call per event over a dedicated x11rb connection — separate from
//! the capture + XDamage connections so an `XTestFakeInput` write
//! can't deadlock against a capture-side `GetImage` reply.
//!
//! Spec: docs/superpowers/specs/2026-06-13-input-forwarding-design.md

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::sync::Mutex;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as XProtoExt;
use x11rb::protocol::xtest::ConnectionExt as XTestExt;
use x11rb::rust_connection::RustConnection;

use ghostframe_lib::transport::input_inject::InputInjector;

pub struct XTestInjector {
    conn: RustConnection,
    root: u32,
    /// Lazily-built `keysym → keycode` lookup, populated on first miss
    /// via `get_keyboard_mapping`. Behind a Mutex because `key()` is
    /// `&self` (the trait promises `Send + Sync`).
    keysym_to_keycode: Mutex<HashMap<u32, u8>>,
}

impl XTestInjector {
    /// Connect to the local X server and confirm the XTEST extension is
    /// present. Errors if no DISPLAY or XTEST is missing — the caller
    /// (xdaemon's main) logs and continues with no injector.
    pub fn new() -> Result<Self> {
        let display_env = std::env::var("DISPLAY").unwrap_or_else(|_| "<unset>".to_string());
        let (conn, screen_num) =
            x11rb::connect(None).map_err(|e| anyhow!("x11rb::connect: {e}"))?;
        let root = conn.setup().roots[screen_num].root;

        // Confirm XTEST extension is available.
        let ext = conn
            .query_extension(b"XTEST")
            .context("query_extension(XTEST)")?
            .reply()
            .context("XTEST query_extension reply")?;
        if !ext.present {
            return Err(anyhow!("XTEST extension not present on this Xorg build"));
        }

        tracing::info!(
            display = %display_env,
            screen = screen_num,
            root = root,
            "XTestInjector connected"
        );

        Ok(Self {
            conn,
            root,
            keysym_to_keycode: Mutex::new(HashMap::new()),
        })
    }
}

impl XTestInjector {
    /// Look up a keysym → keycode mapping via the X server's current
    /// keymap. Caches successful lookups. Returns `None` if no keycode
    /// in the server's table corresponds to this keysym.
    fn keysym_to_keycode(&self, keysym: u32) -> Option<u8> {
        // Fast path: cache hit.
        if let Some(kc) = self.keysym_to_keycode.lock().unwrap().get(&keysym) {
            return Some(*kc);
        }

        // Pull the full keymap on first miss. Servers typically have
        // ~256 keycodes × ~6 keysyms-per-keycode = small.
        let setup = self.conn.setup();
        let min_keycode = setup.min_keycode;
        let max_keycode = setup.max_keycode;
        let count = (max_keycode - min_keycode) + 1;
        let map = self
            .conn
            .get_keyboard_mapping(min_keycode, count)
            .ok()?
            .reply()
            .ok()?;
        let per = map.keysyms_per_keycode as usize;
        if per == 0 {
            return None;
        }

        // Rebuild the cache from scratch — cheap and avoids partial-miss
        // questions.
        let mut cache = self.keysym_to_keycode.lock().unwrap();
        cache.clear();
        for (i, syms) in map.keysyms.chunks(per).enumerate() {
            let kc = min_keycode + i as u8;
            for &sym in syms {
                if sym != 0 {
                    cache.entry(sym).or_insert(kc);
                }
            }
        }
        cache.get(&keysym).copied()
    }
}

impl XTestInjector {
    /// Fire one `xtest_fake_input` request and flush. Logs any X11 error
    /// at warn level — silent swallowing made input bugs invisible.
    fn fake_input(&self, kind: &'static str, event: u8, detail: u8, x: i16, y: i16) {
        match self
            .conn
            .xtest_fake_input(event, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)
        {
            Ok(cookie) => {
                if let Err(e) = cookie.check() {
                    tracing::warn!(kind, event, detail, error = %e, "xtest_fake_input X11 error");
                }
            }
            Err(e) => {
                tracing::warn!(kind, event, detail, error = %e, "xtest_fake_input send failed");
            }
        }
        if let Err(e) = self.conn.flush() {
            tracing::warn!(kind, error = %e, "x11 flush failed");
        }
    }
}

impl InputInjector for XTestInjector {
    fn pointer_move(&self, x: i16, y: i16) {
        self.fake_input(
            "pointer_move",
            x11rb::protocol::xproto::MOTION_NOTIFY_EVENT,
            0,
            x,
            y,
        );
    }

    fn pointer_button(&self, x: i16, y: i16, button: u8, down: bool) {
        // Move first so the click lands where the user expects, in case
        // a `pointermove` got coalesced away client-side.
        self.pointer_move(x, y);
        let event = if down {
            x11rb::protocol::xproto::BUTTON_PRESS_EVENT
        } else {
            x11rb::protocol::xproto::BUTTON_RELEASE_EVENT
        };
        self.fake_input("pointer_button", event, button, x, y);
    }

    fn wheel(&self, dx: i16, dy: i16) {
        // X11 wheel = button 4 (up) / 5 (down) / 6 (left) / 7 (right);
        // one press+release pair per tick.
        let click_pair = |button: u8, count: u16| {
            for _ in 0..count {
                self.fake_input(
                    "wheel_press",
                    x11rb::protocol::xproto::BUTTON_PRESS_EVENT,
                    button,
                    0,
                    0,
                );
                self.fake_input(
                    "wheel_release",
                    x11rb::protocol::xproto::BUTTON_RELEASE_EVENT,
                    button,
                    0,
                    0,
                );
            }
        };
        if dy < 0 {
            click_pair(4, dy.unsigned_abs());
        } else if dy > 0 {
            click_pair(5, dy as u16);
        }
        if dx < 0 {
            click_pair(6, dx.unsigned_abs());
        } else if dx > 0 {
            click_pair(7, dx as u16);
        }
    }

    fn key(&self, keysym: u32, down: bool) {
        let Some(keycode) = self.keysym_to_keycode(keysym) else {
            tracing::warn!(
                keysym = format!("0x{:08x}", keysym),
                "no keycode for keysym; skipping key event"
            );
            return;
        };
        let event = if down {
            x11rb::protocol::xproto::KEY_PRESS_EVENT
        } else {
            x11rb::protocol::xproto::KEY_RELEASE_EVENT
        };
        self.fake_input("key", event, keycode, 0, 0);
    }
}
