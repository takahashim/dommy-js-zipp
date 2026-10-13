//! ZippRb — a Ruby binding for the Zipp engine.
//!
//! The engine's `ScriptState` is `!Send` (the JIT pins heap/register pointers,
//! so a VM must stay on the thread that built it). Magnus's typed-data wrapper
//! requires `Send`, and the plan forbids faking it with `unsafe impl Send`, so
//! VMs live in a thread-local registry keyed by an integer id; the Ruby
//! `ZippRb::VM` class (defined in lib/zipp_rb.rb) holds that id and delegates
//! to the module functions below.
//!
//! Ruby callbacks are registered as JS globals whose bodies call
//! `__zippHostCallValue`; the procs themselves live in a Ruby-side Hash
//! (`ZippRb::CALLBACKS`) so the Ruby GC roots them, and the Rust handler looks
//! them up by kind on each call.
//!
//! Every host entry goes through an [`Entry`]: it takes the VM out of the
//! registry, catches an engine panic (the VM is then discarded — "poisoned" —
//! rather than reused in an unknown state), puts the VM back on every exit
//! path, and finally re-raises whatever Ruby exception or non-local exit a
//! callback raised during the entry (see [`finish`]).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use magnus::{
    block::Proc, error::ErrorType, function, prelude::*, r_hash::ForEach, rb_sys::AsRawValue,
    Class, Error, Exception, ExceptionClass, Float, Integer, RArray, RClass, RHash, RModule,
    RObject, RString, Ruby, Symbol, Value,
};

use zipp_vm::embed::{
    compile_script, ConsoleStream, HostCallValues, HostCtx, HostHandle, HostValue, JsError,
    ScriptState,
};

/// VM ids are process-global and monotonic, so an id minted on one thread can
/// never denote a different VM on another thread. Combined with the
/// thread-local registry (a VM lives only on its owner's thread), a cross-thread
/// use misses the registry and is refused instead of silently hitting an
/// unrelated VM that happens to share the id.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Handles whose Ruby `Handle` was garbage-collected without `release`, as
/// `(vm id, handle id)`. A finalizer may run on any thread and at any moment
/// (even mid-entry), so it only queues; the owning thread releases the engine
/// root on its next entry into that VM. `RELEASE_QUEUED` keeps the common
/// empty case to one atomic load.
static RELEASE_QUEUE: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
static RELEASE_QUEUED: AtomicBool = AtomicBool::new(false);

/// Native memory a fresh VM holds beyond its JS heap (compiled builtins,
/// tables), reported to the Ruby GC so that unreferenced VMs are finalized
/// before process memory balloons. Measured at roughly 250 KB of RSS per VM.
const VM_BASE_BYTES: i64 = 256 * 1024;

/// Nesting depth past which a Ruby Array/Hash crossing into JS becomes `null`
/// (and a cycle becomes `null` where it closes) — the engine's own limit for
/// a host value tree, so nothing deeper would survive the crossing anyway.
const MAX_HOST_DEPTH: usize = 64;

/// Ruby exceptions a callback raised during one entry, kept for re-raising.
/// Only the most recent ones matter (a JS loop that catches thousands of them
/// must not grow this without bound).
const MAX_PENDING_ERRORS: usize = 16;

/// The JS error message a callback produces while a Ruby interrupt is pending.
const INTERRUPT_PENDING_MSG: &str = "Error: a Ruby interrupt is pending";

/// A scheduled timer for a VM.
struct TimerEntry {
    id: u64,
    due_ms: u64,
    repeat_ms: Option<u64>,
}

/// The JS `setTimeout`/`setInterval` shims installed by `enable_timers`. Their
/// registrations cross to Rust via `__zippHostCallValue("zipp:timer:...")`;
/// Rust fires them by calling `__zipp_fire_timer`. A one-shot timer deletes
/// its closure before running, so fired timers do not accumulate.
const TIMER_SHIMS: &str = r#"
globalThis.__zipp_timer_cbs = {};
globalThis.__zipp_timer_seq = 1;
function setTimeout(fn, delay, ...args) {
  const id = globalThis.__zipp_timer_seq++;
  globalThis.__zipp_timer_cbs[id] = () => { delete globalThis.__zipp_timer_cbs[id]; fn(...args); };
  __zippHostCallValue("zipp:timer:set", id, Math.max(0, Number(delay) || 0), 0);
  return id;
}
function clearTimeout(id) {
  delete globalThis.__zipp_timer_cbs[id];
  __zippHostCallValue("zipp:timer:clear", id, 0, 0);
}
function setInterval(fn, delay, ...args) {
  const id = globalThis.__zipp_timer_seq++;
  globalThis.__zipp_timer_cbs[id] = () => fn(...args);
  __zippHostCallValue("zipp:timer:set", id, Math.max(1, Number(delay) || 1), Math.max(1, Number(delay) || 1));
  return id;
}
function clearInterval(id) {
  delete globalThis.__zipp_timer_cbs[id];
  __zippHostCallValue("zipp:timer:clear", id, 0, 0);
}
globalThis.__zipp_fire_timer = (id) => { const cb = globalThis.__zipp_timer_cbs[id]; if (cb) cb(); };
"#;

/// The outcome of [`without_gvl`].
enum Nogvl<R> {
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
struct Interrupter {
    abort: Arc<AtomicBool>,
    interrupted: AtomicBool,
}

unsafe extern "C" fn ubf_interrupt(data: *mut std::os::raw::c_void) {
    // Valid for the whole GVL-free run: the caller holds an `Arc` to it.
    let it = unsafe { &*(data as *const Interrupter) };
    it.interrupted.store(true, Ordering::Relaxed);
    it.abort.store(true, Ordering::Relaxed);
}

/// Run `f` with the GVL released, so other Ruby threads can run while a pure
/// engine evaluation proceeds. The caller MUST guarantee `f` touches no Ruby
/// API (it only drives the engine); a VM with any Ruby bridge installed is not
/// run through this path.
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
unsafe fn without_gvl<F, R>(f: F, interrupter: Option<&Interrupter>) -> Nogvl<R>
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
/// frames. Returning that error to Ruby re-raises (or re-jumps) it.
fn deliver_interrupts() -> Result<(), Error> {
    magnus::rb_sys::protect(|| {
        unsafe { rb_sys::rb_thread_check_ints() };
        rb_sys::Qnil as rb_sys::VALUE
    })
    .map(|_| ())
}

/// Sleep for `d` with the GVL released, then deliver any interrupt that
/// arrived meanwhile, so a waiting event loop stays responsive to
/// `Thread#raise`/`Timeout`/signals and does not stall other threads.
fn sleep_without_gvl(d: Duration) -> Result<(), Error> {
    let _ = unsafe { without_gvl(|| std::thread::sleep(d), None) };
    deliver_interrupts()
}

/// Monotonic milliseconds since the extension loaded, for timer deadlines.
/// Immune to wall-clock jumps.
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Per-VM resource limits, applied around every host entry. The wall-clock
/// timeout is an engine deadline (checked where the engine polls its abort
/// flag), so no timer thread is involved; the abort flag itself is raised by a
/// Ruby interrupt (via [`Interrupter`]) or a callback's non-local exit.
struct LimitState {
    interrupter: Arc<Interrupter>,
    max_steps: u64,
    timeout_ms: Option<u64>,
    memory_limit: Option<usize>,
}

/// A non-local exit a callback raised that JS must not be able to swallow:
/// a non-`StandardError` exception (`Interrupt`, `SystemExit`, the exception
/// `Timeout` and `Thread#raise` deliver …), `Thread#kill`, or `throw`/`break`.
/// It is re-delivered when the VM returns to Ruby. An exception is kept in
/// `ZippRb::PENDING` (so the GC roots it), not here.
enum Fatal {
    Exception,
    /// `Thread#kill`: the captured `Jump(Fatal)` itself. Re-issuing the kill
    /// would be a no-op (the thread is already marked to-be-killed), so the
    /// original jump is resumed instead; it carries no Ruby object.
    Kill(Error),
    Jump(String),
}

thread_local! {
    /// Live VMs for this thread, keyed by id.
    static VMS: RefCell<HashMap<u64, ScriptState>> = RefCell::new(HashMap::new());
    /// VMs currently executing on this thread (taken out of `VMS` for the
    /// duration of a host entry). A `dispose` while a VM is in flight is
    /// refused, so the VM cannot be torn down under a running evaluation.
    static IN_FLIGHT: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
    /// VMs discarded after an engine panic.
    static POISONED: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
    /// Next callback id.
    static NEXT_FN: Cell<u64> = const { Cell::new(1) };
    /// Map a JS error `name` to the Ruby class to raise.
    static ERROR_CLASSES: RefCell<HashMap<&'static str, ExceptionClass>> =
        RefCell::new(HashMap::new());
    /// The engine reentry context active while a Ruby callback runs, for the
    /// VM currently being evaluated. Lets a nested `vm.eval`/`vm.call` from a
    /// callback drive the SAME VM through the engine's `HostCtx` (the supported
    /// reentry path) instead of the id registry. The raw pointer is valid only
    /// within the callback's dynamic scope; the handler saves and restores it.
    static ACTIVE_CTX: Cell<Option<(u64, *mut (dyn HostCtx + 'static))>> = const { Cell::new(None) };
    /// Durable JS value handles the binding has handed to Ruby, keyed by a
    /// binding id. Value is `(owning VM id, engine handle)`, so a VM's handles
    /// can be dropped when it is disposed.
    static HANDLES: RefCell<HashMap<u64, (u64, HostHandle)>> = RefCell::new(HashMap::new());
    /// Next handle id to hand out.
    static NEXT_HANDLE: Cell<u64> = const { Cell::new(1) };
    /// Next `eval_async`/`import` settlement key.
    static NEXT_SETTLE: Cell<u64> = const { Cell::new(1) };
    /// Settled `eval_async`/`import` results, by key: `(fulfilled, value)`.
    /// Delivered through the host channel, so no result is parked on
    /// `globalThis` and no polling script is compiled.
    static SETTLED: RefCell<HashMap<u64, (bool, HostValue)>> = RefCell::new(HashMap::new());
    /// Resource limits per VM (instruction budget, heap ceiling, wall timeout).
    static LIMITS: RefCell<HashMap<u64, LimitState>> = RefCell::new(HashMap::new());
    /// Scheduled timers per VM (opt-in `feature_timeout`).
    static TIMERS: RefCell<HashMap<u64, Vec<TimerEntry>>> = RefCell::new(HashMap::new());
    /// VMs with a Ruby bridge (callback, listener, module provider, timers):
    /// their entries must keep the GVL, unlike a pure eval.
    static BRIDGED: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
    /// VMs with entries in `ZippRb::PENDING`, so an entry that recorded
    /// nothing never touches the Ruby Hash.
    static HAS_PENDING: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
    /// Non-local exits awaiting re-delivery, per VM.
    static FATAL: RefCell<HashMap<u64, Fatal>> = RefCell::new(HashMap::new());
    /// Bytes reported to the Ruby GC per VM (`rb_gc_adjust_memory_usage`).
    static GC_ACCOUNTED: RefCell<HashMap<u64, i64>> = RefCell::new(HashMap::new());
}

/// Mark a VM as bridged (needs the GVL for every entry).
fn mark_bridged(id: u64) {
    BRIDGED.with(|s| {
        s.borrow_mut().insert(id);
    });
}

/// Whether a VM can run a pure evaluation without the GVL.
fn gvl_free_ok(id: u64) -> bool {
    !BRIDGED.with(|s| s.borrow().contains(&id))
}

/// Report a VM's native footprint (`heap_bytes` plus [`VM_BASE_BYTES`]) to the
/// Ruby GC as the delta since the last report, so the GC sees memory that
/// otherwise hides behind a small Ruby object. Called where the heap figure is
/// already at hand — creation, `gc!`, `memory_usage` — never per entry: the
/// engine computes `heap_bytes` with an audit walk of the heap.
fn account_memory(id: u64, heap_bytes: usize) {
    let now = VM_BASE_BYTES + heap_bytes as i64;
    let delta = GC_ACCOUNTED.with(|m| {
        let mut m = m.borrow_mut();
        let prev = m.insert(id, now).unwrap_or(0);
        now - prev
    });
    if delta != 0 {
        unsafe { rb_sys::rb_gc_adjust_memory_usage(delta as _) };
    }
}

/// Withdraw everything reported for VM `id` from the Ruby GC.
fn unaccount_memory(id: u64) {
    if let Some(prev) = GC_ACCOUNTED.with(|m| m.borrow_mut().remove(&id)) {
        unsafe { rb_sys::rb_gc_adjust_memory_usage(-prev as _) };
    }
}

/// One host entry into VM `id`: the VM is out of the registry for the entry's
/// lifetime and is put back when the entry drops — on every exit path,
/// including an early `?` return.
struct Entry {
    id: u64,
    state: Option<ScriptState>,
}

impl Entry {
    fn take(ruby: &Ruby, id: u64) -> Result<Entry, Error> {
        if POISONED.with(|s| s.borrow().contains(&id)) {
            return Err(base_error(ruby, "the VM was discarded after an engine panic"));
        }
        match VMS.with(|vms| vms.borrow_mut().remove(&id)) {
            Some(mut state) => {
                IN_FLIGHT.with(|s| {
                    s.borrow_mut().insert(id);
                });
                arm_limits(&mut state, id);
                release_queued_handles(id, &mut state);
                Ok(Entry { id, state: Some(state) })
            }
            None => Err(runtime_error(
                ruby,
                format!("unknown VM id {id} (used from another thread, or re-entered from a callback)"),
            )),
        }
    }

    fn state(&mut self) -> &mut ScriptState {
        self.state.as_mut().expect("an entry holds its VM until it drops")
    }

    /// Run engine work with the GVL held, catching a panic (the VM is then
    /// discarded: its state after an unwind through the engine is unknown).
    fn run<R>(&mut self, ruby: &Ruby, f: impl FnOnce(&mut ScriptState) -> R) -> Result<R, Error> {
        let state = self.state();
        match std::panic::catch_unwind(AssertUnwindSafe(|| f(state))) {
            Ok(r) => Ok(r),
            Err(_) => Err(self.poison(ruby)),
        }
    }

    /// Run engine work with the GVL released when the VM has no Ruby bridge
    /// (otherwise as [`Self::run`]). An interrupt pending on entry is
    /// delivered (as `Err`) instead of running; a spurious one is retried, and
    /// after a few in a row the work runs with the GVL held instead.
    fn run_nogvl<R>(&mut self, ruby: &Ruby, mut f: impl FnMut(&mut ScriptState) -> R) -> Result<R, Error> {
        if !gvl_free_ok(self.id) {
            return self.run(ruby, f);
        }
        let interrupter = LIMITS.with(|m| m.borrow().get(&self.id).map(|l| l.interrupter.clone()));
        for _ in 0..3 {
            let state = self.state.as_mut().expect("an entry holds its VM until it drops");
            let work = &mut f;
            match unsafe { without_gvl(move || work(state), interrupter.as_deref()) } {
                Nogvl::Done(r) => {
                    // The engine stopped because Ruby interrupted the thread:
                    // deliver that interrupt (Interrupt, Thread#kill, …)
                    // rather than the engine's abort error.
                    if interrupter.as_ref().is_some_and(|it| it.interrupted.load(Ordering::Relaxed)) {
                        deliver_interrupts()?;
                    }
                    return Ok(r);
                }
                Nogvl::Panicked => return Err(self.poison(ruby)),
                Nogvl::NotRun => deliver_interrupts()?,
            }
        }
        self.run(ruby, f)
    }

    /// Discard the VM after an engine panic. Its state is leaked rather than
    /// dropped: a destructor running over half-updated engine state could
    /// panic again (an abort) or worse.
    fn poison(&mut self, ruby: &Ruby) -> Error {
        if let Some(state) = self.state.take() {
            std::mem::forget(state);
        }
        IN_FLIGHT.with(|s| {
            s.borrow_mut().remove(&self.id);
        });
        POISONED.with(|s| {
            s.borrow_mut().insert(self.id);
        });
        forget_vm(ruby, self.id);
        base_error(ruby, "the Zipp engine panicked; the VM has been discarded")
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            put_vm(self.id, state);
        }
    }
}

/// Run one host entry into VM `id`, then [`finish`] it.
fn with_entry<T>(
    ruby: &Ruby,
    id: u64,
    f: impl FnOnce(&mut Entry) -> Result<T, Error>,
) -> Result<T, Error> {
    let mut entry = Entry::take(ruby, id)?;
    let result = f(&mut entry);
    drop(entry);
    finish(ruby, id, result)
}

/// Close a host entry: a non-local exit a callback raised wins over the
/// entry's own result (JS may have caught and swallowed the error standing in
/// for it); the per-entry record of catchable callback exceptions is cleared.
fn finish<T>(ruby: &Ruby, id: u64, result: Result<T, Error>) -> Result<T, Error> {
    let fatal = FATAL.with(|f| f.borrow_mut().remove(&id));
    let fatal_error = fatal.map(|f| fatal_to_error(ruby, id, f));
    if HAS_PENDING.with(|s| s.borrow_mut().remove(&id)) {
        if let Some(pending) = hash_const(ruby, "PENDING") {
            let _ = pending.delete::<_, Value>(id);
            let _ = pending.delete::<_, Value>(format!("fatal:{id}"));
        }
    }
    match fatal_error {
        Some(e) => Err(e),
        None => result,
    }
}

/// The Ruby error that re-delivers a recorded non-local exit.
fn fatal_to_error(ruby: &Ruby, id: u64, fatal: Fatal) -> Error {
    match fatal {
        Fatal::Exception => hash_const(ruby, "PENDING")
            .and_then(|p| p.get(format!("fatal:{id}")))
            .and_then(Exception::from_value)
            .map(Error::from)
            .unwrap_or_else(|| base_error(ruby, "a Ruby exception raised in a callback was lost")),
        // Resume the kill. (On the main thread Thread#kill is an exit, which
        // arrives as a SystemExit exception, not here.)
        Fatal::Kill(jump) => jump,
        Fatal::Jump(what) => base_error(
            ruby,
            &format!("a non-local exit ({what}) out of a JS callback is not supported"),
        ),
    }
}

/// Record a callback's Ruby error. A `StandardError` stays catchable by JS: it
/// becomes the returned JS error message, and the exception is kept so an
/// uncaught throw re-raises the ORIGINAL exception (class, message, backtrace)
/// in Ruby. Anything else is a non-local exit JS must not swallow: it is
/// recorded for re-delivery, the VM is asked to stop (when it has an abort
/// flag), and later callbacks in this entry fail fast.
fn capture_ruby_error(ruby: &Ruby, vm_id: u64, e: Error) -> String {
    let standard = ruby.exception_standard_error();
    match e.error_type() {
        ErrorType::Exception(exc) if exc.is_kind_of(standard) => {
            let msg: String = exc.funcall("message", ()).unwrap_or_default();
            record_pending(ruby, vm_id, &msg, *exc);
            msg
        }
        ErrorType::Error(class, msg) if class.is_inherited(standard) => msg.to_string(),
        ErrorType::Exception(exc) => {
            record_fatal_exception(ruby, vm_id, *exc);
            INTERRUPT_PENDING_MSG.to_string()
        }
        ErrorType::Error(class, msg) => {
            if let Ok(exc) = class.new_instance((msg.to_string(),)) {
                record_fatal_exception(ruby, vm_id, exc);
            }
            INTERRUPT_PENDING_MSG.to_string()
        }
        ErrorType::Jump(tag) => {
            let fatal = match tag {
                magnus::error::Tag::Fatal => Fatal::Kill(e.clone()),
                other => Fatal::Jump(other.to_string().to_lowercase()),
            };
            record_fatal(vm_id, fatal);
            INTERRUPT_PENDING_MSG.to_string()
        }
    }
}

fn record_pending(ruby: &Ruby, vm_id: u64, msg: &str, exc: Exception) {
    let Some(pending) = hash_const(ruby, "PENDING") else { return };
    HAS_PENDING.with(|s| s.borrow_mut().insert(vm_id));
    let list = match pending.get(vm_id).and_then(RArray::from_value) {
        Some(list) => list,
        None => {
            let list = ruby.ary_new();
            let _ = pending.aset(vm_id, list);
            list
        }
    };
    if list.len() >= MAX_PENDING_ERRORS {
        let _ = list.shift::<Value>();
    }
    let _ = list.push(ruby.ary_new_from_values(&[ruby.str_new(msg).as_value(), exc.as_value()]));
}

fn record_fatal_exception(ruby: &Ruby, vm_id: u64, exc: Exception) {
    if let Some(pending) = hash_const(ruby, "PENDING") {
        let _ = pending.aset(format!("fatal:{vm_id}"), exc);
        HAS_PENDING.with(|s| s.borrow_mut().insert(vm_id));
    }
    record_fatal(vm_id, Fatal::Exception);
}

/// Record a non-local exit (the first one wins) and stop the VM if it can be
/// stopped.
fn record_fatal(vm_id: u64, fatal: Fatal) {
    FATAL.with(|f| {
        f.borrow_mut().entry(vm_id).or_insert(fatal);
    });
    LIMITS.with(|m| {
        if let Some(l) = m.borrow().get(&vm_id) {
            l.interrupter.abort.store(true, Ordering::Relaxed);
        }
    });
}

fn fatal_pending(vm_id: u64) -> bool {
    FATAL.with(|f| f.borrow().contains_key(&vm_id))
}

/// The recorded callback exception a JS error stands for, if any: the most
/// recent one whose message is what reached JS (by `name: message`, or a bare
/// message for a plain `Error`).
fn pending_exception_for(ruby: &Ruby, vm_id: u64, e: &JsError) -> Option<Exception> {
    if !HAS_PENDING.with(|s| s.borrow().contains(&vm_id)) {
        return None;
    }
    let list = hash_const(ruby, "PENDING")?.get(vm_id).and_then(RArray::from_value)?;
    let rendered = format!("{}: {}", e.name, e.message);
    for i in (0..list.len()).rev() {
        let pair: RArray = list.entry(i as isize).ok()?;
        let sent: String = pair.entry(0).ok()?;
        if sent == rendered || (e.name == "Error" && sent == e.message) {
            return pair.entry::<Value>(1).ok().and_then(Exception::from_value);
        }
    }
    None
}

/// Create a VM with an empty global context; return its id.
fn create(ruby: &Ruby) -> Result<u64, Error> {
    let mut state = compile_script("").map_err(|e| runtime_error(ruby, e))?;
    state.run_init().map_err(|e| runtime_error(ruby, e))?;
    // Process-global id: unique across threads, so a VM can never be confused
    // with a same-numbered VM on another thread.
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    account_memory(id, state.heap_bytes());
    put_vm(id, state);
    Ok(id)
}

/// Configure resource limits for VM `id`: an instruction budget (`step_limit`),
/// a heap ceiling in bytes (`memory_limit`), and a wall-clock deadline in
/// milliseconds (`timeout_ms`). Any `None` leaves that dimension at its default
/// (instruction budget `u64::MAX` = effectively unbounded).
fn configure_limits(
    ruby: &Ruby,
    id: u64,
    step_limit: Option<u64>,
    memory_limit: Option<u64>,
    timeout_ms: Option<u64>,
) -> Result<(), Error> {
    let mut entry = Entry::take(ruby, id)?;
    let max_steps = step_limit.unwrap_or(u64::MAX);
    // The step/abort/deadline poll lives in the interpreter loop; native code
    // does not iterate it. Turn the JIT off for a bounded VM so a hot loop
    // stays cooperatively interruptible.
    entry.run(ruby, |s| s.disable_vm_jit())?;
    LIMITS.with(|m| {
        m.borrow_mut().insert(
            id,
            LimitState {
                interrupter: Arc::new(Interrupter {
                    abort: Arc::new(AtomicBool::new(false)),
                    interrupted: AtomicBool::new(false),
                }),
                max_steps,
                timeout_ms,
                memory_limit: memory_limit.map(|b| b as usize),
            },
        );
    });
    Ok(())
}

/// Arm the limits for an entry: a fresh recorder (step budget, abort flag),
/// the entry's deadline, and the heap ceiling.
fn arm_limits(state: &mut ScriptState, id: u64) {
    LIMITS.with(|m| {
        if let Some(l) = m.borrow().get(&id) {
            l.interrupter.abort.store(false, Ordering::Relaxed);
            l.interrupter.interrupted.store(false, Ordering::Relaxed);
            // set_limits replaces the recorder, so the deadline and the heap
            // ceiling are re-applied after it.
            state.set_limits(l.max_steps, Some(l.interrupter.abort.clone()));
            state.set_deadline(l.timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms)));
            if let Some(bytes) = l.memory_limit {
                state.set_heap_limit(bytes);
            }
        }
    });
}

/// Evaluate `src` in the VM `id`. The completion value crosses as a structured
/// Ruby value (nil/bool/Integer|Float/String/Array/Hash); an uncaught throw
/// raises the matching `ZippRb::*Error`.
fn eval(ruby: &Ruby, id: u64, src: String) -> Result<Value, Error> {
    // Reentry: a callback for this same VM is on the stack — drive it through
    // the engine's HostCtx rather than the id registry.
    if let Some(ptr) = active_ctx(id) {
        let ctx: &mut dyn HostCtx = unsafe { &mut *ptr };
        let result = ctx.eval_script(&src);
        return nested_result(ruby, id, result);
    }
    with_entry(ruby, id, |entry| {
        // A VM with no Ruby bridge runs without the GVL, so other Ruby threads
        // proceed during a long evaluation.
        match entry.run_nogvl(ruby, |s| s.eval_in_context_structured(&src))? {
            Ok(hv) => Ok(host_to_ruby(ruby, &hv)),
            Err(e) => Err(js_error(ruby, id, &e)),
        }
    })
}

/// Map a nested (reentrant) evaluation's result. A non-local exit recorded
/// further in surfaces as an error here; the outermost entry re-delivers it.
fn nested_result(ruby: &Ruby, id: u64, result: Result<HostValue, String>) -> Result<Value, Error> {
    if fatal_pending(id) {
        return Err(interrupted_error(ruby, "a Ruby interrupt is pending"));
    }
    match result {
        Ok(hv) => Ok(host_to_ruby(ruby, &hv)),
        Err(msg) => Err(js_error_from_message(ruby, id, &msg)),
    }
}

/// Collect garbage now.
fn gc(ruby: &Ruby, id: u64) -> Result<(), Error> {
    let heap = with_entry(ruby, id, |entry| {
        entry.run(ruby, |s| {
            s.gc_now();
            s.heap_bytes()
        })
    })?;
    account_memory(id, heap);
    Ok(())
}

/// The VM's resource usage as a Ruby Hash (heap bytes, instruction steps,
/// retained dynamic code). The closest analogue of quickjs.rb's `memory_usage`.
fn memory_usage(ruby: &Ruby, id: u64) -> Result<RHash, Error> {
    let u = with_entry(ruby, id, |entry| entry.run(ruby, |s| s.resource_usage()))?;
    account_memory(id, u.heap_bytes);
    let hash = ruby.hash_new();
    let set = |name: &str, v: u64| {
        let _ = hash.aset(ruby.to_symbol(name), v);
    };
    set("heap_bytes", u.heap_bytes as u64);
    set("steps_used", u.steps_used);
    set("dynamic_code_calls", u.dynamic_code_calls as u64);
    set("dynamic_code_source_bytes", u.dynamic_code_source_bytes as u64);
    set("retained_functions", u.retained_functions as u64);
    set("retained_classes", u.retained_classes as u64);
    set("retained_function_bytes", u.retained_function_bytes as u64);
    set("retained_class_bytes", u.retained_class_bytes as u64);
    Ok(hash)
}

/// Split a `define_function` name (a String/Symbol, or an Array path) into its
/// segments, each as a String.
fn name_segments(name: Value) -> Vec<String> {
    if let Some(arr) = RArray::from_value(name) {
        arr.into_iter()
            .map(|v| v.funcall::<_, _, String>("to_s", ()).unwrap_or_default())
            .collect()
    } else {
        vec![name.funcall::<_, _, String>("to_s", ()).unwrap_or_default()]
    }
}

/// Evaluate `src` for its side effects, discarding the completion value. Much
/// cheaper than `eval` when a script's last expression is a large object (a
/// UMD factory's return value, say): `eval` marshals that value into Ruby,
/// whereas this only asks the engine for the shallow `ToString`-class result
/// and drops it.
fn load(ruby: &Ruby, id: u64, src: String) -> Result<(), Error> {
    if let Some(ptr) = active_ctx(id) {
        let ctx: &mut dyn HostCtx = unsafe { &mut *ptr };
        let result = ctx.eval_script(&src);
        return nested_result(ruby, id, result).map(|_| ());
    }
    with_entry(ruby, id, |entry| {
        entry
            .run_nogvl(ruby, |s| s.eval_in_context(&src))?
            .map(|_| ())
            .map_err(|msg| js_error_from_message(ruby, id, &msg))
    })
}

/// Call the global function `name` with Ruby `args`, converting both ways.
fn call(ruby: &Ruby, id: u64, name: String, args: RArray) -> Result<Value, Error> {
    let host_args: Vec<HostValue> = args.into_iter().map(|a| ruby_to_host(ruby, a)).collect();
    if let Some(ptr) = active_ctx(id) {
        let ctx: &mut dyn HostCtx = unsafe { &mut *ptr };
        let result = ctx.call_global_value(&name, &host_args);
        return nested_result(ruby, id, result);
    }
    with_entry(ruby, id, |entry| {
        match entry.run_nogvl(ruby, |s| s.call_global_structured(&name, &host_args))? {
            Ok(hv) => Ok(host_to_ruby(ruby, &hv)),
            Err(e) => Err(js_error(ruby, id, &e)),
        }
    })
}

/// Install the Ruby host handler on VM `id` (idempotent) and mark it bridged.
fn install_handler(ruby: &Ruby, entry: &mut Entry) -> Result<(), Error> {
    let id = entry.id;
    mark_bridged(id);
    entry.run(ruby, |s| s.set_host_call_values(rb_handler(id)))
}

/// Register the Ruby `block` as the JS global function `name` on VM `id`.
fn define_function(
    ruby: &Ruby,
    id: u64,
    name: Value,
    block: Value,
    is_async: bool,
) -> Result<Symbol, Error> {
    let segments = name_segments(name);
    let Some(last) = segments.last().cloned() else {
        return Err(runtime_error(ruby, "a function name is required".into()));
    };
    // The kind embeds the VM id so its callbacks can be reclaimed on dispose.
    let kind = format!(
        "rb:{id}:{}",
        NEXT_FN.with(|c| {
            let v = c.get();
            c.set(v.wrapping_add(1));
            v
        })
    );
    // Keep the proc in the Ruby-side registry so the GC roots it.
    let module: RModule = ruby.define_module("ZippRb")?;
    let callbacks: RHash = module.const_get("CALLBACKS")?;
    callbacks.aset(kind.clone(), block)?;

    // A single name installs a global; an Array path installs on a nested
    // object (`["a","b"]` → `a.b`), creating intermediates as needed — the
    // quickjs.rb `define_function ["a","b"]` shape.
    let mut js = String::new();
    let mut base = String::from("globalThis");
    for seg in &segments[..segments.len() - 1] {
        let path = format!("{base}[{}]", js_string(seg));
        js.push_str(&format!("{path} = {path} || {{}};\n"));
        base = path;
    }
    // An `:async` function returns a Promise (resolved with the host result, or
    // rejected with its throw), matching quickjs.rb's `define_function(:async)`.
    let body = if is_async {
        format!(
            "try {{ return Promise.resolve(__zippHostCallValue({k}, ...args)); }} \
             catch (e) {{ return Promise.reject(e); }}",
            k = js_string(&kind),
        )
    } else {
        format!("return __zippHostCallValue({}, ...args);", js_string(&kind))
    };
    js.push_str(&format!(
        "{base}[{}] = function(...args) {{ {body} }};",
        js_string(&last),
    ));
    with_entry(ruby, id, |entry| {
        install_handler(ruby, entry)?;
        entry
            .run(ruby, |s| s.eval_in_context(&js))?
            .map_err(|e| runtime_error(ruby, e))
    })?;
    Ok(ruby.to_symbol(last.as_str()))
}

/// Enable `setTimeout`/`setInterval` on VM `id` (opt-in, like quickjs.rb's
/// `feature_timeout`): install the host handler and the JS shims that register
/// timers with Rust.
fn enable_timers(ruby: &Ruby, id: u64) -> Result<(), Error> {
    with_entry(ruby, id, |entry| {
        install_handler(ruby, entry)?;
        entry
            .run(ruby, |s| s.eval_in_context_structured(TIMER_SHIMS))?
            .map(|_| ())
            .map_err(|e| js_error(ruby, id, &e))
    })
}

/// Handle `__zippHostCallValue("zipp:timer:...", id, delay, repeat)`.
fn handle_timer_op(vm_id: u64, op: &str, args: &[HostValue]) -> Result<HostValue, String> {
    let id = match args.first() {
        Some(HostValue::Number(n)) => *n as u64,
        _ => return Err("TypeError: bad timer id".into()),
    };
    let delay = match args.get(1) {
        Some(HostValue::Number(n)) => *n,
        _ => 0.0,
    };
    let repeat = match args.get(2) {
        Some(HostValue::Number(n)) if *n > 0.0 => Some(*n as u64),
        _ => None,
    };
    match op {
        "set" => TIMERS.with(|m| {
            m.borrow_mut()
                .entry(vm_id)
                .or_default()
                .push(TimerEntry {
                    id,
                    due_ms: now_ms().saturating_add(delay as u64),
                    repeat_ms: repeat,
                })
        }),
        "clear" => TIMERS.with(|m| {
            if let Some(list) = m.borrow_mut().get_mut(&vm_id) {
                list.retain(|t| t.id != id);
            }
        }),
        _ => {}
    }
    Ok(HostValue::Undefined)
}

/// Handle `__zippHostCallValue("zipp:settle", key, fulfilled, value)`, the
/// settlement report of an `eval_async`/`import` promise.
fn handle_settle(args: &[HostValue]) -> Result<HostValue, String> {
    let Some(HostValue::Number(key)) = args.first() else {
        return Err("TypeError: bad settle key".into());
    };
    let fulfilled = matches!(args.get(1), Some(HostValue::Bool(true)));
    let value = args.get(2).cloned().unwrap_or(HostValue::Undefined);
    SETTLED.with(|m| m.borrow_mut().insert(*key as u64, (fulfilled, value)));
    Ok(HostValue::Undefined)
}

/// Fire all due timers once, re-entering JS for each; repeating timers are
/// rescheduled, one-shots removed. Returns whether anything fired.
fn fire_due_timers(ruby: &Ruby, entry: &mut Entry) -> Result<bool, Error> {
    let vm_id = entry.id;
    let now = now_ms();
    let due: Vec<u64> = TIMERS.with(|m| {
        let mut map = m.borrow_mut();
        let Some(list) = map.get_mut(&vm_id) else {
            return Vec::new();
        };
        let mut due = Vec::new();
        let mut i = 0;
        while i < list.len() {
            if list[i].due_ms <= now {
                match list[i].repeat_ms {
                    Some(r) => {
                        list[i].due_ms = now.saturating_add(r.max(1));
                        due.push(list[i].id);
                        i += 1;
                    }
                    None => {
                        due.push(list[i].id);
                        list.remove(i);
                    }
                }
            } else {
                i += 1;
            }
        }
        due
    });
    for tid in &due {
        entry
            .run(ruby, |s| {
                s.call_global_structured("__zipp_fire_timer", &[HostValue::Number(*tid as f64)])
            })?
            .map_err(|e| js_error(ruby, vm_id, &e))?;
    }
    Ok(!due.is_empty())
}

/// The earliest pending timer deadline for a VM, if any.
fn next_timer_due(vm_id: u64) -> Option<u64> {
    TIMERS.with(|m| m.borrow().get(&vm_id).and_then(|l| l.iter().map(|t| t.due_ms).min()))
}

fn next_settle_key() -> u64 {
    NEXT_SETTLE.with(|c| {
        let v = c.get();
        c.set(v.wrapping_add(1));
        v
    })
}

/// JS that reports `promise`'s settlement for `key` through the host channel.
fn settle_js(promise: &str, key: u64) -> String {
    format!(
        "({promise}).then(\n\
           v => __zippHostCallValue(\"zipp:settle\", {key}, true, v),\n\
           e => __zippHostCallValue(\"zipp:settle\", {key}, false, \
                ((e && e.name) ? e.name + ': ' : '') + String((e && e.message) || e)));"
    )
}

/// The settlement reported for `key` as a Ruby value, or the rejection raised.
fn settled_to_ruby(ruby: &Ruby, id: u64, settled: (bool, HostValue)) -> Result<Value, Error> {
    match settled {
        (true, hv) => Ok(host_to_ruby(ruby, &hv)),
        (false, HostValue::String(msg)) => Err(js_error_from_message(ruby, id, &msg)),
        (false, _) => Err(base_error(ruby, "the promise was rejected")),
    }
}

/// Await-capable evaluation (quickjs.rb's `eval_code` shape): run `src` as an
/// async function body, then drive the event loop — draining microtasks and
/// firing timers — until the top-level promise settles (or `max_ms` elapses).
fn eval_async(ruby: &Ruby, id: u64, src: String, max_ms: u64) -> Result<Value, Error> {
    let key = next_settle_key();
    let js = settle_js(&format!("(async () => {{\n{src}\n}})()"), key);
    let settled = with_entry(ruby, id, |entry| {
        install_handler(ruby, entry)?;
        entry
            .run(ruby, |s| s.eval_in_context_structured(&js))?
            .map_err(|e| js_error(ruby, id, &e))?;
        let deadline = now_ms().saturating_add(max_ms);
        loop {
            entry.run(ruby, |s| s.run_microtasks())?;
            if let Some(done) = SETTLED.with(|m| m.borrow_mut().remove(&key)) {
                return Ok(Some(done));
            }
            // Checked every turn: a timer that fires on every pass must not
            // keep the loop past its deadline.
            if now_ms() >= deadline || fatal_pending(id) {
                return Ok(None);
            }
            deliver_interrupts()?;
            if fire_due_timers(ruby, entry)? {
                continue;
            }
            // No timers: the promise can never settle here.
            let Some(due) = next_timer_due(id) else { return Ok(None) };
            let wait = due.min(deadline).saturating_sub(now_ms()).clamp(1, 20);
            sleep_without_gvl(Duration::from_millis(wait))?;
        }
    });
    SETTLED.with(|m| m.borrow_mut().remove(&key));
    match settled? {
        Some(done) => settled_to_ruby(ruby, id, done),
        // An unsettled top-level promise must not read as a normal `undefined`
        // result: report it (a pending promise, or `max_ms` elapsed).
        None => {
            let class = ERROR_CLASSES
                .with(|m| m.borrow().get("NoAwaitError").copied())
                .unwrap_or_else(|| ruby.exception_runtime_error());
            Err(Error::new(
                class,
                format!("eval_async: the awaited expression did not settle within {max_ms} ms"),
            ))
        }
    }
}

/// Evaluate ES module `specifier` (a dynamic `import()`), drive the job queue,
/// and return its namespace exports as a Ruby Hash. Requires a module loader
/// (usually `set_module_provider`).
fn import_module(ruby: &Ruby, id: u64, specifier: String) -> Result<Value, Error> {
    let key = next_settle_key();
    let js = settle_js(
        &format!(
            "import({}).then(ns => {{ const o = {{}}; for (const p of Object.keys(ns)) o[p] = ns[p]; return o; }})",
            js_string(&specifier)
        ),
        key,
    );
    let settled = with_entry(ruby, id, |entry| {
        install_handler(ruby, entry)?;
        entry
            .run(ruby, |s| s.eval_in_context_structured(&js))?
            .map_err(|e| js_error(ruby, id, &e))?;
        entry.run(ruby, |s| s.run_microtasks())?;
        Ok(SETTLED.with(|m| m.borrow_mut().remove(&key)))
    });
    SETTLED.with(|m| m.borrow_mut().remove(&key));
    match settled? {
        Some(done) => settled_to_ruby(ruby, id, done),
        None => Ok(ruby.qnil().as_value()),
    }
}

/// Register a Ruby block as the console listener for VM `id`; it is called
/// with `(severity_symbol, line)` for every `console.*` line.
fn on_log(ruby: &Ruby, id: u64, block: Value) -> Result<(), Error> {
    mark_bridged(id);
    let key = format!("log:{id}");
    store_listener(ruby, &key, block)?;
    with_entry(ruby, id, |entry| {
        entry.run(ruby, |s| {
            s.set_console_sink(Box::new(move |stream, line| {
                let severity = match stream {
                    ConsoleStream::Stdout => "log",
                    ConsoleStream::Stderr => "error",
                };
                let Ok(ruby) = Ruby::get() else { return };
                let args = [
                    ruby.to_symbol(severity).as_value(),
                    ruby.str_new(line).as_value(),
                ];
                call_listener(&ruby, id, &key, &args);
            }))
        })
    })
}

/// Register a Ruby block as the unhandled-rejection listener for VM `id`; it
/// is called with the rejection's message.
fn on_unhandled_rejection(ruby: &Ruby, id: u64, block: Value) -> Result<(), Error> {
    mark_bridged(id);
    let key = format!("rejection:{id}");
    store_listener(ruby, &key, block)?;
    with_entry(ruby, id, |entry| {
        entry.run(ruby, |s| {
            s.set_unhandled_rejection_sink(Box::new(move |message| {
                let Ok(ruby) = Ruby::get() else { return };
                let args = [ruby.str_new(message).as_value()];
                call_listener(&ruby, id, &key, &args);
            }))
        })
    })
}

/// Run queued microtasks (promise reactions, `queueMicrotask`).
fn drain_jobs(ruby: &Ruby, id: u64) -> Result<(), Error> {
    with_entry(ruby, id, |entry| entry.run(ruby, |s| s.run_microtasks()))
}

/// Store a Ruby block in the `ZippRb::LISTENERS` constant so the GC roots it.
fn store_listener(ruby: &Ruby, key: &str, block: Value) -> Result<(), Error> {
    let module: RModule = ruby.define_module("ZippRb")?;
    let listeners: RHash = module.const_get("LISTENERS")?;
    listeners.aset(key, block)?;
    Ok(())
}

/// Call the listener registered under `key`, if any. A listener's
/// `StandardError` is dropped (a listener has no JS caller to report it to);
/// a non-local exit is recorded for re-delivery like a callback's.
fn call_listener(ruby: &Ruby, vm_id: u64, key: &str, args: &[Value]) {
    if fatal_pending(vm_id) {
        return;
    }
    let Some(listeners) = hash_const(ruby, "LISTENERS") else { return };
    let Some(block) = listeners.get(key) else { return };
    if block.is_nil() {
        return;
    }
    if let Err(e) = block.funcall::<_, _, Value>("call", args) {
        if !e.is_kind_of(ruby.exception_standard_error()) {
            capture_ruby_error(ruby, vm_id, e);
        }
    }
}

/// Take a durable handle to the global `name` on VM `id`; returns a binding
/// handle id (or `None` if the name does not resolve).
fn handle(ruby: &Ruby, id: u64, name: String) -> Result<Option<u64>, Error> {
    let made = with_entry(ruby, id, |entry| entry.run(ruby, |s| s.handle_of_global(&name)))?;
    let Some(engine_handle) = made else {
        return Ok(None);
    };
    let hid = NEXT_HANDLE.with(|c| {
        let v = c.get();
        c.set(v.wrapping_add(1));
        v
    });
    HANDLES.with(|h| h.borrow_mut().insert(hid, (id, engine_handle)));
    Ok(Some(hid))
}

/// Queue a garbage-collected `Handle` for release (see [`RELEASE_QUEUE`]).
/// Called from a Ruby finalizer; never raises.
fn queue_release(vm_id: u64, handle_id: u64) {
    if let Ok(mut q) = RELEASE_QUEUE.lock() {
        q.push((vm_id, handle_id));
        RELEASE_QUEUED.store(true, Ordering::Release);
    }
}

/// Release the engine roots of VM `id`'s queued handles (the VM is held by
/// the entry being opened, on its owning thread).
fn release_queued_handles(id: u64, state: &mut ScriptState) {
    if !RELEASE_QUEUED.load(Ordering::Acquire) {
        return;
    }
    let mine: Vec<u64> = match RELEASE_QUEUE.lock() {
        Ok(mut q) => {
            let mut mine = Vec::new();
            q.retain(|&(vm, hid)| {
                if vm == id {
                    mine.push(hid);
                }
                vm != id
            });
            RELEASE_QUEUED.store(!q.is_empty(), Ordering::Release);
            mine
        }
        Err(_) => return,
    };
    for hid in mine {
        let found = HANDLES.with(|h| {
            let mut h = h.borrow_mut();
            match h.get(&hid) {
                Some(&(vm, engine_handle)) if vm == id => {
                    h.remove(&hid);
                    Some(engine_handle)
                }
                _ => None,
            }
        });
        if let Some(engine_handle) = found {
            state.release_handle(engine_handle);
        }
    }
}

/// Look up a binding handle, refusing one that belongs to another VM.
fn lookup_handle(ruby: &Ruby, id: u64, handle_id: u64) -> Result<HostHandle, Error> {
    let Some((vm_id, engine_handle)) = HANDLES.with(|h| h.borrow().get(&handle_id).copied()) else {
        return Err(runtime_error(ruby, format!("unknown handle {handle_id}")));
    };
    if vm_id != id {
        return Err(runtime_error(ruby, "handle belongs to another VM".to_string()));
    }
    Ok(engine_handle)
}

/// Call the function a binding handle roots. From inside a callback for the
/// same VM it runs re-entrantly through the engine's `HostCtx`.
fn call_handle(ruby: &Ruby, id: u64, handle_id: u64, args: RArray) -> Result<Value, Error> {
    let engine_handle = lookup_handle(ruby, id, handle_id)?;
    let host_args: Vec<HostValue> = args.into_iter().map(|a| ruby_to_host(ruby, a)).collect();
    if let Some(ptr) = active_ctx(id) {
        let ctx: &mut dyn HostCtx = unsafe { &mut *ptr };
        let result = ctx.call_handle_value(engine_handle, &host_args);
        return nested_result(ruby, id, result);
    }
    with_entry(ruby, id, |entry| {
        entry
            .run_nogvl(ruby, |s| s.call_handle(engine_handle, &host_args))?
            .map(|hv| host_to_ruby(ruby, &hv))
            .map_err(|e| js_error_from_message(ruby, id, &e))
    })
}

/// Read the value a binding handle roots.
fn get_handle(ruby: &Ruby, id: u64, handle_id: u64) -> Result<Value, Error> {
    let engine_handle = lookup_handle(ruby, id, handle_id)?;
    if let Some(ptr) = active_ctx(id) {
        let ctx: &mut dyn HostCtx = unsafe { &mut *ptr };
        let result = ctx.read_handle_value(engine_handle);
        return nested_result(ruby, id, result);
    }
    with_entry(ruby, id, |entry| {
        entry
            .run(ruby, |s| s.get_handle(engine_handle))?
            .map(|hv| host_to_ruby(ruby, &hv))
            .map_err(|e| js_error_from_message(ruby, id, &e))
    })
}

/// Release a binding handle. Returns whether it was live. The binding record
/// is dropped only once the engine has released its root, so a release that
/// fails leaves the handle intact.
fn release_handle(ruby: &Ruby, id: u64, handle_id: u64) -> Result<bool, Error> {
    let engine_handle = match lookup_handle(ruby, id, handle_id) {
        Ok(h) => h,
        Err(_) if HANDLES.with(|h| !h.borrow().contains_key(&handle_id)) => return Ok(false),
        Err(e) => return Err(e),
    };
    let released = if let Some(ptr) = active_ctx(id) {
        let ctx: &mut dyn HostCtx = unsafe { &mut *ptr };
        ctx.release_handle_value(engine_handle)
    } else {
        with_entry(ruby, id, |entry| entry.run(ruby, |s| s.release_handle(engine_handle)))?
    };
    HANDLES.with(|h| h.borrow_mut().remove(&handle_id));
    Ok(released)
}

/// Install an in-memory module loader on VM `id`: `sources` maps a resolved
/// module path (String) to its source (a String, or a Proc called with the
/// path). No file is read for a module served this way.
fn set_module_provider(
    ruby: &Ruby,
    id: u64,
    root: String,
    sources: Value,
    max_module_bytes: u64,
) -> Result<(), Error> {
    mark_bridged(id);
    // Keep the sources Hash in a Ruby constant so the GC roots any Procs in it.
    let module: RModule = ruby.define_module("ZippRb")?;
    let modules: RHash = module.const_get("MODULES")?;
    modules.aset(id, sources)?;

    let provider = move |path: &str| -> Option<Vec<u8>> {
        let ruby = Ruby::get().ok()?;
        if fatal_pending(id) {
            return None;
        }
        let sources = hash_const(&ruby, "MODULES")?.get(id).and_then(RHash::from_value)?;
        let value: Value = sources.get(path)?;
        if value.is_nil() {
            return None;
        }
        let text: String = if let Some(proc) = Proc::from_value(value) {
            match proc.call((path,)) {
                Ok(text) => text,
                Err(e) => {
                    if !e.is_kind_of(ruby.exception_standard_error()) {
                        capture_ruby_error(&ruby, id, e);
                    }
                    return None;
                }
            }
        } else {
            String::try_convert(value).ok()?
        };
        Some(text.into_bytes())
    };
    with_entry(ruby, id, |entry| {
        entry
            .run(ruby, |s| {
                s.set_module_provider(std::path::Path::new(&root), max_module_bytes, Rc::new(provider))
            })?
            .map_err(|e| runtime_error(ruby, e))
    })
}

/// Dispose the VM `id`. Idempotent; an unknown id is a no-op. Refused with a
/// `ThreadError` while the VM is executing on this thread (its state has been
/// taken out of the registry for the call), so a callback cannot tear down the
/// VM under a running evaluation.
fn dispose(ruby: &Ruby, id: u64) -> Result<(), Error> {
    if IN_FLIGHT.with(|s| s.borrow().contains(&id)) {
        return Err(Error::new(
            ruby.exception_thread_error(),
            "cannot dispose a VM while it is executing",
        ));
    }
    if let Some(mut state) = VMS.with(|vms| vms.borrow_mut().remove(&id)) {
        state.dispose();
        forget_vm(ruby, id);
    }
    Ok(())
}

/// Whether the VM `id` has been disposed (unknown ids report `true`).
fn disposed(id: u64) -> bool {
    VMS.with(|vms| {
        vms.borrow()
            .get(&id)
            .map(|state| state.disposed())
            .unwrap_or(true)
    })
}

/// Whether the VM `id` was discarded after an engine panic.
fn poisoned(id: u64) -> bool {
    POISONED.with(|s| s.borrow().contains(&id))
}

/// Finalizer entry: drop the VM `id` and everything the binding retained for
/// it. Tolerant — it may run on a different thread (the registry is
/// thread-local) and must never raise.
fn drop_vm(ruby: &Ruby, id: u64) {
    if let Some(mut state) = VMS.with(|vms| vms.borrow_mut().remove(&id)) {
        state.dispose();
    }
    forget_vm(ruby, id);
}

/// Put a VM back after a host entry, clearing its in-flight marker.
fn put_vm(id: u64, state: ScriptState) {
    IN_FLIGHT.with(|s| {
        s.borrow_mut().remove(&id);
    });
    VMS.with(|vms| vms.borrow_mut().insert(id, state));
}

/// Drop every Ruby-side retention the binding holds for VM `id`: its callbacks,
/// listeners, in-memory module sources, and durable handles. Without this, the
/// procs (and whatever application objects they close over) would live for the
/// process lifetime even after the VM is disposed or GC'd.
fn forget_vm(ruby: &Ruby, id: u64) {
    if let Some(listeners) = hash_const(ruby, "LISTENERS") {
        for key in [format!("log:{id}"), format!("rejection:{id}")] {
            let _ = listeners.delete::<_, Value>(key);
        }
    }
    if let Some(modules) = hash_const(ruby, "MODULES") {
        let _ = modules.delete::<_, Value>(id);
    }
    if HAS_PENDING.with(|s| s.borrow_mut().remove(&id)) {
        if let Some(pending) = hash_const(ruby, "PENDING") {
            let _ = pending.delete::<_, Value>(id);
            let _ = pending.delete::<_, Value>(format!("fatal:{id}"));
        }
    }
    if let Some(callbacks) = hash_const(ruby, "CALLBACKS") {
        let prefix = format!("rb:{id}:");
        let mut keys: Vec<String> = Vec::new();
        let _ = callbacks.foreach(|k: String, _v: Value| -> Result<ForEach, Error> {
            if k.starts_with(&prefix) {
                keys.push(k);
            }
            Ok(ForEach::Continue)
        });
        for key in keys {
            let _ = callbacks.delete::<_, Value>(key);
        }
    }
    LIMITS.with(|m| {
        m.borrow_mut().remove(&id);
    });
    TIMERS.with(|m| {
        m.borrow_mut().remove(&id);
    });
    BRIDGED.with(|s| {
        s.borrow_mut().remove(&id);
    });
    FATAL.with(|f| {
        f.borrow_mut().remove(&id);
    });
    HANDLES.with(|h| h.borrow_mut().retain(|_, (vm, _)| *vm != id));
    if RELEASE_QUEUED.load(Ordering::Acquire) {
        if let Ok(mut q) = RELEASE_QUEUE.lock() {
            q.retain(|&(vm, _)| vm != id);
            RELEASE_QUEUED.store(!q.is_empty(), Ordering::Release);
        }
    }
    unaccount_memory(id);
}

/// Look up a `ZippRb` constant that is a Hash, or `None`.
fn hash_const(ruby: &Ruby, name: &str) -> Option<RHash> {
    let module: RModule = ruby.define_module("ZippRb").ok()?;
    module.const_get(name).ok()
}

/// The structured host handler backing Ruby callbacks: on
/// `__zippHostCallValue(kind, ...)`, look up the proc registered under `kind`,
/// convert the arguments to Ruby, call it, and convert the result back.
///
/// While the proc runs it publishes `ctx` as the active reentry context for
/// `vm_id`, so a nested `vm.eval`/`vm.call` drives this same VM through the
/// engine's `HostCtx`. `ctx` is moved into a raw pointer (the only handle for
/// its dynamic scope) and restored on return, including on error.
fn rb_handler(vm_id: u64) -> HostCallValues {
    Rc::new(move |ctx: &mut dyn HostCtx, kind: &str, args: &[HostValue]| -> Result<HostValue, String> {
        // Timer registrations and promise settlements are handled in Rust, not
        // dispatched to Ruby.
        if let Some(op) = kind.strip_prefix("zipp:timer:") {
            return handle_timer_op(vm_id, op, args);
        }
        if kind == "zipp:settle" {
            return handle_settle(args);
        }
        let ctx_ptr: *mut (dyn HostCtx + 'static) = unsafe { std::mem::transmute(ctx) };
        let prev = ACTIVE_CTX.with(|c| c.replace(Some((vm_id, ctx_ptr))));
        let result = call_ruby_back(vm_id, kind, args);
        ACTIVE_CTX.with(|c| c.set(prev));
        result
    })
}

fn call_ruby_back(vm_id: u64, kind: &str, args: &[HostValue]) -> Result<HostValue, String> {
    // A non-local exit is waiting to be re-delivered: run no more Ruby.
    if fatal_pending(vm_id) {
        return Err(INTERRUPT_PENDING_MSG.into());
    }
    let ruby = Ruby::get().map_err(|e| e.to_string())?;
    let module: RModule = ruby.define_module("ZippRb").map_err(|e| e.to_string())?;
    let callbacks: RHash = module.const_get("CALLBACKS").map_err(|e| e.to_string())?;
    let proc: Value = callbacks.aref(kind).map_err(|e| e.to_string())?;
    if proc.is_nil() {
        return Err(format!("Error: no Ruby function registered as {kind}"));
    }
    // Build the argument list as an RArray (a stack `VALUE`, so the Ruby GC
    // sees the whole list) rather than a heap `Vec<Value>`: values held only in
    // a Rust heap buffer are not rooted and could be collected while a later
    // argument is allocated.
    let ruby_args = ruby.ary_new_capa(args.len());
    for a in args {
        ruby_args
            .push(host_to_ruby(&ruby, a))
            .map_err(|e| e.to_string())?;
    }
    let callable = Proc::from_value(proc).ok_or_else(|| format!("{kind} is not a Proc"))?;
    match callable.call::<_, Value>(ruby_args) {
        Ok(result) => Ok(ruby_to_host(&ruby, result)),
        Err(e) => Err(capture_ruby_error(&ruby, vm_id, e)),
    }
}

/// The active reentry context for `vm_id`, if a callback for that VM is on the
/// stack.
fn active_ctx(vm_id: u64) -> Option<*mut (dyn HostCtx + 'static)> {
    ACTIVE_CTX
        .with(|c| c.get())
        .and_then(|(id, ptr)| (id == vm_id).then_some(ptr))
}

/// Raise the mapped Ruby exception for a rendered JS error message.
fn js_error_from_message(ruby: &Ruby, vm_id: u64, msg: &str) -> Error {
    js_error(ruby, vm_id, &JsError::from_message(msg))
}

fn runtime_error(ruby: &Ruby, msg: String) -> Error {
    Error::new(ruby.exception_runtime_error(), msg)
}

/// A `ZippRb::Error` (the base class) with `msg`.
fn base_error(ruby: &Ruby, msg: &str) -> Error {
    let class = ERROR_CLASSES
        .with(|m| m.borrow().get("Error").copied())
        .unwrap_or_else(|| ruby.exception_runtime_error());
    Error::new(class, msg.to_string())
}

/// A `ZippRb::InterruptedError` with `msg`.
fn interrupted_error(ruby: &Ruby, msg: &str) -> Error {
    let class = ERROR_CLASSES
        .with(|m| m.borrow().get("InterruptedError").copied())
        .unwrap_or_else(|| ruby.exception_runtime_error());
    Error::new(class, msg.to_string())
}

/// Reinterpret a class we just defined (a subclass of Exception) as an
/// `ExceptionClass`, which is what [`Error::new`] wants.
fn to_exception_class(class: RClass) -> ExceptionClass {
    ExceptionClass::from_value(class.as_value()).expect("defined a subclass of Exception")
}

/// Build the Ruby exception for a structured JS error, by its `name`. Carries
/// the JS error `name` as `#js_name` and the JS `stack` as the Ruby backtrace,
/// as quickjs.rb does. A JS error that stands for a Ruby exception a callback
/// raised (and JS did not catch) re-raises that original exception instead.
fn js_error(ruby: &Ruby, vm_id: u64, e: &JsError) -> Error {
    if let Some(exc) = pending_exception_for(ruby, vm_id, e) {
        return Error::from(exc);
    }
    // A JS error name with no dedicated class falls back to ZippRb::Error (the
    // base), not to a bare RuntimeError, so `#js_name` stays available.
    // A host abort (the deadline passed, or the abort flag was raised) is
    // reported as InterruptedError rather than the engine's RangeError.
    let key = if e.message.contains("aborted by the host") {
        "InterruptedError"
    } else {
        e.name.as_str()
    };
    let class = ERROR_CLASSES
        .with(|m| {
            let m = m.borrow();
            m.get(key).or_else(|| m.get("Error")).copied()
        })
        .unwrap_or_else(|| ruby.exception_runtime_error());
    let Ok(instance) = class.new_instance((e.message.clone(),)) else {
        return Error::new(class, e.message.clone());
    };
    if let Some(obj) = RObject::from_value(instance.as_value()) {
        let _ = obj.ivar_set("@js_name", e.name.clone());
        if let Some(stack) = &e.stack {
            let _ = obj.funcall::<_, _, Value>("set_backtrace", (stack.clone(),));
        }
    }
    Error::from(instance)
}

/// `HostValue` → Ruby value. Structured data (arrays, plain objects) becomes
/// Array/Hash recursively; a JS function/Map/Set/Date/… (`Opaque`) has no Ruby
/// representation here and becomes `nil`.
fn host_to_ruby(ruby: &Ruby, v: &HostValue) -> Value {
    match v {
        // quickjs.rb compatibility: undefined and NaN are sentinel symbols,
        // null is nil, and a function/Map/Set/Date/… (`Opaque`) is nil here.
        HostValue::Undefined => ruby.to_symbol("undefined").as_value(),
        HostValue::Null | HostValue::Opaque => ruby.qnil().as_value(),
        HostValue::Bool(b) => ruby.into_value(*b),
        HostValue::Number(n) if n.is_nan() => ruby.to_symbol("NaN").as_value(),
        HostValue::Number(n) => number_to_ruby(ruby, *n),
        HostValue::String(s) => ruby.str_new(s).as_value(),
        HostValue::Utf16(units) => ruby.into_value(String::from_utf16_lossy(units)),
        HostValue::Array(items) => {
            let ary = ruby.ary_new_capa(items.len());
            for item in items {
                let _ = ary.push(host_to_ruby(ruby, item));
            }
            ary.as_value()
        }
        HostValue::Object(props) => {
            let hash = ruby.hash_new_capa(props.len());
            for (key, value) in props {
                let _ = hash.aset(ruby.str_new(key), host_to_ruby(ruby, value));
            }
            hash.as_value()
        }
        HostValue::Float32Array(items) => {
            let ary = ruby.ary_new_capa(items.len());
            for item in items {
                let _ = ary.push(*item as f64);
            }
            ary.as_value()
        }
    }
}

/// Ruby value → `HostValue`, for call arguments and callback results.
fn ruby_to_host(ruby: &Ruby, v: Value) -> HostValue {
    to_host(ruby, v, &mut Vec::new())
}

/// [`ruby_to_host`]'s walk. Handles nil, bool, Integer/Float, the
/// `:undefined`/`:NaN` sentinels (so a value read from JS round-trips), other
/// Symbols and Strings, Array and Hash (keys by `to_s`); anything else becomes
/// its `to_s`. `path` holds the containers being converted: a container met
/// again on its own path (a cycle) or nested past [`MAX_HOST_DEPTH`] becomes
/// `null`, as the engine treats such structures when converting the other
/// way, instead of recursing until the stack overflows.
fn to_host(ruby: &Ruby, v: Value, path: &mut Vec<rb_sys::VALUE>) -> HostValue {
    if v.is_nil() {
        return HostValue::Null;
    }
    // Strict booleans: Ruby truthiness (magnus's bool TryConvert) would turn
    // any non-nil/false value — a number, a string — into `true`.
    if matches!(v.equal(ruby.qtrue()), Ok(true)) {
        return HostValue::Bool(true);
    }
    if matches!(v.equal(ruby.qfalse()), Ok(true)) {
        return HostValue::Bool(false);
    }
    // Integer and Float by class: `i64::try_convert` would accept a Float (or
    // Rational) and truncate it.
    if let Some(i) = Integer::from_value(v) {
        return HostValue::Number(match i.to_i64() {
            Ok(n) => n as f64,
            Err(_) => f64::try_convert(v).unwrap_or(f64::NAN),
        });
    }
    if let Some(f) = Float::from_value(v) {
        return HostValue::Number(f.to_f64());
    }
    if let Some(sym) = Symbol::from_value(v) {
        let name = sym.name().map(|n| n.into_owned()).unwrap_or_default();
        return match name.as_str() {
            "undefined" => HostValue::Undefined,
            "NaN" => HostValue::Number(f64::NAN),
            _ => HostValue::String(name),
        };
    }
    if RString::from_value(v).is_some() {
        if let Ok(s) = String::try_convert(v) {
            return HostValue::String(s);
        }
    }
    let raw = v.as_raw();
    if let Some(ary) = RArray::from_value(v) {
        if path.len() >= MAX_HOST_DEPTH || path.contains(&raw) {
            return HostValue::Null;
        }
        path.push(raw);
        let items = ary.into_iter().map(|e| to_host(ruby, e, path)).collect();
        path.pop();
        return HostValue::Array(items);
    }
    if let Some(hash) = RHash::from_value(v) {
        if path.len() >= MAX_HOST_DEPTH || path.contains(&raw) {
            return HostValue::Null;
        }
        path.push(raw);
        let mut props = Vec::new();
        let _ = hash.foreach(|k: Value, val: Value| -> Result<ForEach, Error> {
            let key: String = k.funcall("to_s", ()).unwrap_or_default();
            props.push((key, to_host(ruby, val, path)));
            Ok(ForEach::Continue)
        });
        path.pop();
        return HostValue::Object(props);
    }
    HostValue::String(v.funcall::<_, _, String>("to_s", ()).unwrap_or_default())
}

/// A JS number as Ruby Integer when it is an exact integer, else Float. The
/// engine stores every number as `f64`, so this is the closest match to
/// quickjs.rb's Integer/Float split.
fn number_to_ruby(ruby: &Ruby, n: f64) -> Value {
    if n.is_finite() && n.fract() == 0.0 && (i64::MIN as f64..=i64::MAX as f64).contains(&n) {
        ruby.into_value(n as i64)
    } else {
        ruby.into_value(n)
    }
}

/// Quote a Rust string as a JS string literal (double-quoted, minimal escapes).
fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[magnus::init]
fn init(ruby: &Ruby) -> Result<(), Error> {
    let module: RModule = ruby.define_module("ZippRb")?;

    // Exception hierarchy: ZippRb::Error < RuntimeError, with a subclass per
    // JS error name, mirroring quickjs.rb's Quickjs::RuntimeError tree.
    let base_rclass = module.define_class("Error", ruby.exception_runtime_error().as_r_class())?;
    let base = to_exception_class(base_rclass);
    ERROR_CLASSES.with(|m| {
        let mut m = m.borrow_mut();
        m.clear();
        m.insert("Error", base);
        for name in [
            "SyntaxError",
            "TypeError",
            "ReferenceError",
            "RangeError",
            "EvalError",
            "URIError",
            "AggregateError",
            "InterruptedError",
            "NoAwaitError",
        ] {
            if let Ok(class) = module.define_class(name, base_rclass) {
                m.insert(name, to_exception_class(class));
            }
        }
    });

    module.define_singleton_method("_create", function!(create, 0))?;
    module.define_singleton_method("_configure_limits", function!(configure_limits, 4))?;
    module.define_singleton_method("_eval", function!(eval, 2))?;
    module.define_singleton_method("_load", function!(load, 2))?;
    module.define_singleton_method("_call", function!(call, 3))?;
    module.define_singleton_method("_handle", function!(handle, 2))?;
    module.define_singleton_method("_call_handle", function!(call_handle, 3))?;
    module.define_singleton_method("_get_handle", function!(get_handle, 2))?;
    module.define_singleton_method("_release_handle", function!(release_handle, 2))?;
    module.define_singleton_method("_queue_release", function!(queue_release, 2))?;
    module.define_singleton_method("_define_function", function!(define_function, 4))?;
    module.define_singleton_method("_set_module_provider", function!(set_module_provider, 4))?;
    module.define_singleton_method("_import", function!(import_module, 2))?;
    module.define_singleton_method("_enable_timers", function!(enable_timers, 1))?;
    module.define_singleton_method("_eval_async", function!(eval_async, 3))?;
    module.define_singleton_method("_on_log", function!(on_log, 2))?;
    module.define_singleton_method("_on_unhandled_rejection", function!(on_unhandled_rejection, 2))?;
    module.define_singleton_method("_drain_jobs", function!(drain_jobs, 1))?;
    module.define_singleton_method("_dispose", function!(dispose, 1))?;
    module.define_singleton_method("_drop", function!(drop_vm, 1))?;
    module.define_singleton_method("_memory_usage", function!(memory_usage, 1))?;
    module.define_singleton_method("_gc", function!(gc, 1))?;
    module.define_singleton_method("_disposed?", function!(disposed, 1))?;
    module.define_singleton_method("_poisoned?", function!(poisoned, 1))?;
    Ok(())
}
