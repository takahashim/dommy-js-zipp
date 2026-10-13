//! Evaluating scripts and calling JS functions from Ruby.

use crate::*;

/// Evaluate `src` in the VM `id`. The completion value crosses as a structured
/// Ruby value (nil/bool/Integer|Float/String/Array/Hash); an uncaught throw
/// raises the matching `ZippRb::*Error`.
pub(crate) fn eval(ruby: &Ruby, id: u64, src: String) -> Result<Value, Error> {
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

/// Collect garbage now.
pub(crate) fn gc(ruby: &Ruby, id: u64) -> Result<(), Error> {
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
pub(crate) fn memory_usage(ruby: &Ruby, id: u64) -> Result<RHash, Error> {
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

/// Evaluate `src` for its side effects, discarding the completion value. Much
/// cheaper than `eval` when a script's last expression is a large object (a
/// UMD factory's return value, say): `eval` marshals that value into Ruby,
/// whereas this only asks the engine for the shallow `ToString`-class result
/// and drops it.
pub(crate) fn load(ruby: &Ruby, id: u64, src: String) -> Result<(), Error> {
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
pub(crate) fn call(ruby: &Ruby, id: u64, name: String, args: RArray) -> Result<Value, Error> {
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
