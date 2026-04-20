//! XDamage integration: monitors root window damage events and maps
//! damaged pixel rectangles to tile grid coordinates.

use std::collections::HashSet;

use x11rb::connection::Connection;
use x11rb::protocol::damage::{self, ConnectionExt as DamageExt};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use ghostframe_lib::tile::TILE_SIZE;

/// Tracks X11 damage events on the root window.
pub struct XDamageMonitor {
    conn: RustConnection,
    damage_id: damage::Damage,
}

impl XDamageMonitor {
    /// Initialize XDamage on the root window.
    /// Returns `None` if the XDamage extension is not available.
    pub fn new() -> Option<Self> {
        let (conn, screen_num) = RustConnection::connect(None).ok()?;

        // Query and initialize the damage extension.
        // This also registers extension info so events are properly parsed.
        conn.damage_query_version(1, 1).ok()?.reply().ok()?;

        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;

        // Create damage object on root window, reporting RawRectangles
        let damage_id = conn.generate_id().ok()?;
        conn.damage_create(damage_id, root, damage::ReportLevel::RAW_RECTANGLES)
            .ok()?;

        conn.flush().ok()?;

        Some(Self { conn, damage_id })
    }

    /// Drain all pending damage events and return the set of dirty tile coordinates.
    /// Non-blocking: returns empty vec if no damage events are pending.
    pub fn drain_damage(&self) -> Vec<(u32, u32)> {
        let mut tile_set = HashSet::new();

        // Drain pending events FIRST, then subtract. This ensures we consume
        // all notifications before resetting the damage region — otherwise
        // events queued between subtract and poll could be lost.
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(Event::DamageNotify(ev))) => {
                    let x = ev.area.x.max(0) as u32;
                    let y = ev.area.y.max(0) as u32;
                    let w = ev.area.width as u32;
                    let h = ev.area.height as u32;

                    // Map pixel rect to tile coords
                    let tx_start = x / TILE_SIZE;
                    let ty_start = y / TILE_SIZE;
                    let tx_end = (x + w).div_ceil(TILE_SIZE);
                    let ty_end = (y + h).div_ceil(TILE_SIZE);

                    for ty in ty_start..ty_end {
                        for tx in tx_start..tx_end {
                            tile_set.insert((tx, ty));
                        }
                    }
                }
                Ok(Some(_)) => {
                    // Ignore non-damage events
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        // Subtract (reset) damage region after draining all events.
        let _ = self.conn.damage_subtract(self.damage_id, 0u32, 0u32);
        let _ = self.conn.flush();

        tile_set.into_iter().collect()
    }
}

impl Drop for XDamageMonitor {
    fn drop(&mut self) {
        let _ = self.conn.damage_destroy(self.damage_id);
    }
}
