//! Durable handles to JS values held by Ruby.

use crate::*;

/// Handles whose Ruby `Handle` was garbage-collected without `release`, as
/// `(vm id, handle id)`. A finalizer may run on any thread and at any moment
/// (even mid-entry), so it only queues; the owning thread releases the engine
/// root on its next entry into that VM. `RELEASE_QUEUED` keeps the common
/// empty case to one atomic load.
pub(crate) static RELEASE_QUEUE: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
pub(crate) static RELEASE_QUEUED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Durable JS value handles the binding has handed to Ruby, keyed by a
    /// binding id. Value is `(owning VM id, engine handle)`, so a VM's handles
    /// can be dropped when it is disposed.
    pub(crate) static HANDLES: RefCell<HashMap<u64, (u64, HostHandle)>> = RefCell::new(HashMap::new());
    /// Next handle id to hand out.
    pub(crate) static NEXT_HANDLE: Cell<u64> = const { Cell::new(1) };
}

/// Take a durable handle to the global `name` on VM `id`; returns a binding
/// handle id (or `None` if the name does not resolve).
pub(crate) fn handle(ruby: &Ruby, id: u64, name: String) -> Result<Option<u64>, Error> {
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
pub(crate) fn queue_release(vm_id: u64, handle_id: u64) {
    if let Ok(mut q) = RELEASE_QUEUE.lock() {
        q.push((vm_id, handle_id));
        RELEASE_QUEUED.store(true, Ordering::Release);
    }
}

/// Release the engine roots of VM `id`'s queued handles (the VM is held by
/// the entry being opened, on its owning thread).
pub(crate) fn release_queued_handles(id: u64, state: &mut ScriptState) {
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
pub(crate) fn lookup_handle(ruby: &Ruby, id: u64, handle_id: u64) -> Result<HostHandle, Error> {
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
pub(crate) fn call_handle(ruby: &Ruby, id: u64, handle_id: u64, args: RArray) -> Result<Value, Error> {
    let engine_handle = lookup_handle(ruby, id, handle_id)?;
    let host_args: Vec<HostValue> = args.into_iter().map(|a| ruby_to_host(ruby, a)).collect();
    if let Some(result) = with_active_ctx(id, |ctx| ctx.call_handle_value(engine_handle, &host_args)) {
        return nested_result(ruby, id, result);
    }
    with_entry(ruby, id, |entry| {
        // SAFETY: the closure only drives the engine.
        unsafe { entry.run_nogvl(ruby, |s| s.call_handle(engine_handle, &host_args)) }?
            .map(|hv| host_to_ruby(ruby, &hv))
            .map_err(|e| js_error_from_message(ruby, id, &e))
    })
}

/// Read the value a binding handle roots.
pub(crate) fn get_handle(ruby: &Ruby, id: u64, handle_id: u64) -> Result<Value, Error> {
    let engine_handle = lookup_handle(ruby, id, handle_id)?;
    if let Some(result) = with_active_ctx(id, |ctx| ctx.read_handle_value(engine_handle)) {
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
pub(crate) fn release_handle(ruby: &Ruby, id: u64, handle_id: u64) -> Result<bool, Error> {
    let engine_handle = match lookup_handle(ruby, id, handle_id) {
        Ok(h) => h,
        Err(_) if HANDLES.with(|h| !h.borrow().contains_key(&handle_id)) => return Ok(false),
        Err(e) => return Err(e),
    };
    let released = if let Some(released) = with_active_ctx(id, |ctx| ctx.release_handle_value(engine_handle)) {
        released
    } else {
        with_entry(ruby, id, |entry| entry.run(ruby, |s| s.release_handle(engine_handle)))?
    };
    HANDLES.with(|h| h.borrow_mut().remove(&handle_id));
    Ok(released)
}
