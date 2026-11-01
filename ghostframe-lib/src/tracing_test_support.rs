//! Test support for tests that capture `tracing` output through a scoped
//! (thread-local) subscriber.

/// Keep tracing-core's dispatcher registry out of its "just one" mode for as
/// long as the returned value lives.
///
/// tracing-core caches each callsite's `Interest` globally. A callsite is
/// registered by whichever thread first executes it. While the registry
/// believes exactly one dispatcher exists (`Dispatchers::has_just_one`,
/// tracing-core `callsite.rs`), that registration asks only the *registering
/// thread's* current dispatcher (`Rebuilder::JustOne` -> `get_default`). In
/// a parallel suite that thread is usually another test with no subscriber,
/// the answer is `Interest::never()`, and it is cached for every thread.
/// The capturing test's own `set_default` cannot repair it: the callsite was
/// not registered yet when the cache was rebuilt, so the event is silently
/// skipped.
///
/// With two live dispatchers the registry takes the read-locked path and
/// combines the answer of every live dispatcher, the capturing test's
/// included, so the interest is at worst `sometimes` and each thread's own
/// `enabled` decides.
///
/// Call this BEFORE installing the capturing subscriber and bind it so it is
/// dropped AFTER the subscriber's guard (declare it first).
#[must_use = "bind it to a variable that outlives the capturing subscriber"]
pub(crate) fn pin_dispatcher_registry() -> tracing::Dispatch {
    tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default())
}
