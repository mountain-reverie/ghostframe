use ghostframe_client_native::event::{ClientEvent, EventQueue};

fn fd_is_readable(fd: std::os::fd::RawFd) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: pfd is a valid single-element array for the duration of the call.
    unsafe { libc::poll(&mut pfd, 1, 0) > 0 }
}

#[test]
fn queue_signals_its_fd_and_drains_in_order() {
    let q = EventQueue::new().expect("queue");
    assert!(q.pop().is_none());
    assert!(
        !fd_is_readable(q.as_raw_fd()),
        "empty queue must not be readable"
    );

    q.push(ClientEvent::Connected);
    q.push(ClientEvent::Resized {
        width: 800,
        height: 600,
    });
    assert!(fd_is_readable(q.as_raw_fd()), "eventfd not signalled");

    assert_eq!(q.pop(), Some(ClientEvent::Connected));
    assert_eq!(
        q.pop(),
        Some(ClientEvent::Resized {
            width: 800,
            height: 600
        })
    );
    assert!(q.pop().is_none());
}

#[test]
fn fd_clears_once_the_queue_is_drained() {
    let q = EventQueue::new().expect("queue");
    q.push(ClientEvent::Connected);
    q.push(ClientEvent::Connected);
    while q.pop().is_some() {}
    assert!(
        !fd_is_readable(q.as_raw_fd()),
        "fd still readable after drain; the host would spin at 100% CPU"
    );
}

#[test]
fn fd_stays_readable_while_events_remain() {
    // A partial drain must leave the fd readable, or the host sleeps with
    // work still queued.
    let q = EventQueue::new().expect("queue");
    q.push(ClientEvent::Connected);
    q.push(ClientEvent::FrameReady { frame_id: 1 });
    assert_eq!(q.pop(), Some(ClientEvent::Connected));
    assert!(
        fd_is_readable(q.as_raw_fd()),
        "fd cleared while an event remained"
    );
}

#[test]
fn concurrent_pushes_are_all_delivered() {
    use std::sync::Arc;
    let q = Arc::new(EventQueue::new().expect("queue"));
    let mut handles = Vec::new();
    for t in 0..4u32 {
        let q = Arc::clone(&q);
        handles.push(std::thread::spawn(move || {
            for i in 0..250u32 {
                q.push(ClientEvent::FrameReady {
                    frame_id: t * 1000 + i,
                });
            }
        }));
    }
    for h in handles {
        h.join().expect("thread");
    }

    let mut n = 0;
    while q.pop().is_some() {
        n += 1;
    }
    assert_eq!(n, 1000, "lost events under concurrent push");
    assert!(!fd_is_readable(q.as_raw_fd()));
}

#[test]
fn the_fd_is_close_on_exec() {
    // This library gets linked into arbitrary host applications; a leaked
    // fd across exec is a real bug there.
    //
    // The queue is bound to a variable (not a temporary) so it stays
    // alive across the fcntl call below: `EventQueue::new().expect(..).as_raw_fd()`
    // would drop the temporary `EventQueue` -- and close its fd -- at the
    // end of that `let` statement, before fcntl ever saw it.
    let q = EventQueue::new().expect("queue");
    let fd = q.as_raw_fd();
    // SAFETY: fd is valid and owned by `q`, which is still alive.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(flags >= 0, "fcntl failed");
    assert_ne!(flags & libc::FD_CLOEXEC, 0, "eventfd is not CLOEXEC");
}
