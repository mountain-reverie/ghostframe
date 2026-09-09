mod common;

use common::{connected_session, pump_with_wt};

/// `ClientCore` arms deadlines for ACK batching, NACK debounce, the tail
/// sweep, and periodic `ReceiverFeedback` at construction, and never
/// disarms all of them (`ClientCore::poll_timeout`'s doc comment claims
/// this explicitly). Nothing calls `on_timeout` on its own, so this checks
/// both that `poll_timeout` really is always armed once a session exists,
/// and that firing it at its own deadline actually produces a feedback
/// message the server receives on the feedback stream opened for gap 1.
#[test]
fn poll_timeout_fires_the_core_timers() {
    let (mut client, mut server, mut wt, mut now_us, base) = connected_session();
    let deadline = client.poll_timeout().expect("core always arms a timeout");
    assert!(deadline > now_us);

    now_us = deadline;
    client.on_timeout(now_us);
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 8);

    assert!(
        !wt.drain_feedback().is_empty(),
        "a fired feedback timer must produce a stream message"
    );
}
