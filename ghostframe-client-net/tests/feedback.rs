mod common;

use common::{connected_session, pump_with_wt};

/// The Hello queued by `ClientCore::new` at construction must survive the
/// gap between "queued" and "a feedback stream exists to carry it" — that
/// gap is exactly what `pending_stream_out` exists to bridge. Once the
/// WebTransport session is ready, `ClientNet` opens a bidi stream and
/// flushes it there; the server routes data on any bidi stream other than
/// the session stream into its feedback queue.
#[test]
fn hello_reaches_the_server_on_the_feedback_stream() {
    let (mut client, mut server, mut wt, mut now_us, base) = connected_session();
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);

    let feedback = wt.drain_feedback();
    assert!(
        feedback.iter().any(|m| m.first() == Some(&0x03)),
        "server must receive the Hello message (type 0x03), got {feedback:?}"
    );
}
