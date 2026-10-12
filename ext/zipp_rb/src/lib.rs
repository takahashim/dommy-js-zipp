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

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use magnus::{
    block::Proc, function, prelude::*, r_hash::ForEach, Class, Error, ExceptionClass, RArray,
    RClass, RHash, RModule, RObject, Ruby, Symbol, Value,
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

/// Next timer id (binding-side).
static NEXT_TIMER: AtomicU64 = AtomicU64::new(1);

/// A scheduled timer for a VM.
struct TimerEntry {
    id: u64,
    due_ms: u64,
    repeat_ms: Option<u64>,
}

/// The JS `setTimeout`/`setInterval` shims installed by `enable_timers`. Their
/// registrations cross to Rust via `__zippHostCallValue("zipp:timer:...")`;
/// Rust fires them by re-entering `__zipp_fire_timer`.
const TIMER_SHIMS: &str = r#"
globalThis.__zipp_timer_cbs = {};
globalThis.__zipp_timer_seq = 1;
function setTimeout(fn, delay, ...args) {
  const id = globalThis.__zipp_timer_seq++;
  globalThis.__zipp_timer_cbs[id] = () => fn(...args);
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

/// Run `f` with the GVL released, so other Ruby threads can run while a pure
/// engine evaluation proceeds. The caller MUST guarantee `f` touches no Ruby
/// API (it only drives the engine); a VM with any Ruby bridge installed is not
/// run through this path.
///
/// `rb_thread_call_without_gvl` runs `f` on the same OS thread with the GVL
/// dropped; the `ubf` is `None`, so the region is not interruptible by
/// `Thread#raise`/`Timeout` (the engine's own timeout watchdog still applies).
unsafe fn without_gvl<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    unsafe extern "C" fn trampoline<F, R>(data: *mut std::os::raw::c_void) -> *mut std::os::raw::c_void
    where
        F: FnOnce() -> R,
    {
        let f = unsafe { Box::from_raw(data as *mut F) };
        let result = f();
        Box::into_raw(Box::new(result)) as *mut std::os::raw::c_void
    }
    let data = Box::into_raw(Box::new(f)) as *mut std::os::raw::c_void;
    let out = unsafe {
        rb_sys::rb_thread_call_without_gvl(
            Some(trampoline::<F, R>),
            data,
            None,
            std::ptr::null_mut(),
        )
    };
    unsafe { *Box::from_raw(out as *mut R) }
}

/// Wall-clock milliseconds, for timeout deadlines shared with a watchdog thread.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Per-VM resource limits, applied around every host entry. `abort` and
/// `deadline` are shared with a watchdog thread that sets `abort` when the
/// deadline passes.
struct LimitState {
    abort: Arc<AtomicBool>,
    deadline: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    max_steps: u64,
    timeout_ms: Option<u64>,
    memory_limit: Option<usize>,
}

thread_local! {
    /// Live VMs for this thread, keyed by id.
    static VMS: RefCell<HashMap<u64, ScriptState>> = RefCell::new(HashMap::new());
    /// VMs currently executing on this thread (taken out of `VMS` for the
    /// duration of a host entry). A `dispose` while a VM is in flight is
    /// refused, so the VM cannot be torn down under a running evaluation.
    static IN_FLIGHT: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
    /// Next callback id.
    static NEXT_FN: Cell<u64> = const { Cell::new(1) };
    /// Next module-provider id.
    static NEXT_PROVIDER: Cell<u64> = const { Cell::new(1) };
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
    /// Next import scratch-key id.
    static NEXT_IMPORT: Cell<u64> = const { Cell::new(1) };
    /// Resource limits per VM (instruction budget, heap ceiling, wall timeout).
    static LIMITS: RefCell<HashMap<u64, LimitState>> = RefCell::new(HashMap::new());
    /// Scheduled timers per VM (opt-in `feature_timeout`).
    static TIMERS: RefCell<HashMap<u64, Vec<TimerEntry>>> = RefCell::new(HashMap::new());
    /// VMs with a Ruby bridge (callback, listener, module provider, timers):
    /// their entries must keep the GVL, unlike a pure eval.
    static BRIDGED: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
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

/// Create a VM with an empty global context; return its id.
fn create(ruby: &Ruby) -> Result<u64, Error> {
    let mut state = compile_script("").map_err(|e| runtime_error(ruby, e))?;
    state.run_init().map_err(|e| runtime_error(ruby, e))?;
    // Process-global id: unique across threads, so a VM can never be confused
    // with a same-numbered VM on another thread.
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
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
    let mut state = take_vm(ruby, id)?;
    let abort = Arc::new(AtomicBool::new(false));
    let deadline = Arc::new(AtomicU64::new(0));
    let running = Arc::new(AtomicBool::new(true));
    let max_steps = step_limit.unwrap_or(u64::MAX);
    // The step/abort hook lives in the interpreter loop; native code does not
    // iterate it. Turn the JIT off for a bounded VM so a hot loop stays
    // cooperatively interruptible.
    state.disable_vm_jit();
    if timeout_ms.is_some() {
        let (d, a, r) = (deadline.clone(), abort.clone(), running.clone());
        std::thread::spawn(move || {
            while r.load(Ordering::Relaxed) {
                let dl = d.load(Ordering::Relaxed);
                if dl != 0 && now_ms() >= dl {
                    a.store(true, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });
    }
    LIMITS.with(|m| {
        m.borrow_mut().insert(
            id,
            LimitState {
                abort,
                deadline,
                running,
                max_steps,
                timeout_ms,
                memory_limit: memory_limit.map(|b| b as usize),
            },
        );
    });
    put_vm(id, state);
    Ok(())
}

/// Arm the limits for an entry: reset the abort flag and set the deadline.
fn arm_limits(state: &mut ScriptState, id: u64) {
    LIMITS.with(|m| {
        if let Some(l) = m.borrow().get(&id) {
            l.abort.store(false, Ordering::Relaxed);
            l.deadline
                .store(now_ms().saturating_add(l.timeout_ms.unwrap_or(0)), Ordering::Relaxed);
            // A fresh recorder per entry (set_limits replaces it), so the heap
            // ceiling must be re-applied here too, after set_limits.
            state.set_limits(l.max_steps, l.timeout_ms.map(|_| l.abort.clone()));
            if let Some(bytes) = l.memory_limit {
                state.set_heap_limit(bytes);
            }
        }
    });
}

/// Disarm the wall-clock deadline after an entry (leaves the abort flag alone).
fn disarm_limits(id: u64) {
    LIMITS.with(|m| {
        if let Some(l) = m.borrow().get(&id) {
            l.deadline.store(0, Ordering::Relaxed);
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
        return match ctx.eval_script(&src) {
            Ok(hv) => Ok(host_to_ruby(ruby, &hv)),
            Err(msg) => Err(js_error_from_message(ruby, &msg)),
        };
    }
    // Take the state out for the call so a nested call cannot double-borrow the
    // registry (the reentry path above is what makes a nested call work).
    let mut state = take_vm(ruby, id)?;
    // A VM with no Ruby bridge runs without the GVL, so other Ruby threads
    // proceed during a long evaluation.
    let result = if gvl_free_ok(id) {
        unsafe { without_gvl(|| state.eval_in_context_structured(&src)) }
    } else {
        state.eval_in_context_structured(&src)
    };
    put_vm(id, state);
    match result {
        Ok(hv) => Ok(host_to_ruby(ruby, &hv)),
        Err(e) => Err(js_error(ruby, &e)),
    }
}

/// Call the global function `name` with Ruby `args`, converting both ways.
fn call(ruby: &Ruby, id: u64, name: String, args: RArray) -> Result<Value, Error> {
    let host_args: Vec<HostValue> = args.into_iter().map(|a| ruby_to_host(ruby, a)).collect();
    if let Some(ptr) = active_ctx(id) {
        let ctx: &mut dyn HostCtx = unsafe { &mut *ptr };
        return match ctx.call_global_value(&name, &host_args) {
            Ok(hv) => Ok(host_to_ruby(ruby, &hv)),
            Err(msg) => Err(js_error_from_message(ruby, &msg)),
        };
    }
    let mut state = take_vm(ruby, id)?;
    let result = if gvl_free_ok(id) {
        unsafe { without_gvl(|| state.call_global_structured(&name, &host_args)) }
    } else {
        state.call_global_structured(&name, &host_args)
    };
    put_vm(id, state);
    match result {
        Ok(hv) => Ok(host_to_ruby(ruby, &hv)),
        Err(e) => Err(js_error(ruby, &e)),
    }
}

/// Register the Ruby `block` as the JS global function `name` on VM `id`.
fn define_function(
    ruby: &Ruby,
    id: u64,
    name: String,
    block: Value,
) -> Result<Symbol, Error> {
    // The kind embeds the VM id so its callbacks can be reclaimed on dispose.
    let kind = format!(
        "rb:{id}:{}",
        NEXT_FN.with(|c| {
            let v = c.get();
            c.set(v.wrapping_add(1));
            v
        })
    );
    mark_bridged(id);
    // Keep the proc in the Ruby-side registry so the GC roots it.
    let module: RModule = ruby.define_module("ZippRb")?;
    let callbacks: RHash = module.const_get("CALLBACKS")?;
    callbacks.aset(kind.clone(), block)?;

    let mut state = take_vm(ruby, id)?;
    state.set_host_call_values(rb_handler(id));
    let js = format!(
        "globalThis[{name}] = function(...args) {{ return __zippHostCallValue({kind}, ...args); }};",
        name = js_string(&name),
        kind = js_string(&kind),
    );
    let result = state.eval_in_context(&js).map_err(|e| runtime_error(ruby, e));
    put_vm(id, state);
    result.map(|_| ruby.to_symbol(name.as_str()))
}

/// Enable `setTimeout`/`setInterval` on VM `id` (opt-in, like quickjs.rb's
/// `feature_timeout`): install the host handler and the JS shims that register
/// timers with Rust.
fn enable_timers(ruby: &Ruby, id: u64) -> Result<(), Error> {
    mark_bridged(id);
    let mut state = take_vm(ruby, id)?;
    state.set_host_call_values(rb_handler(id));
    let result = state
        .eval_in_context_structured(TIMER_SHIMS)
        .map_err(|e| js_error(ruby, &e));
    put_vm(id, state);
    result.map(|_| ())
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

/// Fire all due timers once, re-entering JS for each; repeating timers are
/// rescheduled, one-shots removed. Returns whether anything fired.
fn fire_due_timers(state: &mut ScriptState, vm_id: u64) -> Result<bool, JsError> {
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
        state
            .call_global_structured("__zipp_fire_timer", &[HostValue::Number(*tid as f64)])?;
    }
    Ok(!due.is_empty())
}

/// The earliest pending timer deadline for a VM, if any.
fn next_timer_due(vm_id: u64) -> Option<u64> {
    TIMERS.with(|m| m.borrow().get(&vm_id).and_then(|l| l.iter().map(|t| t.due_ms).min()))
}

/// Await-capable evaluation (quickjs.rb's `eval_code` shape): run `src` as an
/// async function body, then drive the event loop — draining microtasks and
/// firing timers — until the top-level promise settles (or `max_ms` elapses).
fn eval_async(ruby: &Ruby, id: u64, src: String, max_ms: u64) -> Result<Value, Error> {
    let key = format!(
        "__zipp_async_{}",
        NEXT_IMPORT.with(|c| {
            let v = c.get();
            c.set(v.wrapping_add(1));
            v
        })
    );
    let k = js_string(&key);
    // Store the settled result / a rendered error string (an Error object would
    // cross as Opaque).
    let js = format!(
        "globalThis[{k}] = null;\n\
         (async () => {{\n\
           try {{ const v = await (async () => {{\n{src}\n}})(); globalThis[{k}] = {{ done: true, value: v }}; }}\n\
           catch (e) {{ globalThis[{k}] = {{ done: true, error: ((e && e.name) ? e.name + ': ' : '') + String((e && e.message) || e) }}; }}\n\
         }})();"
    );
    let mut state = take_vm(ruby, id)?;
    let run = (|| -> Result<Value, Error> {
        state
            .eval_in_context_structured(&js)
            .map_err(|e| js_error(ruby, &e))?;
        let deadline = now_ms().saturating_add(max_ms);
        loop {
            state.run_microtasks();
            match state.eval_in_context_structured(&format!(
                "!!(globalThis[{k}] && globalThis[{k}].done)"
            )) {
                Ok(HostValue::Bool(true)) => break,
                Ok(_) => {}
                Err(e) => return Err(js_error(ruby, &e)),
            }
            if fire_due_timers(&mut state, id).map_err(|e| js_error(ruby, &e))? {
                continue;
            }
            match next_timer_due(id) {
                Some(due) if now_ms() < deadline => {
                    let wait = due.saturating_sub(now_ms()).clamp(1, 20);
                    std::thread::sleep(Duration::from_millis(wait));
                }
                // No timers: the promise can never settle here.
                None => break,
                // Deadline passed.
                Some(_) => break,
            }
            if now_ms() >= deadline {
                break;
            }
        }
        let err = state
            .eval_in_context_structured(&format!(
                "(globalThis[{k}] && globalThis[{k}].error) || ''"
            ))
            .map_err(|e| js_error(ruby, &e))?;
        if let HostValue::String(message) = &err {
            if !message.is_empty() {
                return Err(js_error_from_message(ruby, message));
            }
        }
        match state.eval_in_context_structured(&format!(
            "globalThis[{k}] ? globalThis[{k}].value : undefined"
        )) {
            Ok(hv) => Ok(host_to_ruby(ruby, &hv)),
            Err(e) => Err(js_error(ruby, &e)),
        }
    })();
    put_vm(id, state);
    run
}

/// Evaluate ES module `specifier` (a dynamic `import()`), drive the job queue,
/// and return its namespace exports as a Ruby Hash. Requires a module loader
/// (usually `set_module_provider`).
fn import_module(ruby: &Ruby, id: u64, specifier: String) -> Result<Value, Error> {
    let key = format!(
        "__zipp_import_{}",
        NEXT_IMPORT.with(|c| {
            let v = c.get();
            c.set(v.wrapping_add(1));
            v
        })
    );
    let k = js_string(&key);
    let js = format!(
        "globalThis[{k} + '_r'] = null; globalThis[{k} + '_e'] = null;\n\
         import({spec}).then(\n\
           ns => {{ const o = {{}}; for (const p of Object.keys(ns)) o[p] = ns[p]; globalThis[{k} + '_r'] = o; }},\n\
           e => {{ const n = (e && e.name) ? e.name + ': ' : ''; globalThis[{k} + '_e'] = n + String((e && e.message) || e); }});",
        k = k,
        spec = js_string(&specifier),
    );
    let mut state = take_vm(ruby, id)?;
    let run = (|| -> Result<Value, Error> {
        state
            .eval_in_context_structured(&js)
            .map_err(|e| js_error(ruby, &e))?;
        state.run_microtasks();
        let err = state.eval_in_context_structured(&format!("globalThis[{k} + '_e']"));
        if let Ok(HostValue::String(message)) = err {
            if !message.is_empty() {
                return Err(js_error_from_message(ruby, &message));
            }
        }
        match state.eval_in_context_structured(&format!("globalThis[{k} + '_r']")) {
            Ok(hv) => Ok(host_to_ruby(ruby, &hv)),
            Err(e) => Err(js_error(ruby, &e)),
        }
    })();
    put_vm(id, state);
    run
}

/// Register a Ruby block as the console listener for VM `id`; it is called
/// with `(severity_symbol, line)` for every `console.*` line.
fn on_log(ruby: &Ruby, id: u64, block: Value) -> Result<(), Error> {
    mark_bridged(id);
    let key = format!("log:{id}");
    store_listener(ruby, &key, block)?;
    let mut state = take_vm(ruby, id)?;
    let k = key.clone();
    state.set_console_sink(Box::new(move |stream, line| {
        let severity = match stream {
            ConsoleStream::Stdout => "log",
            ConsoleStream::Stderr => "error",
        };
        let ruby = match Ruby::get() {
            Ok(ruby) => ruby,
            Err(_) => return,
        };
        let args = [
            ruby.to_symbol(severity).as_value(),
            ruby.into_value(line.to_string()),
        ];
        let _ = call_listener(&k, &args);
    }));
    put_vm(id, state);
    Ok(())
}

/// Register a Ruby block as the unhandled-rejection listener for VM `id`; it
/// is called with the rejection's message.
fn on_unhandled_rejection(ruby: &Ruby, id: u64, block: Value) -> Result<(), Error> {
    mark_bridged(id);
    let key = format!("rejection:{id}");
    store_listener(ruby, &key, block)?;
    let mut state = take_vm(ruby, id)?;
    let k = key.clone();
    state.set_unhandled_rejection_sink(Box::new(move |message| {
        let ruby = match Ruby::get() {
            Ok(ruby) => ruby,
            Err(_) => return,
        };
        let args = [ruby.into_value(message.to_string())];
        let _ = call_listener(&k, &args);
    }));
    put_vm(id, state);
    Ok(())
}

/// Run queued microtasks (promise reactions, `queueMicrotask`).
fn drain_jobs(ruby: &Ruby, id: u64) -> Result<(), Error> {
    let mut state = take_vm(ruby, id)?;
    state.run_microtasks();
    put_vm(id, state);
    Ok(())
}

/// Store a Ruby block in the `ZippRb::LISTENERS` constant so the GC roots it.
fn store_listener(ruby: &Ruby, key: &str, block: Value) -> Result<(), Error> {
    let module: RModule = ruby.define_module("ZippRb")?;
    let listeners: RHash = module.const_get("LISTENERS")?;
    listeners.aset(key, block)?;
    Ok(())
}

/// Call the listener registered under `key`, if any.
fn call_listener(key: &str, args: &[Value]) -> Option<Value> {
    let ruby = Ruby::get().ok()?;
    let module: RModule = ruby.define_module("ZippRb").ok()?;
    let listeners: RHash = module.const_get("LISTENERS").ok()?;
    let block: Value = listeners.aref(key).ok()?;
    if block.is_nil() {
        return None;
    }
    block.funcall("call", args).ok()
}

/// Take a durable handle to the global `name` on VM `id`; returns a binding
/// handle id (or `None` if the name does not resolve).
fn handle(ruby: &Ruby, id: u64, name: String) -> Result<Option<u64>, Error> {
    let mut state = take_vm(ruby, id)?;
    let made = state.handle_of_global(&name);
    put_vm(id, state);
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

/// Call the function a binding handle roots.
fn call_handle(ruby: &Ruby, id: u64, handle_id: u64, args: RArray) -> Result<Value, Error> {
    let Some((vm_id, engine_handle)) = HANDLES.with(|h| h.borrow().get(&handle_id).copied()) else {
        return Err(runtime_error(ruby, format!("unknown handle {handle_id}")));
    };
    if vm_id != id {
        return Err(runtime_error(ruby, "handle belongs to another VM".to_string()));
    }
    let host_args: Vec<HostValue> = args.into_iter().map(|a| ruby_to_host(ruby, a)).collect();
    let mut state = take_vm(ruby, id)?;
    let result = state.call_handle(engine_handle, &host_args);
    put_vm(id, state);
    result
        .map(|hv| host_to_ruby(ruby, &hv))
        .map_err(|e| js_error_from_message(ruby, &e))
}

/// Read the value a binding handle roots.
fn get_handle(ruby: &Ruby, id: u64, handle_id: u64) -> Result<Value, Error> {
    let Some((vm_id, engine_handle)) = HANDLES.with(|h| h.borrow().get(&handle_id).copied()) else {
        return Err(runtime_error(ruby, format!("unknown handle {handle_id}")));
    };
    if vm_id != id {
        return Err(runtime_error(ruby, "handle belongs to another VM".to_string()));
    }
    let mut state = take_vm(ruby, id)?;
    let result = state.get_handle(engine_handle);
    put_vm(id, state);
    result
        .map(|hv| host_to_ruby(ruby, &hv))
        .map_err(|e| js_error_from_message(ruby, &e))
}

/// Release a binding handle. Returns whether it was live.
fn release_handle(ruby: &Ruby, id: u64, handle_id: u64) -> Result<bool, Error> {
    let Some((vm_id, engine_handle)) = HANDLES.with(|h| h.borrow_mut().remove(&handle_id)) else {
        return Ok(false);
    };
    if vm_id != id {
        HANDLES.with(|h| h.borrow_mut().insert(handle_id, (vm_id, engine_handle)));
        return Err(runtime_error(ruby, "handle belongs to another VM".to_string()));
    }
    let mut state = take_vm(ruby, id)?;
    let released = state.release_handle(engine_handle);
    put_vm(id, state);
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
    // One provider per VM, keyed by the VM id, so it is reclaimed on dispose.
    let _ = NEXT_PROVIDER.with(|c| {
        let v = c.get();
        c.set(v.wrapping_add(1));
        v
    });
    // Keep the sources Hash in a Ruby constant so the GC roots any Procs in it.
    let module: RModule = ruby.define_module("ZippRb")?;
    let modules: RHash = module.const_get("MODULES")?;
    modules.aset(id, sources)?;

    let mut state = take_vm(ruby, id)?;
    let provider = move |path: &str| -> Option<Vec<u8>> {
        let ruby = Ruby::get().ok()?;
        let module = ruby.define_module("ZippRb").ok()?;
        let modules: RHash = module.const_get("MODULES").ok()?;
        let sources: Value = modules.aref(id).ok()?;
        let sources = RHash::from_value(sources)?;
        let value: Value = sources.aref(path).ok()?;
        if value.is_nil() {
            return None;
        }
        let text: String = if let Some(proc) = Proc::from_value(value) {
            proc.call((path,)).ok()?
        } else {
            String::try_convert(value).ok()?
        };
        Some(text.into_bytes())
    };
    let result = state
        .set_module_provider(std::path::Path::new(&root), max_module_bytes, Rc::new(provider))
        .map_err(|e| runtime_error(ruby, e));
    put_vm(id, state);
    result
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

/// Finalizer entry: drop the VM `id` and everything the binding retained for
/// it. Tolerant — it may run on a different thread (the registry is
/// thread-local) and must never raise.
fn drop_vm(ruby: &Ruby, id: u64) {
    if let Some(mut state) = VMS.with(|vms| vms.borrow_mut().remove(&id)) {
        state.dispose();
    }
    forget_vm(ruby, id);
}

fn take_vm(ruby: &Ruby, id: u64) -> Result<ScriptState, Error> {
    match VMS.with(|vms| vms.borrow_mut().remove(&id)) {
        Some(mut state) => {
            IN_FLIGHT.with(|s| {
                s.borrow_mut().insert(id);
            });
            arm_limits(&mut state, id);
            Ok(state)
        }
        None => Err(runtime_error(
            ruby,
            format!("unknown VM id {id} (used from another thread, or re-entered from a callback)"),
        )),
    }
}

/// Put a VM back after a host entry, clearing its in-flight marker and deadline.
fn put_vm(id: u64, state: ScriptState) {
    disarm_limits(id);
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
            let _ = listeners.funcall::<_, _, Value>("delete", (key,));
        }
    }
    if let Some(modules) = hash_const(ruby, "MODULES") {
        let _ = modules.funcall::<_, _, Value>("delete", (id,));
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
            let _ = callbacks.funcall::<_, _, Value>("delete", (key,));
        }
    }
    if let Some(l) = LIMITS.with(|m| m.borrow_mut().remove(&id)) {
        l.running.store(false, Ordering::Relaxed);
    }
    TIMERS.with(|m| {
        m.borrow_mut().remove(&id);
    });
    BRIDGED.with(|s| {
        s.borrow_mut().remove(&id);
    });
    HANDLES.with(|h| h.borrow_mut().retain(|_, (vm, _)| *vm != id));
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
        // Timer registrations are handled in Rust, not dispatched to Ruby.
        if let Some(op) = kind.strip_prefix("zipp:timer:") {
            return handle_timer_op(vm_id, op, args);
        }
        let ctx_ptr: *mut (dyn HostCtx + 'static) = unsafe { std::mem::transmute(ctx) };
        let prev = ACTIVE_CTX.with(|c| c.replace(Some((vm_id, ctx_ptr))));
        let result = call_ruby_back(kind, args);
        ACTIVE_CTX.with(|c| c.set(prev));
        result
    })
}

fn call_ruby_back(kind: &str, args: &[HostValue]) -> Result<HostValue, String> {
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
    let result: Value = callable.call(ruby_args).map_err(|e| e.to_string())?;
    Ok(ruby_to_host(&ruby, result))
}

/// The active reentry context for `vm_id`, if a callback for that VM is on the
/// stack.
fn active_ctx(vm_id: u64) -> Option<*mut (dyn HostCtx + 'static)> {
    ACTIVE_CTX
        .with(|c| c.get())
        .and_then(|(id, ptr)| (id == vm_id).then_some(ptr))
}

/// Raise the mapped Ruby exception for a rendered JS error message.
fn js_error_from_message(ruby: &Ruby, msg: &str) -> Error {
    js_error(ruby, &JsError::from_message(msg))
}

fn runtime_error(ruby: &Ruby, msg: String) -> Error {
    Error::new(ruby.exception_runtime_error(), msg)
}

/// Reinterpret a class we just defined (a subclass of Exception) as an
/// `ExceptionClass`, which is what [`Error::new`] wants.
fn to_exception_class(class: RClass) -> ExceptionClass {
    ExceptionClass::from_value(class.as_value()).expect("defined a subclass of Exception")
}

/// Build the Ruby exception for a structured JS error, by its `name`. Carries
/// the JS error `name` as `#js_name` and the JS `stack` as the Ruby backtrace,
/// as quickjs.rb does.
fn js_error(ruby: &Ruby, e: &JsError) -> Error {
    // A JS error name with no dedicated class falls back to ZippRb::Error (the
    // base), not to a bare RuntimeError, so `#js_name` stays available.
    // A host abort is a timeout (the watchdog set the flag), reported as
    // InterruptedError rather than the engine's RangeError.
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
        HostValue::String(s) => ruby.into_value(s.clone()),
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
                let _ = hash.aset(ruby.into_value(key.clone()), host_to_ruby(ruby, value));
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

/// Ruby value → `HostValue`, for a callback's arguments. Handles nil, bool,
/// Integer/Float, String, Array and Hash (string keys); anything else becomes
/// its `to_s`.
fn ruby_to_host(ruby: &Ruby, v: Value) -> HostValue {
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
    if let Ok(i) = i64::try_convert(v) {
        return HostValue::Number(i as f64);
    }
    if let Ok(f) = f64::try_convert(v) {
        return HostValue::Number(f);
    }
    if let Ok(s) = String::try_convert(v) {
        return HostValue::String(s);
    }
    if let Some(ary) = RArray::from_value(v) {
        return HostValue::Array(ary.into_iter().map(|e| ruby_to_host(ruby, e)).collect());
    }
    if let Some(hash) = RHash::from_value(v) {
        let mut props = Vec::new();
        let _ = hash.foreach(|k: Value, val: Value| -> Result<ForEach, Error> {
            let key: String = k.funcall("to_s", ()).unwrap_or_default();
            props.push((key, ruby_to_host(ruby, val)));
            Ok(ForEach::Continue)
        });
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
        ] {
            if let Ok(class) = module.define_class(name, base_rclass) {
                m.insert(name, to_exception_class(class));
            }
        }
    });

    module.define_singleton_method("_create", function!(create, 0))?;
    module.define_singleton_method("_configure_limits", function!(configure_limits, 4))?;
    module.define_singleton_method("_eval", function!(eval, 2))?;
    module.define_singleton_method("_call", function!(call, 3))?;
    module.define_singleton_method("_handle", function!(handle, 2))?;
    module.define_singleton_method("_call_handle", function!(call_handle, 3))?;
    module.define_singleton_method("_get_handle", function!(get_handle, 2))?;
    module.define_singleton_method("_release_handle", function!(release_handle, 2))?;
    module.define_singleton_method("_define_function", function!(define_function, 3))?;
    module.define_singleton_method("_set_module_provider", function!(set_module_provider, 4))?;
    module.define_singleton_method("_import", function!(import_module, 2))?;
    module.define_singleton_method("_enable_timers", function!(enable_timers, 1))?;
    module.define_singleton_method("_eval_async", function!(eval_async, 3))?;
    module.define_singleton_method("_on_log", function!(on_log, 2))?;
    module.define_singleton_method("_on_unhandled_rejection", function!(on_unhandled_rejection, 2))?;
    module.define_singleton_method("_drain_jobs", function!(drain_jobs, 1))?;
    module.define_singleton_method("_dispose", function!(dispose, 1))?;
    module.define_singleton_method("_drop", function!(drop_vm, 1))?;
    module.define_singleton_method("_disposed?", function!(disposed, 1))?;
    Ok(())
}
