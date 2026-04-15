fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!("ghostframe-xdaemon starting (M0: ping server mode)");
    // Task 8 replaces this with the real I/O bridge entry point.
    std::thread::park();
}
