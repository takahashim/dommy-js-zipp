//! The per-thread VM registry, host entries (`Entry`), and VM lifecycle.

use crate::*;

/// VM ids are process-global and monotonic, so an id minted on one thread can
/// never denote a different VM on another thread. Combined with the
/// thread-local registry (a VM lives only on its owner's thread), a cross-thread
/// use misses the registry and is refused instead of silently hitting an
/// unrelated VM that happens to share the id.
pub(crate) static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Native memory a fresh VM holds beyond its JS heap (compiled builtins,
/// tables), reported to the Ruby GC so that unreferenced VMs are finalized
/// before process memory balloons. Measured at roughly 250 KB of RSS per VM.
pub(crate) const VM_BASE_BYTES: i64 = 256 * 1024;

thread_local! {
    /// Live VMs for this thread, keyed by id.
    pub(crate) static VMS: RefCell<HashMap<u64, ScriptState>> = RefCell::new(HashMap::new());
    /// VMs currently executing on this thread (taken out of `VMS` for the
    /// duration of a host entry). A `dispose` while a VM is in flight is
    /// refused, so the VM cannot be torn down under a running evaluation.
    pub(crate) static IN_FLIGHT: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
    /// VMs discarded after an engine panic.
    pub(crate) static POISONED: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
    /// VMs with a Ruby bridge (callback, listener, module provider, timers):
    /// their entries must keep the GVL, unlike a pure eval.
    pub(crate) static BRIDGED: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
    /// Bytes reported to the Ruby GC per VM (`rb_gc_adjust_memory_usage`).
    pub(crate) static GC_ACCOUNTED: RefCell<HashMap<u64, i64>> = RefCell::new(HashMap::new());
}

/// Mark a VM as bridged (needs the GVL for every entry).
pub(crate) fn mark_bridged(id: u64) {
    BRIDGED.with(|s| {
        s.borrow_mut().insert(id);
    });
}

/// Whether a VM can run a pure evaluation without the GVL.
pub(crate) fn gvl_free_ok(id: u64) -> bool {
    !BRIDGED.with(|s| s.borrow().contains(&id))
}

/// Report a VM's native footprint (`heap_bytes` plus [`VM_BASE_BYTES`]) to the
/// Ruby GC as the delta since the last report, so the GC sees memory that
/// otherwise hides behind a small Ruby object. Called where the heap figure is
/// already at hand — creation, `gc!`, `memory_usage` — never per entry: the
/// engine computes `heap_bytes` with an audit walk of the heap.
pub(crate) fn account_memory(id: u64, heap_bytes: usize) {
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
pub(crate) fn unaccount_memory(id: u64) {
    if let Some(prev) = GC_ACCOUNTED.with(|m| m.borrow_mut().remove(&id)) {
        unsafe { rb_sys::rb_gc_adjust_memory_usage(-prev as _) };
    }
}

/// One host entry into VM `id`: the VM is out of the registry for the entry's
/// lifetime and is put back when the entry drops — on every exit path,
/// including an early `?` return.
pub(crate) struct Entry {
    pub(crate) id: u64,
    pub(crate) state: Option<ScriptState>,
}

impl Entry {
    pub(crate) fn take(ruby: &Ruby, id: u64) -> Result<Entry, Error> {
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

    pub(crate) fn state(&mut self) -> &mut ScriptState {
        self.state.as_mut().expect("an entry holds its VM until it drops")
    }

    /// Run engine work with the GVL held, catching a panic (the VM is then
    /// discarded: its state after an unwind through the engine is unknown).
    pub(crate) fn run<R>(&mut self, ruby: &Ruby, f: impl FnOnce(&mut ScriptState) -> R) -> Result<R, Error> {
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
    pub(crate) fn run_nogvl<R>(&mut self, ruby: &Ruby, mut f: impl FnMut(&mut ScriptState) -> R) -> Result<R, Error> {
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
    pub(crate) fn poison(&mut self, ruby: &Ruby) -> Error {
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
pub(crate) fn with_entry<T>(
    ruby: &Ruby,
    id: u64,
    f: impl FnOnce(&mut Entry) -> Result<T, Error>,
) -> Result<T, Error> {
    let mut entry = Entry::take(ruby, id)?;
    let result = f(&mut entry);
    drop(entry);
    deliver_rejections(ruby, id);
    finish(ruby, id, result)
}

/// Create a VM with an empty global context; return its id.
pub(crate) fn create(ruby: &Ruby) -> Result<u64, Error> {
    let mut state = compile_script("").map_err(|e| runtime_error(ruby, e))?;
    state.run_init().map_err(|e| runtime_error(ruby, e))?;
    // Process-global id: unique across threads, so a VM can never be confused
    // with a same-numbered VM on another thread.
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    account_memory(id, state.heap_bytes());
    put_vm(id, state);
    Ok(id)
}

/// Dispose the VM `id`. Idempotent; an unknown id is a no-op. Refused with a
/// `ThreadError` while the VM is executing on this thread (its state has been
/// taken out of the registry for the call), so a callback cannot tear down the
/// VM under a running evaluation.
pub(crate) fn dispose(ruby: &Ruby, id: u64) -> Result<(), Error> {
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

/// Whether the VM `id` has been disposed (unknown ids report `true`). A VM
/// that is executing is out of the registry (its `Entry` holds it) but alive.
pub(crate) fn disposed(id: u64) -> bool {
    if IN_FLIGHT.with(|s| s.borrow().contains(&id)) {
        return false;
    }
    VMS.with(|vms| {
        vms.borrow()
            .get(&id)
            .map(|state| state.disposed())
            .unwrap_or(true)
    })
}

/// Whether the VM `id` was discarded after an engine panic.
pub(crate) fn poisoned(id: u64) -> bool {
    POISONED.with(|s| s.borrow().contains(&id))
}

/// Finalizer entry: drop the VM `id` and everything the binding retained for
/// it. Tolerant — it may run on a different thread (the registry is
/// thread-local) and must never raise.
pub(crate) fn drop_vm(ruby: &Ruby, id: u64) {
    if let Some(mut state) = VMS.with(|vms| vms.borrow_mut().remove(&id)) {
        state.dispose();
    }
    forget_vm(ruby, id);
}

/// Put a VM back after a host entry, clearing its in-flight marker.
pub(crate) fn put_vm(id: u64, state: ScriptState) {
    IN_FLIGHT.with(|s| {
        s.borrow_mut().remove(&id);
    });
    VMS.with(|vms| vms.borrow_mut().insert(id, state));
}

/// Drop every Ruby-side retention the binding holds for VM `id`: its callbacks,
/// listeners, in-memory module sources, and durable handles. Without this, the
/// procs (and whatever application objects they close over) would live for the
/// process lifetime even after the VM is disposed or GC'd.
pub(crate) fn forget_vm(ruby: &Ruby, id: u64) {
    forget_rejections(id);
    if let Some(listeners) = hash_const(ruby, "LISTENERS") {
        for key in [
            format!("log:{id}"),
            format!("rejection:{id}"),
            format!("module_resolver:{id}"),
            format!("module_meta_url:{id}"),
        ] {
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
    HOST_FN_HELPER.with(|s| {
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
pub(crate) fn hash_const(ruby: &Ruby, name: &str) -> Option<RHash> {
    let module: RModule = ruby.define_module("ZippRb").ok()?;
    module.const_get(name).ok()
}
