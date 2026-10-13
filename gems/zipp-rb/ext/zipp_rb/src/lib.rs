//! ZippRb — a Ruby binding for the Zipp engine.
//!
//! The engine's `ScriptState` is `!Send` (the JIT pins heap/register pointers,
//! so a VM must stay on the thread that built it). Magnus's typed-data wrapper
//! requires `Send`, and the plan forbids faking it with `unsafe impl Send`, so
//! VMs live in a thread-local registry keyed by an integer id; the Ruby
//! `ZippRb::VM` class (defined in lib/zipp_rb.rb) holds that id and delegates
//! to the module functions `init` registers (one per entry point; see the
//! modules below).
//!
//! Ruby callbacks are registered as JS globals whose bodies call
//! `__zippHostCallValue`; the procs themselves live in a Ruby-side Hash
//! (`ZippRb::CALLBACKS`) so the Ruby GC roots them, and the Rust handler looks
//! them up by kind on each call.
//!
//! Every host entry goes through an [`Entry`]: it takes the VM out of the
//! registry, catches an engine panic (the VM is then discarded — "poisoned" —
//! rather than reused in an unknown state), puts the VM back on every exit
//! path, and finally re-raises whatever Ruby exception or non-local exit a
//! callback raised during the entry (see [`finish`]).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use magnus::{
    block::Proc, error::ErrorType, function, prelude::*, r_hash::ForEach, rb_sys::AsRawValue,
    Class, Error, Exception, ExceptionClass, Float, Integer, RArray, RClass, RHash, RModule,
    RObject, RString, Ruby, Symbol, Value,
};

use zipp_vm::embed::{
    compile_script, ConsoleLevel, HostCallValues, HostCtx, HostHandle, HostValue, JsError,
    ScriptState,
};

mod nogvl;
mod registry;
mod errors;
mod convert;
mod limits;
mod event_loop;
mod handles;
mod callbacks;
mod modules;
mod evaluate;

use nogvl::*;
use registry::*;
use errors::*;
use convert::*;
use limits::*;
use event_loop::*;
use handles::*;
use callbacks::*;
use modules::*;
use evaluate::*;

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
            "NoAwaitError",
        ] {
            if let Ok(class) = module.define_class(name, base_rclass) {
                m.insert(name, to_exception_class(class));
            }
        }
    });

    module.define_singleton_method("_create", function!(create, 0))?;
    module.define_singleton_method("_configure_limits", function!(configure_limits, 4))?;
    module.define_singleton_method("_eval", function!(eval, 2))?;
    module.define_singleton_method("_load", function!(load, 2))?;
    module.define_singleton_method("_call", function!(call, 4))?;
    module.define_singleton_method("_set_script_name", function!(set_script_name, 2))?;
    module.define_singleton_method("_handle", function!(handle, 2))?;
    module.define_singleton_method("_call_handle", function!(call_handle, 3))?;
    module.define_singleton_method("_get_handle", function!(get_handle, 2))?;
    module.define_singleton_method("_release_handle", function!(release_handle, 2))?;
    module.define_singleton_method("_queue_release", function!(queue_release, 2))?;
    module.define_singleton_method("_define_function", function!(define_function, 4))?;
    module.define_singleton_method("_set_module_provider", function!(set_module_provider, 4))?;
    module.define_singleton_method("_set_module_resolver", function!(set_module_resolver, 2))?;
    module.define_singleton_method("_set_module_meta_url", function!(set_module_meta_url, 2))?;
    module.define_singleton_method("_import", function!(import_module, 2))?;
    module.define_singleton_method("_enable_timers", function!(enable_timers, 1))?;
    module.define_singleton_method("_eval_async", function!(eval_async, 3))?;
    module.define_singleton_method("_on_log", function!(on_log, 2))?;
    module.define_singleton_method("_set_promise_rejection_hook", function!(set_promise_rejection_hook, 2))?;
    module.define_singleton_method("_on_unhandled_rejection", function!(on_unhandled_rejection, 2))?;
    module.define_singleton_method("_drain_jobs", function!(drain_jobs, 1))?;
    module.define_singleton_method("_dispose", function!(dispose, 1))?;
    module.define_singleton_method("_drop", function!(drop_vm, 1))?;
    module.define_singleton_method("_memory_usage", function!(memory_usage, 1))?;
    module.define_singleton_method("_gc", function!(gc, 1))?;
    module.define_singleton_method("_disposed?", function!(disposed, 1))?;
    module.define_singleton_method("_poisoned?", function!(poisoned, 1))?;
    Ok(())
}
