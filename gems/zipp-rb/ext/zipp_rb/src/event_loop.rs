//! Timers, `eval_async`, `import`, and microtask draining.

use crate::*;

/// A scheduled timer for a VM.
pub(crate) struct TimerEntry {
    pub(crate) id: u64,
    pub(crate) due_ms: u64,
    pub(crate) repeat_ms: Option<u64>,
}

/// The JS `setTimeout`/`setInterval` shims installed by `enable_timers`. Their
/// registrations cross to Rust via `__zippHostCallValue("zipp:timer:...")`;
/// Rust fires them by calling `__zipp_fire_timer`. A one-shot timer deletes
/// its closure before running, so fired timers do not accumulate.
pub(crate) const TIMER_SHIMS: &str = r#"
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

/// Monotonic milliseconds since the extension loaded, for timer deadlines.
/// Immune to wall-clock jumps.
pub(crate) fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

thread_local! {
    /// Next `eval_async`/`import` settlement key.
    pub(crate) static NEXT_SETTLE: Cell<u64> = const { Cell::new(1) };
    /// Settled `eval_async`/`import` results, by key: `(fulfilled, value)`.
    /// Delivered through the host channel, so no result is parked on
    /// `globalThis` and no polling script is compiled.
    pub(crate) static SETTLED: RefCell<HashMap<u64, (bool, HostValue)>> = RefCell::new(HashMap::new());
    /// Scheduled timers per VM (opt-in `feature_timeout`).
    pub(crate) static TIMERS: RefCell<HashMap<u64, Vec<TimerEntry>>> = RefCell::new(HashMap::new());
}

/// Enable `setTimeout`/`setInterval` on VM `id` (opt-in, like quickjs.rb's
/// `feature_timeout`): install the host handler and the JS shims that register
/// timers with Rust.
pub(crate) fn enable_timers(ruby: &Ruby, id: u64) -> Result<(), Error> {
    with_entry(ruby, id, |entry| {
        install_handler(ruby, entry)?;
        entry
            .run(ruby, |s| s.eval_in_context_structured(TIMER_SHIMS))?
            .map(|_| ())
            .map_err(|e| js_error(ruby, id, &e))
    })
}

/// Handle `__zippHostCallValue("zipp:timer:...", id, delay, repeat)`.
pub(crate) fn handle_timer_op(vm_id: u64, op: &str, args: &[HostValue]) -> Result<HostValue, String> {
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
pub(crate) fn handle_settle(args: &[HostValue]) -> Result<HostValue, String> {
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
pub(crate) fn fire_due_timers(ruby: &Ruby, entry: &mut Entry) -> Result<bool, Error> {
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
pub(crate) fn next_timer_due(vm_id: u64) -> Option<u64> {
    TIMERS.with(|m| m.borrow().get(&vm_id).and_then(|l| l.iter().map(|t| t.due_ms).min()))
}

pub(crate) fn next_settle_key() -> u64 {
    NEXT_SETTLE.with(|c| {
        let v = c.get();
        c.set(v.wrapping_add(1));
        v
    })
}

/// JS that reports `promise`'s settlement for `key` through the host channel.
pub(crate) fn settle_js(promise: &str, key: u64) -> String {
    format!(
        "({promise}).then(\n\
           v => __zippHostCallValue(\"zipp:settle\", {key}, true, v),\n\
           e => __zippHostCallValue(\"zipp:settle\", {key}, false, \
                ((e && e.name) ? e.name + ': ' : '') + String((e && e.message) || e)));"
    )
}

/// The settlement reported for `key` as a Ruby value, or the rejection raised.
pub(crate) fn settled_to_ruby(ruby: &Ruby, id: u64, settled: (bool, HostValue)) -> Result<Value, Error> {
    match settled {
        (true, hv) => Ok(host_to_ruby(ruby, &hv)),
        (false, HostValue::String(msg)) => Err(js_error_from_message(ruby, id, &msg)),
        (false, _) => Err(base_error(ruby, "the promise was rejected")),
    }
}

/// Await-capable evaluation (quickjs.rb's `eval_code` shape): run `src` as an
/// async function body, then drive the event loop — draining microtasks and
/// firing timers — until the top-level promise settles (or `max_ms` elapses).
pub(crate) fn eval_async(ruby: &Ruby, id: u64, src: String, max_ms: u64) -> Result<Value, Error> {
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
            deliver_interrupts(ruby)?;
            if fire_due_timers(ruby, entry)? {
                continue;
            }
            // No timers: the promise can never settle here.
            let Some(due) = next_timer_due(id) else { return Ok(None) };
            let wait = due.min(deadline).saturating_sub(now_ms()).clamp(1, 20);
            sleep_without_gvl(ruby, Duration::from_millis(wait))?;
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
pub(crate) fn import_module(ruby: &Ruby, id: u64, specifier: String) -> Result<Value, Error> {
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

/// Run queued microtasks (promise reactions, `queueMicrotask`).
pub(crate) fn drain_jobs(ruby: &Ruby, id: u64) -> Result<(), Error> {
    with_entry(ruby, id, |entry| entry.run(ruby, |s| s.run_microtasks()))
}
