//! Value conversion between `HostValue` and Ruby values.

use crate::*;

/// Nesting depth past which a Ruby Array/Hash crossing into JS becomes `null`
/// (and a cycle becomes `null` where it closes) — the engine's own limit for
/// a host value tree, so nothing deeper would survive the crossing anyway.
pub(crate) const MAX_HOST_DEPTH: usize = 64;

/// `HostValue` → Ruby value. Structured data (arrays, plain objects) becomes
/// Array/Hash recursively; a JS function/Map/Set/Date/… (`Opaque`) has no Ruby
/// representation here and becomes `nil`.
pub(crate) fn host_to_ruby(ruby: &Ruby, v: &HostValue) -> Value {
    match v {
        // quickjs.rb compatibility: undefined and NaN are sentinel symbols,
        // null is nil, and a function/Map/Set/Date/… (`Opaque`) is nil here.
        HostValue::Undefined => ruby.to_symbol("undefined").as_value(),
        HostValue::Null | HostValue::Opaque => ruby.qnil().as_value(),
        HostValue::Bool(b) => ruby.into_value(*b),
        HostValue::Number(n) if n.is_nan() => ruby.to_symbol("NaN").as_value(),
        HostValue::Number(n) => number_to_ruby(ruby, *n),
        HostValue::String(s) => ruby.str_new(s).as_value(),
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
                let _ = hash.aset(ruby.str_new(key), host_to_ruby(ruby, value));
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

/// Ruby value → `HostValue`, for call arguments and callback results.
pub(crate) fn ruby_to_host(ruby: &Ruby, v: Value) -> HostValue {
    to_host(ruby, v, &mut Vec::new())
}

/// [`ruby_to_host`]'s walk. Handles nil, bool, Integer/Float, the
/// `:undefined`/`:NaN` sentinels (so a value read from JS round-trips), other
/// Symbols and Strings, Array and Hash (keys by `to_s`); anything else becomes
/// its `to_s`. `path` holds the containers being converted: a container met
/// again on its own path (a cycle) or nested past [`MAX_HOST_DEPTH`] becomes
/// `null`, as the engine treats such structures when converting the other
/// way, instead of recursing until the stack overflows.
pub(crate) fn to_host(ruby: &Ruby, v: Value, path: &mut Vec<rb_sys::VALUE>) -> HostValue {
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
    // Integer and Float by class: `i64::try_convert` would accept a Float (or
    // Rational) and truncate it.
    if let Some(i) = Integer::from_value(v) {
        return HostValue::Number(match i.to_i64() {
            Ok(n) => n as f64,
            Err(_) => f64::try_convert(v).unwrap_or(f64::NAN),
        });
    }
    if let Some(f) = Float::from_value(v) {
        return HostValue::Number(f.to_f64());
    }
    if let Some(sym) = Symbol::from_value(v) {
        let name = sym.name().map(|n| n.into_owned()).unwrap_or_default();
        return match name.as_str() {
            "undefined" => HostValue::Undefined,
            "NaN" => HostValue::Number(f64::NAN),
            _ => HostValue::String(name),
        };
    }
    if let Some(rs) = RString::from_value(v) {
        if let Ok(s) = String::try_convert(v) {
            return HostValue::String(s);
        }
        // Bytes that are not valid UTF-8 (a binary string, or a UTF-8 one with
        // a stray byte): each invalid sequence becomes U+FFFD, as quickjs.rb
        // does — not an empty string, which hid what the page was given.
        // SAFETY: `as_slice` borrows the string's buffer, valid until Ruby
        // modifies or frees the string. `v` is on this stack (so the GC keeps
        // it), and no Ruby code runs while the slice is borrowed:
        // `from_utf8_lossy` copies it out without calling Ruby.
        let bytes = unsafe { rs.as_slice() };
        return HostValue::String(String::from_utf8_lossy(bytes).into_owned());
    }
    let raw = v.as_raw();
    if let Some(ary) = RArray::from_value(v) {
        if path.len() >= MAX_HOST_DEPTH || path.contains(&raw) {
            return HostValue::Null;
        }
        path.push(raw);
        let items = ary.into_iter().map(|e| to_host(ruby, e, path)).collect();
        path.pop();
        return HostValue::Array(items);
    }
    if let Some(hash) = RHash::from_value(v) {
        if path.len() >= MAX_HOST_DEPTH || path.contains(&raw) {
            return HostValue::Null;
        }
        path.push(raw);
        let mut props = Vec::new();
        let _ = hash.foreach(|k: Value, val: Value| -> Result<ForEach, Error> {
            let key: String = k.funcall("to_s", ()).unwrap_or_default();
            props.push((key, to_host(ruby, val, path)));
            Ok(ForEach::Continue)
        });
        path.pop();
        return HostValue::Object(props);
    }
    HostValue::String(v.funcall::<_, _, String>("to_s", ()).unwrap_or_default())
}

/// A JS number as Ruby Integer when it is an exact integer, else Float. The
/// engine stores every number as `f64`, so this is the closest match to
/// quickjs.rb's Integer/Float split.
pub(crate) fn number_to_ruby(ruby: &Ruby, n: f64) -> Value {
    if n.is_finite() && n.fract() == 0.0 && (i64::MIN as f64..=i64::MAX as f64).contains(&n) {
        ruby.into_value(n as i64)
    } else {
        ruby.into_value(n)
    }
}

/// Quote a Rust string as a JS string literal (double-quoted, minimal escapes).
pub(crate) fn js_string(s: &str) -> String {
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
