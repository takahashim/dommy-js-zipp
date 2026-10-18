//! Resource limits: step budget, heap ceiling, run deadline, interruption.

use crate::*;

/// Per-VM resource limits, applied around every host entry. The wall-clock
/// timeout is an engine deadline (checked where the engine polls its abort
/// flag), so no timer thread is involved; the abort flag itself is raised by a
/// Ruby interrupt (via [`Interrupter`]) or a callback's non-local exit.
pub(crate) struct LimitState {
    pub(crate) interrupter: Arc<Interrupter>,
    pub(crate) max_steps: u64,
    pub(crate) timeout_ms: Option<u64>,
    pub(crate) memory_limit: Option<usize>,
}

thread_local! {
    /// Resource limits per VM (instruction budget, heap ceiling, wall timeout).
    pub(crate) static LIMITS: RefCell<HashMap<u64, LimitState>> = RefCell::new(HashMap::new());
}

/// Configure resource limits for VM `id`: an instruction budget (`step_limit`),
/// a heap ceiling in bytes (`memory_limit`), and a wall-clock deadline in
/// milliseconds (`timeout_ms`). Any `None` leaves that dimension at its default
/// (instruction budget `u64::MAX` = effectively unbounded).
pub(crate) fn configure_limits(
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

/// Arm the limits for an entry: a renewed step budget, the entry's deadline,
/// and, the first time and after a limit was hit, a fresh recorder with the
/// heap ceiling.
///
/// The recorder is kept across entries while no limit has been hit: renewing
/// the step budget is all an entry needs. Re-applying the heap ceiling every
/// entry would cost a major collection per entry once the live heap passes
/// half the ceiling (`set_heap_limit` collects then, so a VM that filled its
/// heap can allocate again): 11 ms a call at 265 MB under 512 MB. A limit
/// that was hit is sticky in the recorder (`renew_step_budget` refuses), so
/// the next entry gets a fresh one, and that collection is the recovery the
/// VM needs to run again.
pub(crate) fn arm_limits(state: &mut ScriptState, id: u64) {
    LIMITS.with(|m| {
        if let Some(l) = m.borrow().get(&id) {
            l.interrupter.abort.store(false, Ordering::Relaxed);
            l.interrupter.interrupted.store(false, Ordering::Relaxed);
            if !state.renew_step_budget(l.max_steps) {
                // No recorder yet, or a limit was hit: set_limits installs a
                // fresh one, so the heap ceiling is re-applied after it.
                state.set_limits(l.max_steps, Some(l.interrupter.abort.clone()));
                if let Some(bytes) = l.memory_limit {
                    state.set_heap_limit(bytes);
                }
            }
            state.set_deadline(l.timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms)));
        }
    });
}
