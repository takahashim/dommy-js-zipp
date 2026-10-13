//! Classic scripts compiled once per process and loaded into any VM.
//!
//! A host loads the same large scripts into every VM it builds (a DOM bridge's
//! runtime, a framework bundle); parsing and compiling them is most of a VM's
//! start-up. A compiled program holds no VM state, so one compilation serves
//! every VM on every thread. The cache is keyed by the source itself, so a
//! changed script is a new entry, and bounded by total source bytes.

use crate::*;
use std::collections::VecDeque;
use zipp_vm::embed::CompiledScript;

/// Sources kept compiled, by total bytes; the oldest entries go first.
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct Cache {
    scripts: HashMap<Arc<str>, CompiledScript>,
    order: VecDeque<Arc<str>>,
    bytes: usize,
    hits: u64,
    misses: u64,
}

fn cache() -> std::sync::MutexGuard<'static, Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    // A panic never happens while the lock is held (compiling runs outside
    // it), but a poisoned cache is still a valid one.
    CACHE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// `src` compiled, from the cache or compiled now and cached. `None` when it
/// does not compile: the caller loads the source itself for the structured
/// error. Touches no Ruby API (it runs with the GVL released).
pub(crate) fn compiled(src: &str) -> Option<CompiledScript> {
    {
        let mut c = cache();
        if let Some(script) = c.scripts.get(src).cloned() {
            c.hits += 1;
            return Some(script);
        }
        c.misses += 1;
    }
    let script = CompiledScript::compile(src).ok()?;
    let mut c = cache();
    if !c.scripts.contains_key(src) {
        let key: Arc<str> = src.into();
        c.bytes += key.len();
        c.order.push_back(key.clone());
        c.scripts.insert(key, script.clone());
        while c.bytes > MAX_SOURCE_BYTES && c.order.len() > 1 {
            let Some(old) = c.order.pop_front() else { break };
            c.bytes -= old.len();
            c.scripts.remove(&old);
        }
    }
    Some(script)
}

/// `ZippRb.precompile(src)`: compile `src` into the cache without a VM (before
/// forking workers, say, so they share it). False when it does not compile.
pub(crate) fn precompile(ruby: &Ruby, src: String) -> Result<bool, Error> {
    std::panic::catch_unwind(AssertUnwindSafe(|| compiled(&src).is_some()))
        .map_err(|_| base_error(ruby, "zipp: internal engine error while compiling"))
}

/// `ZippRb.compile_cache_stats`: entries, source bytes, hits and misses.
pub(crate) fn compile_cache_stats(ruby: &Ruby) -> RHash {
    let c = cache();
    let hash = ruby.hash_new();
    let set = |name: &str, v: u64| {
        let _ = hash.aset(ruby.to_symbol(name), v);
    };
    set("entries", c.scripts.len() as u64);
    set("bytes", c.bytes as u64);
    set("hits", c.hits);
    set("misses", c.misses);
    hash
}
