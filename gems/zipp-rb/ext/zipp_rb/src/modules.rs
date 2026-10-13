//! In-memory ES module provider.

use crate::*;

/// Install an in-memory module loader on VM `id`: `sources` maps a resolved
/// module path (String) to its source (a String, or a Proc called with the
/// path). No file is read for a module served this way.
pub(crate) fn set_module_provider(
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

/// Call the module hook registered under `key` with `args`, returning its
/// String answer (nil, a non-String or a raised `StandardError` answer
/// `None`; a non-local exit is recorded for re-delivery).
fn call_module_hook(id: u64, key: &str, args: &[Value]) -> Option<String> {
    let ruby = Ruby::get().ok()?;
    if fatal_pending(id) {
        return None;
    }
    let hook = hash_const(&ruby, "LISTENERS")?.get(key)?;
    match hook.funcall::<_, _, Value>("call", args) {
        Ok(answer) if answer.is_nil() => None,
        Ok(answer) => String::try_convert(answer).ok(),
        Err(e) => {
            if !e.is_kind_of(ruby.exception_standard_error()) {
                capture_ruby_error(&ruby, id, e);
            }
            None
        }
    }
}

/// Resolve module specifiers on VM `id` through `resolver`, a Proc called
/// with `(referrer_dir, specifier)` (`referrer_dir` nil for the entry) that
/// answers the module path to load, or nil for the default lexical join.
pub(crate) fn set_module_resolver(ruby: &Ruby, id: u64, resolver: Value) -> Result<(), Error> {
    mark_bridged(id);
    let key = format!("module_resolver:{id}");
    store_listener(ruby, &key, resolver)?;
    let hook = move |dir: Option<&std::path::Path>, spec: &str| -> Option<std::path::PathBuf> {
        let ruby = Ruby::get().ok()?;
        let dir = match dir {
            Some(d) => ruby.str_new(&d.to_string_lossy()).as_value(),
            None => ruby.qnil().as_value(),
        };
        let args = [dir, ruby.str_new(spec).as_value()];
        call_module_hook(id, &key, &args).map(std::path::PathBuf::from)
    };
    with_entry(ruby, id, |entry| entry.run(ruby, |s| s.set_module_resolver(Rc::new(hook))))
}

/// Answer `import.meta.url` on VM `id` through `url_of`, a Proc called with a
/// loaded module's path that answers its URL (or nil for none).
pub(crate) fn set_module_meta_url(ruby: &Ruby, id: u64, url_of: Value) -> Result<(), Error> {
    mark_bridged(id);
    let key = format!("module_meta_url:{id}");
    store_listener(ruby, &key, url_of)?;
    let hook = move |path: &std::path::Path| -> Option<String> {
        let ruby = Ruby::get().ok()?;
        let args = [ruby.str_new(&path.to_string_lossy()).as_value()];
        call_module_hook(id, &key, &args)
    };
    with_entry(ruby, id, |entry| entry.run(ruby, |s| s.set_module_meta_url(Rc::new(hook))))
}
