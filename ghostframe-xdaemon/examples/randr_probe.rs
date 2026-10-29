//! RandR investigation tool for the ghostframe-server X11 rework (design
//! section 4: "record the RandR event stream for `xrandr` and for
//! `XrandrDisplay` and close the difference").
//!
//! ```text
//! randr_probe state            # screen, outputs, modes, CRTC, panning
//! randr_probe record SECS      # print every RandR/root event for SECS seconds
//! randr_probe apply W H        # XrandrDisplay::set_output(W, H, 1000)
//! ```
//!
//! `apply` CHANGES THE PHYSICAL MONITOR MODE of `$DISPLAY`. On a live desktop
//! run it only with the owner's go-ahead, and restore with
//! `xrandr --output <OUT> --mode <WxH>` afterwards.
//!
//! An example, not a `src/bin/*`, for the reason `cvt_sweep.rs` gives:
//! `cargo build -p` (the e2e image) does not build examples, while
//! `clippy --all-targets` still lints them. It includes the shipped modules
//! verbatim via `#[path]`, so it always exercises the code that ships.

#[path = "../src/cvt.rs"]
mod cvt;
#[path = "../src/display.rs"]
mod display;
#[path = "../src/randr_events.rs"]
mod randr_events;

use std::time::Duration;

use ghostframe_lib::transport::display::DisplayController;
use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xproto::ConnectionExt as _;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["state"] => state(),
        ["record", secs] => record(secs.parse()?),
        ["apply", w, h] => apply(w.parse()?, h.parse()?),
        _ => {
            eprintln!("usage: randr_probe state | record SECS | apply W H");
            std::process::exit(2);
        }
    }
}

fn mode_name(res: &randr::GetScreenResourcesReply, id: randr::Mode) -> String {
    let mut off = 0usize;
    for m in &res.modes {
        let end = off + usize::from(m.name_len);
        if m.id == id {
            return String::from_utf8_lossy(&res.names[off..end]).into_owned();
        }
        off = end;
    }
    format!("#{id}")
}

fn state() -> Result<()> {
    let (conn, n) = x11rb::connect(None)?;
    let root = conn.setup().roots[n].root;
    conn.randr_query_version(1, 5)?.reply()?;
    let geom = conn.get_geometry(root)?.reply()?;
    let range = conn.randr_get_screen_size_range(root)?.reply()?;
    println!(
        "screen {}x{} (range {}x{}..{}x{})",
        geom.width,
        geom.height,
        range.min_width,
        range.min_height,
        range.max_width,
        range.max_height
    );
    let res = conn.randr_get_screen_resources(root)?.reply()?;
    for &output in &res.outputs {
        let info = conn
            .randr_get_output_info(output, res.config_timestamp)?
            .reply()?;
        println!(
            "output {} ({}) {:?} crtc={} {}x{}mm preferred={}",
            String::from_utf8_lossy(&info.name),
            output,
            info.connection,
            info.crtc,
            info.mm_width,
            info.mm_height,
            info.num_preferred
        );
        for (i, &m) in info.modes.iter().enumerate() {
            let mi = res.modes.iter().find(|x| x.id == m);
            if let Some(mi) = mi {
                let total = u64::from(mi.htotal) * u64::from(mi.vtotal);
                let hz = (u64::from(mi.dot_clock) * 100)
                    .checked_div(total)
                    .unwrap_or(0);
                println!(
                    "  mode {} ({m}) {}x{} {}.{:02}Hz{}",
                    mode_name(&res, m),
                    mi.width,
                    mi.height,
                    hz / 100,
                    hz % 100,
                    if i < usize::from(info.num_preferred) {
                        " preferred"
                    } else {
                        ""
                    }
                );
            }
        }
        if info.crtc != 0 {
            let c = conn
                .randr_get_crtc_info(info.crtc, res.config_timestamp)?
                .reply()?;
            println!(
                "  crtc {} mode {} ({}) {}x{}+{}+{} rotation {:?}",
                info.crtc,
                mode_name(&res, c.mode),
                c.mode,
                c.width,
                c.height,
                c.x,
                c.y,
                c.rotation
            );
            let p = conn.randr_get_panning(info.crtc)?.reply()?;
            println!(
                "  panning {}x{}+{}+{} track {}x{}+{}+{} border {},{},{},{}",
                p.width,
                p.height,
                p.left,
                p.top,
                p.track_width,
                p.track_height,
                p.track_left,
                p.track_top,
                p.border_left,
                p.border_top,
                p.border_right,
                p.border_bottom
            );
        }
    }
    Ok(())
}

fn record(secs: u64) -> Result<()> {
    let rec = randr_events::Recorder::connect(None)?;
    eprintln!("recording for {secs}s");
    let window = Duration::from_secs(secs);
    let events = rec.collect(window, window)?;
    for (at, e) in &events {
        println!("{:>8.1}ms {e:?}", at.as_secs_f64() * 1e3);
    }
    println!("--- shape");
    for line in randr_events::shape(&events) {
        println!("{line}");
    }
    Ok(())
}

fn apply(w: u16, h: u16) -> Result<()> {
    let ctl = display::XrandrDisplay::new(None).map_err(|e| e.to_string())?;
    ctl.set_output(w, h, 1000).map_err(|e| e.to_string())?;
    println!("set_output({w}, {h}, 1000) returned Ok");
    Ok(())
}
