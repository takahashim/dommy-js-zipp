//! Ruby callbacks callable from JS, listeners, and same-VM re-entry.

use crate::*;

thread_local! {
    /// Next callback id.
    pub(crate) static NEXT_FN: Cell<u64> = const { Cell::new(1) };
    /// The engine reentry context active while a Ruby callback runs, for the
    /// VM currently being evaluated. Lets a nested `vm.eval`/`vm.call` from a
    /// callback drive the SAME VM through the engine's `HostCtx` (the supported
    /// reentry path) instead of the id registry. The raw pointer is valid only
    /// within the callback's dynamic scope; the handler saves and restores it.
    pub(crate) static ACTIVE_CTX: Cell<Option<(u64, *mut (dyn HostCtx + 'static))>> = const { Cell::new(None) };
}

/// Split a `define_function` name (a String/Symbol, or an Array path) into its
/// segments, each as a String.
pub(crate) fn name_segments(name: Value) -> Vec<String> {
    if let Some(arr) = RArray::from_value(name) {
        arr.into_iter()
            .map(|v| v.funcall::<_, _, String>("to_s", ()).unwrap_or_default())
            .collect()
    } else {
        vec![name.funcall::<_, _, String>("to_s", ()).unwrap_or_default()]
    }
}

/// Install the Ruby host handler on VM `id` (idempotent) and mark it bridged.
pub(crate) fn install_handler(ruby: &Ruby, entry: &mut Entry) -> Result<(), Error> {
    let id = entry.id;
    mark_bridged(id);
    entry.run(ruby, |s| s.set_host_call_values(rb_handler(id)))
}

/// Register the Ruby `block` as the JS global function `name` on VM `id`.
pub(crate) fn define_function(
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

/// Register a Ruby block as the console listener for VM `id`; it is called
/// with `(severity_symbol, line)` for every `console.*` line.
pub(crate) fn on_log(ruby: &Ruby, id: u64, block: Value) -> Result<(), Error> {
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
pub(crate) fn on_unhandled_rejection(ruby: &Ruby, id: u64, block: Value) -> Result<(), Error> {
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

/// Store a Ruby block in the `ZippRb::LISTENERS` constant so the GC roots it.
pub(crate) fn store_listener(ruby: &Ruby, key: &str, block: Value) -> Result<(), Error> {
    let module: RModule = ruby.define_module("ZippRb")?;
    let listeners: RHash = module.const_get("LISTENERS")?;
    listeners.aset(key, block)?;
    Ok(())
}

/// Call the listener registered under `key`, if any. A listener's
/// `StandardError` is dropped (a listener has no JS caller to report it to);
/// a non-local exit is recorded for re-delivery like a callback's.
pub(crate) fn call_listener(ruby: &Ruby, vm_id: u64, key: &str, args: &[Value]) {
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

/// The structured host handler backing Ruby callbacks: on
/// `__zippHostCallValue(kind, ...)`, look up the proc registered under `kind`,
/// convert the arguments to Ruby, call it, and convert the result back.
///
/// While the proc runs it publishes `ctx` as the active reentry context for
/// `vm_id`, so a nested `vm.eval`/`vm.call` drives this same VM through the
/// engine's `HostCtx`. `ctx` is moved into a raw pointer (the only handle for
/// its dynamic scope) and restored on return, including on error.
pub(crate) fn rb_handler(vm_id: u64) -> HostCallValues {
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

pub(crate) fn call_ruby_back(vm_id: u64, kind: &str, args: &[HostValue]) -> Result<HostValue, String> {
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
pub(crate) fn active_ctx(vm_id: u64) -> Option<*mut (dyn HostCtx + 'static)> {
    ACTIVE_CTX
        .with(|c| c.get())
        .and_then(|(id, ptr)| (id == vm_id).then_some(ptr))
}
