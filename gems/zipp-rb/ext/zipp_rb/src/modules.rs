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
