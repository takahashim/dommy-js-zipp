//! Running engine work with the GVL released, and delivering Ruby interrupts.

use crate::*;

/// The outcome of [`without_gvl`].
pub(crate) enum Nogvl<R> {
    Done(R),
    /// Ruby had an interrupt pending, so the function was never called.
    NotRun,
    /// The function panicked (caught at the FFI boundary).
    Panicked,
}

/// What a GVL-free run's unblocking function (`ubf`) acts on: Ruby calls it
/// from another thread when an interrupt (`Thread#raise`/`#kill`, a signal,
/// `Timeout`) targets the thread running the engine. It sets the VM's abort
/// flag — the engine stops at its next poll — and records that the stop was
/// Ruby's doing, so the caller delivers the interrupt rather than reporting a
/// timeout.
pub(crate) struct Interrupter {
    pub(crate) abort: Arc<AtomicBool>,
    pub(crate) interrupted: AtomicBool,
}

/// The unblocking function [`without_gvl`] hands Ruby with an [`Interrupter`].
///
/// # Safety
///
/// `data` must point to an `Interrupter` that stays alive for the call. Ruby
/// calls this from another thread, so it does nothing but atomic stores.
pub(crate) unsafe extern "C" fn ubf_interrupt(data: *mut std::os::raw::c_void) {
    // SAFETY: `without_gvl` passes `data` as a pointer to the `Interrupter`
    // its caller borrowed for the whole call, and Ruby calls the unblocking
    // function only while that call is in progress. A shared reference is
    // enough: both fields are atomics, so another thread may hold one too.
    let it = unsafe { &*(data as *const Interrupter) };
    it.interrupted.store(true, Ordering::Relaxed);
    it.abort.store(true, Ordering::Relaxed);
}

/// Run `f` with the GVL released, so other Ruby threads can run while a pure
/// engine evaluation proceeds. A VM with any Ruby bridge installed is not run
/// through this path. `_ruby` shows the caller is on a Ruby thread holding
/// the GVL, which `rb_thread_call_without_gvl2` requires.
///
/// `rb_thread_call_without_gvl2` is used deliberately: `..._without_gvl` checks
/// interrupts while re-acquiring the GVL and can raise there, which would unwind
/// (via a Ruby longjmp that skips Rust destructors) past the caller's entry
/// guard. The `2` variant instead defers a pending interrupt to a later check —
/// and, if one is ALREADY pending on entry, returns without calling `f` at all
/// ([`Nogvl::NotRun`]); the caller must deliver the interrupt and retry.
///
/// The result travels through a caller-owned slot, never through the returned
/// pointer (which is NULL in the not-run case), and a panic is caught inside
/// the trampoline so it never unwinds across the `extern "C"` boundary.
///
/// With an `interrupter`, a Ruby interrupt stops the engine (see
/// [`Interrupter`]); without one the run cannot be interrupted, and a pending
/// interrupt is delivered after it returns.
///
/// # Safety
///
/// `f` must not use the Ruby API, directly or through anything it calls (a
/// Ruby callback, a Ruby object's method): it runs without the GVL.
pub(crate) unsafe fn without_gvl<F, R>(_ruby: &Ruby, f: F, interrupter: Option<&Interrupter>) -> Nogvl<R>
where
    F: FnOnce() -> R,
{
    struct Slot<F, R> {
        f: Option<F>,
        out: Option<std::thread::Result<R>>,
    }
    unsafe extern "C" fn trampoline<F, R>(data: *mut std::os::raw::c_void) -> *mut std::os::raw::c_void
    where
        F: FnOnce() -> R,
    {
        // SAFETY: `data` is the `Slot<F, R>` on `without_gvl`'s stack, which
        // does not touch it until `rb_thread_call_without_gvl2` returns; Ruby
        // calls this at most once, on the calling thread, within that call.
        let slot = unsafe { &mut *(data as *mut Slot<F, R>) };
        if let Some(f) = slot.f.take() {
            slot.out = Some(std::panic::catch_unwind(AssertUnwindSafe(f)));
        }
        std::ptr::null_mut()
    }
    let mut slot = Slot { f: Some(f), out: None };
    let (ubf, ubf_data) = match interrupter {
        Some(it) => (
            Some(ubf_interrupt as unsafe extern "C" fn(*mut std::os::raw::c_void)),
            it as *const Interrupter as *mut std::os::raw::c_void,
        ),
        None => (None, std::ptr::null_mut()),
    };
    // SAFETY: `_ruby` shows this thread holds the GVL, as the call requires.
    // `trampoline` gets a pointer to `slot`, alive across the call; it uses no
    // Ruby API (`f` does not, by this function's contract) and catches any
    // panic, so nothing unwinds into Ruby. `ubf_interrupt` gets a pointer to
    // `interrupter`, borrowed for the whole call (see there).
    unsafe {
        rb_sys::rb_thread_call_without_gvl2(
            Some(trampoline::<F, R>),
            &mut slot as *mut Slot<F, R> as *mut std::os::raw::c_void,
            ubf,
            ubf_data,
        );
    }
    match slot.out {
        Some(Ok(r)) => Nogvl::Done(r),
        Some(Err(_)) => Nogvl::Panicked,
        None => Nogvl::NotRun,
    }
}

/// Deliver any interrupt pending on this thread (`Thread#raise`, `Thread#kill`,
/// a trapped signal, `Timeout`) as an `Err`, without a longjmp through Rust
/// frames. Returning that error to Ruby re-raises (or re-jumps) it. `_ruby`
/// shows the caller holds the GVL.
pub(crate) fn deliver_interrupts(_ruby: &Ruby) -> Result<(), Error> {
    magnus::rb_sys::protect(|| {
        // SAFETY: `_ruby` shows this thread holds the GVL, as the call
        // requires, and `protect` turns the raise or throw it may make into
        // an `Err` rather than a longjmp through Rust frames.
        unsafe { rb_sys::rb_thread_check_ints() };
        rb_sys::Qnil as rb_sys::VALUE
    })
    .map(|_| ())
}

/// Sleep for `d` with the GVL released, then deliver any interrupt that
/// arrived meanwhile, so a waiting event loop stays responsive to
/// `Thread#raise`/`Timeout`/signals and does not stall other threads.
pub(crate) fn sleep_without_gvl(ruby: &Ruby, d: Duration) -> Result<(), Error> {
    // SAFETY: the closure only sleeps.
    let _ = unsafe { without_gvl(ruby, || std::thread::sleep(d), None) };
    deliver_interrupts(ruby)
}
