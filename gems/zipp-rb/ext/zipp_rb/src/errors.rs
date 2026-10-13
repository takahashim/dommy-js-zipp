//! JS errors as Ruby exceptions, and Ruby exceptions raised by callbacks.

use crate::*;

/// Ruby exceptions a callback raised during one entry, kept for re-raising.
/// Only the most recent ones matter (a JS loop that catches thousands of them
/// must not grow this without bound).
pub(crate) const MAX_PENDING_ERRORS: usize = 16;

/// The JS error message a callback produces while a Ruby interrupt is pending.
pub(crate) const INTERRUPT_PENDING_MSG: &str = "Error: a Ruby interrupt is pending";

/// A non-local exit a callback raised that JS must not be able to swallow:
/// a non-`StandardError` exception (`Interrupt`, `SystemExit`, the exception
/// `Timeout` and `Thread#raise` deliver …), `Thread#kill`, `throw`, or
/// `break`. It is re-delivered when the VM returns to Ruby. An exception is
/// kept in `ZippRb::PENDING` (so the GC roots it), not here.
pub(crate) enum Fatal {
    Exception,
    /// `Thread#kill` or `throw`: the captured jump itself, resumed when the VM
    /// returns to Ruby. Re-issuing a kill would be a no-op (the thread is
    /// already marked to-be-killed), and a throw's tag and value live in the
    /// thread's errinfo, which the resumed jump picks up. (Ruby 3.2's
    /// `Timeout` delivers its timeout as a throw.)
    Resume(Error),
    Jump(String),
}

thread_local! {
    /// Map a JS error `name` to the Ruby class to raise.
    pub(crate) static ERROR_CLASSES: RefCell<HashMap<&'static str, ExceptionClass>> =
        RefCell::new(HashMap::new());
    /// VMs with entries in `ZippRb::PENDING`, so an entry that recorded
    /// nothing never touches the Ruby Hash.
    pub(crate) static HAS_PENDING: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
    /// Non-local exits awaiting re-delivery, per VM.
    pub(crate) static FATAL: RefCell<HashMap<u64, Fatal>> = RefCell::new(HashMap::new());
}

/// Close a host entry: a non-local exit a callback raised wins over the
/// entry's own result (JS may have caught and swallowed the error standing in
/// for it); the per-entry record of catchable callback exceptions is cleared.
pub(crate) fn finish<T>(ruby: &Ruby, id: u64, result: Result<T, Error>) -> Result<T, Error> {
    let fatal = FATAL.with(|f| f.borrow_mut().remove(&id));
    let fatal_error = fatal.map(|f| fatal_to_error(ruby, id, f));
    if HAS_PENDING.with(|s| s.borrow_mut().remove(&id)) {
        if let Some(pending) = hash_const(ruby, "PENDING") {
            let _ = pending.delete::<_, Value>(id);
            let _ = pending.delete::<_, Value>(format!("fatal:{id}"));
        }
    }
    match fatal_error {
        Some(e) => Err(e),
        None => result,
    }
}

/// The Ruby error that re-delivers a recorded non-local exit.
pub(crate) fn fatal_to_error(ruby: &Ruby, id: u64, fatal: Fatal) -> Error {
    match fatal {
        Fatal::Exception => hash_const(ruby, "PENDING")
            .and_then(|p| p.get(format!("fatal:{id}")))
            .and_then(Exception::from_value)
            .map(Error::from)
            .unwrap_or_else(|| base_error(ruby, "a Ruby exception raised in a callback was lost")),
        // Resume the kill or throw. (On the main thread Thread#kill is an
        // exit, which arrives as a SystemExit exception, not here.)
        Fatal::Resume(jump) => jump,
        Fatal::Jump(what) => base_error(
            ruby,
            &format!("a non-local exit ({what}) out of a JS callback is not supported"),
        ),
    }
}

/// Record a callback's Ruby error. A `StandardError` stays catchable by JS: it
/// becomes the returned JS error message, and the exception is kept so an
/// uncaught throw re-raises the ORIGINAL exception (class, message, backtrace)
/// in Ruby. Anything else is a non-local exit JS must not swallow: it is
/// recorded for re-delivery, the VM is asked to stop (when it has an abort
/// flag), and later callbacks in this entry fail fast.
pub(crate) fn capture_ruby_error(ruby: &Ruby, vm_id: u64, e: Error) -> String {
    let standard = ruby.exception_standard_error();
    match e.error_type() {
        ErrorType::Exception(exc) if exc.is_kind_of(standard) => {
            let msg: String = exc.funcall("message", ()).unwrap_or_default();
            record_pending(ruby, vm_id, &msg, *exc);
            msg
        }
        ErrorType::Error(class, msg) if class.is_inherited(standard) => msg.to_string(),
        ErrorType::Exception(exc) => {
            record_fatal_exception(ruby, vm_id, *exc);
            INTERRUPT_PENDING_MSG.to_string()
        }
        ErrorType::Error(class, msg) => {
            if let Ok(exc) = class.new_instance((msg.to_string(),)) {
                record_fatal_exception(ruby, vm_id, exc);
            }
            INTERRUPT_PENDING_MSG.to_string()
        }
        ErrorType::Jump(tag) => {
            let fatal = match tag {
                magnus::error::Tag::Fatal | magnus::error::Tag::Throw => Fatal::Resume(e.clone()),
                other => Fatal::Jump(other.to_string().to_lowercase()),
            };
            record_fatal(vm_id, fatal);
            INTERRUPT_PENDING_MSG.to_string()
        }
    }
}

pub(crate) fn record_pending(ruby: &Ruby, vm_id: u64, msg: &str, exc: Exception) {
    let Some(pending) = hash_const(ruby, "PENDING") else { return };
    HAS_PENDING.with(|s| s.borrow_mut().insert(vm_id));
    let list = match pending.get(vm_id).and_then(RArray::from_value) {
        Some(list) => list,
        None => {
            let list = ruby.ary_new();
            let _ = pending.aset(vm_id, list);
            list
        }
    };
    if list.len() >= MAX_PENDING_ERRORS {
        let _ = list.shift::<Value>();
    }
    let _ = list.push(ruby.ary_new_from_values(&[ruby.str_new(msg).as_value(), exc.as_value()]));
}

pub(crate) fn record_fatal_exception(ruby: &Ruby, vm_id: u64, exc: Exception) {
    if let Some(pending) = hash_const(ruby, "PENDING") {
        let _ = pending.aset(format!("fatal:{vm_id}"), exc);
        HAS_PENDING.with(|s| s.borrow_mut().insert(vm_id));
    }
    record_fatal(vm_id, Fatal::Exception);
}

/// Record a non-local exit (the first one wins) and stop the VM if it can be
/// stopped.
pub(crate) fn record_fatal(vm_id: u64, fatal: Fatal) {
    FATAL.with(|f| {
        f.borrow_mut().entry(vm_id).or_insert(fatal);
    });
    LIMITS.with(|m| {
        if let Some(l) = m.borrow().get(&vm_id) {
            l.interrupter.abort.store(true, Ordering::Relaxed);
        }
    });
}

pub(crate) fn fatal_pending(vm_id: u64) -> bool {
    FATAL.with(|f| f.borrow().contains_key(&vm_id))
}

/// The recorded callback exception a JS error stands for, if any: the most
/// recent one whose message is what reached JS (by `name: message`, or a bare
/// message for a plain `Error`).
pub(crate) fn pending_exception_for(ruby: &Ruby, vm_id: u64, e: &JsError) -> Option<Exception> {
    if !HAS_PENDING.with(|s| s.borrow().contains(&vm_id)) {
        return None;
    }
    let list = hash_const(ruby, "PENDING")?.get(vm_id).and_then(RArray::from_value)?;
    let rendered = format!("{}: {}", e.name, e.message);
    for i in (0..list.len()).rev() {
        let pair: RArray = list.entry(i as isize).ok()?;
        let sent: String = pair.entry(0).ok()?;
        if sent == rendered || (e.name == "Error" && sent == e.message) {
            return pair.entry::<Value>(1).ok().and_then(Exception::from_value);
        }
    }
    None
}

/// Map a nested (reentrant) evaluation's result. A non-local exit recorded
/// further in surfaces as an error here; the outermost entry re-delivers it.
pub(crate) fn nested_result(ruby: &Ruby, id: u64, result: Result<HostValue, String>) -> Result<Value, Error> {
    if fatal_pending(id) {
        return Err(interrupted_error(ruby, "a Ruby interrupt is pending"));
    }
    match result {
        Ok(hv) => Ok(host_to_ruby(ruby, &hv)),
        Err(msg) => Err(js_error_from_message(ruby, id, &msg)),
    }
}

/// Raise the mapped Ruby exception for a rendered JS error message.
pub(crate) fn js_error_from_message(ruby: &Ruby, vm_id: u64, msg: &str) -> Error {
    js_error(ruby, vm_id, &JsError::from_message(msg))
}

pub(crate) fn runtime_error(ruby: &Ruby, msg: String) -> Error {
    Error::new(ruby.exception_runtime_error(), msg)
}

/// A `ZippRb::Error` (the base class) with `msg`.
pub(crate) fn base_error(ruby: &Ruby, msg: &str) -> Error {
    let class = ERROR_CLASSES
        .with(|m| m.borrow().get("Error").copied())
        .unwrap_or_else(|| ruby.exception_runtime_error());
    Error::new(class, msg.to_string())
}

/// A `ZippRb::InterruptedError` with `msg`.
pub(crate) fn interrupted_error(ruby: &Ruby, msg: &str) -> Error {
    let class = ERROR_CLASSES
        .with(|m| m.borrow().get("InterruptedError").copied())
        .unwrap_or_else(|| ruby.exception_runtime_error());
    Error::new(class, msg.to_string())
}

/// Reinterpret a class we just defined (a subclass of Exception) as an
/// `ExceptionClass`, which is what [`Error::new`] wants.
pub(crate) fn to_exception_class(class: RClass) -> ExceptionClass {
    ExceptionClass::from_value(class.as_value()).expect("defined a subclass of Exception")
}

/// Build the Ruby exception for a structured JS error, by its `name`. Carries
/// the JS error `name` as `#js_name` and the JS `stack` as the Ruby backtrace,
/// as quickjs.rb does. A JS error that stands for a Ruby exception a callback
/// raised (and JS did not catch) re-raises that original exception instead.
pub(crate) fn js_error(ruby: &Ruby, vm_id: u64, e: &JsError) -> Error {
    if let Some(exc) = pending_exception_for(ruby, vm_id, e) {
        return Error::from(exc);
    }
    // A JS error name with no dedicated class falls back to ZippRb::Error (the
    // base), not to a bare RuntimeError, so `#js_name` stays available.
    // A host abort (the deadline passed, or the abort flag was raised) is
    // reported as InterruptedError rather than the engine's RangeError.
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
        // The JS frames (`at f (script:1:2)`), without the stack's
        // `Name: message` header, as the Ruby backtrace.
        if let Some(stack) = &e.stack {
            let frames: Vec<String> = stack
                .lines()
                .map(str::trim)
                .filter(|line| line.starts_with("at "))
                .map(str::to_string)
                .collect();
            if !frames.is_empty() {
                let _ = obj.funcall::<_, _, Value>("set_backtrace", (ruby.ary_from_vec(frames),));
            }
        }
    }
    Error::from(instance)
}
