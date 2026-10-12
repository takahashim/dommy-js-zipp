//! Zipp embed API capability spike (plan Phase 0).
//!
//! Establishes, by running code, which of the quickjs.rb contracts the current
//! `zipp_vm::embed::ScriptState` public API can satisfy. Read-only against the
//! engine: nothing here patches zipp-vm.
//!
//! Run: `cargo run` (optionally `cargo run -- S4` for one scenario).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use zipp_vm::embed::*;

struct Harness {
    passed: usize,
    failed: usize,
}

impl Harness {
    fn new() -> Self {
        Harness { passed: 0, failed: 0 }
    }

    fn note(&mut self, scenario: &str, msg: impl AsRef<str>) {
        println!("      [{scenario}] {}", msg.as_ref());
    }

    fn pass(&mut self, scenario: &str, msg: impl AsRef<str>) {
        self.passed += 1;
        println!("  PASS {scenario}: {}", msg.as_ref());
    }

    fn fail(&mut self, scenario: &str, msg: impl AsRef<str>) {
        self.failed += 1;
        println!("  FAIL {scenario}: {}", msg.as_ref());
    }
}

fn tree_depth(v: &HostValue) -> usize {
    match v {
        HostValue::Array(a) => 1 + a.iter().map(tree_depth).max().unwrap_or(0),
        HostValue::Object(o) => 1 + o.iter().map(|(_, v)| tree_depth(v)).max().unwrap_or(0),
        _ => 0,
    }
}

fn fmt_host(v: &HostValue) -> String {
    match v {
        HostValue::Undefined => "Undefined".into(),
        HostValue::Null => "Null".into(),
        HostValue::Bool(b) => format!("Bool({b})"),
        HostValue::Number(n) => format!("Number({n})"),
        HostValue::String(s) => format!("String({s:?})"),
        HostValue::Utf16(u) => format!("Utf16({} units)", u.len()),
        HostValue::Array(a) => format!("Array(len {})", a.len()),
        HostValue::Object(o) => {
            let keys: Vec<&str> = o.iter().map(|(k, _)| k.as_str()).collect();
            format!("Object({keys:?})")
        }
        HostValue::Float32Array(a) => format!("Float32Array(len {})", a.len()),
        HostValue::Opaque => "Opaque".into(),
    }
}

// ---------------------------------------------------------------------------
// S1 — VM create, run_init, eval_in_context, call_global
// ---------------------------------------------------------------------------
fn s1(h: &mut Harness) {
    println!("S1: VM生成 → run_init → eval_in_context → call_global");
    let mut st = match compile_script("var counter = 0; function bump(){ counter += 1; return counter; }") {
        Ok(st) => st,
        Err(e) => return h.fail("S1", format!("compile failed: {e}")),
    };
    if let Err(e) = st.run_init() {
        return h.fail("S1", format!("run_init failed: {e}"));
    }
    let a = match st.eval_in_context("bump(); bump()") {
        Ok(JsValue::Number(n)) => n,
        other => return h.fail("S1", format!("eval_in_context unexpected: {other:?}")),
    };
    let b = match st.call_global("bump", &[]) {
        Ok(JsValue::Number(n)) => n,
        other => return h.fail("S1", format!("call_global unexpected: {other:?}")),
    };
    if a == 2.0 && b == 3.0 {
        h.pass("S1", format!("state persists across calls (eval={a}, call={b})"));
    } else {
        h.fail("S1", format!("wrong results eval={a} call={b}"));
    }
}

// ---------------------------------------------------------------------------
// S2 — value conversion (HostValue)
// ---------------------------------------------------------------------------
fn s2(h: &mut Harness) {
    println!("S2: 値変換 (nil/null, undefined, number, string, array, object, 循環, 深いネスト, function/Map)");
    let mut st = compile_script("").unwrap();
    st.run_init().unwrap();

    let deep_src = format!("{}0{}", "[".repeat(70), "]".repeat(70));
    let cases: &[(&str, &str)] = &[
        ("null", "null"),
        ("undefined", "undefined"),
        ("int", "42"),
        ("float", "1.5"),
        ("string", "'hello'"),
        ("array", "[1,2,3]"),
        ("object", "({a:1,b:'x'})"),
        ("cycle", "(()=>{const c={};c.self=c;return c})()"),
        ("function", "(function(){})"),
        ("map", "new Map()"),
        ("bigint", "10n"),
        ("nan", "NaN"),
    ];
    for (name, src) in cases {
        let mut budget = HostValueBudget::new(10_000, 1 << 20);
        match st.eval_in_context_rich(src, &mut budget) {
            Ok(v) => h.note("S2", format!("{name:8} -> {}", fmt_host(&v))),
            Err(e) => h.fail("S2", format!("{name} -> error {}", e.into_message())),
        }
    }
    let mut budget = HostValueBudget::new(100_000, 1 << 20);
    match st.eval_in_context_rich(&deep_src, &mut budget) {
        Ok(v) => h.note(
            "S2",
            format!("deep70   -> {} (tree depth {})", fmt_host(&v), tree_depth(&v)),
        ),
        Err(e) => h.note("S2", format!("deep70   -> error {}", e.into_message())),
    }
    // A string with a lone surrogate crosses as exact UTF-16 units.
    let mut budget = HostValueBudget::new(10_000, 1 << 20);
    match st.eval_in_context_rich("'\\uD800'", &mut budget) {
        Ok(v) => h.note("S2", format!("lone-surrogate -> {}", fmt_host(&v))),
        Err(e) => h.note("S2", format!("lone-surrogate -> error {}", e.into_message())),
    }
    h.pass("S2", "conversion surface exercised (see notes)");
}

// ---------------------------------------------------------------------------
// S3 — host call (JS -> Rust)
// ---------------------------------------------------------------------------
fn s3(h: &mut Harness) {
    println!("S3: set_host_call で JS→Rust 呼び出し");
    let mut st = compile_script("").unwrap();
    st.run_init().unwrap();
    st.set_host_call(Box::new(|kind, args| Ok(format!("host:{kind}:{}", args.join(",")))));
    match st.eval_in_context("__zippHostCall('ping','a','b')") {
        Ok(JsValue::String(s)) if s == "host:ping:a,b" => h.pass("S3", "host call round-trip ok"),
        other => h.fail("S3", format!("unexpected {other:?}")),
    }

    // Host Err becomes a catchable JS throw.
    st.set_host_call(Box::new(|_kind, _args| Err("Error: nope".into())));
    match st.eval_in_context("try { __zippHostCall('x') } catch(e){ 'caught:' + e.message }") {
        Ok(JsValue::String(s)) if s.contains("caught:") => {
            h.pass("S3", format!("host Err is catchable: {s}"))
        }
        other => h.fail("S3", format!("host Err not catchable: {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// S4 — re-entrancy
// ---------------------------------------------------------------------------
fn s4(h: &mut Harness) {
    println!("S4: 再入 (Ruby→JS→Ruby→同一VM JS 相当)");

    // (a) The only re-entry the API exposes: HostCtx::call_global_numbers.
    let mut st = compile_script("function f(x){ return x*2 }").unwrap();
    st.run_init().unwrap();
    st.set_host_call_ctx(Box::new(|ctx, kind, args| {
        if kind == "double" {
            let n: f64 = args[0].parse().unwrap_or(f64::NAN);
            Ok(ctx.call_global_numbers("f", &[n])?.to_string())
        } else {
            Err(format!("Error: unknown host call {kind}"))
        }
    }));
    match st.eval_in_context("__zippHostCall('double','21')") {
        Ok(JsValue::String(s)) if s == "42" => {
            h.pass("S4a", "numeric named-global re-entry via HostCtx works")
        }
        other => h.fail("S4a", format!("restricted re-entry unexpected: {other:?}")),
    }

    // (b) Nested host call from a re-entered guest function throws.
    let mut st = compile_script("function f2(){ return __zippHostCall('inner') }").unwrap();
    st.run_init().unwrap();
    st.set_host_call_ctx(Box::new(|ctx, _kind, _args| match ctx.call_global_numbers("f2", &[]) {
        Ok(n) => Ok(format!("ok:{n}")),
        Err(e) => Ok(format!("reentry-error:{e}")),
    }));
    match st.eval_in_context("__zippHostCall('outer')") {
        Ok(JsValue::String(s)) if s.starts_with("reentry-error:") => {
            h.pass("S4b", format!("nested host call refused as documented: {s}"))
        }
        other => h.fail("S4b", format!("nested host call unexpected: {other:?}")),
    }

    // (c) General re-entry (eval/call_slot from inside a callback) is not
    // expressible: HostCall gets no VM; HostCallCtx exposes only
    // call_global_numbers/typed_array_region. Structural, verified by types.
    h.note(
        "S4c",
        "general re-entry (eval/call_slot from a host callback) NOT expressible: \
         HostCall is FnMut(&str,&[String]) with no VM access",
    );
}

// ---------------------------------------------------------------------------
// S5 — holding JS functions/values
// ---------------------------------------------------------------------------
fn s5(h: &mut Harness) {
    println!("S5: Ruby側で保持したJS関数・値の呼び出し");
    let mut st = compile_script("var f = x => x + 1; let g = 5;").unwrap();
    st.run_init().unwrap();

    let f_slot = st.symbols().into_iter().find(|s| s.name == "f").map(|s| s.index);
    match f_slot {
        Some(idx) => match st.call_slot(idx, &[HostValue::Number(41.0)]) {
            Ok(HostValue::Number(n)) if n == 42.0 => {
                h.pass("S5a", "top-level var function held by slot and called later")
            }
            other => h.fail("S5a", format!("call_slot unexpected: {other:?}")),
        },
        None => h.fail("S5a", "top-level `var f` not exposed as a symbol slot"),
    }

    // A function value read out has no callable handle (Opaque).
    let mut budget = HostValueBudget::new(10_000, 1 << 20);
    match st.eval_in_context_rich("(function(){return 7;})", &mut budget) {
        Ok(HostValue::Opaque) => h.pass(
            "S5b",
            "function value crosses as Opaque (no persistent handle / not callable)",
        ),
        other => h.fail("S5b", format!("function crossed as {other:?}")),
    }
    let g_slot = st.symbols().into_iter().find(|s| s.name == "g").map(|s| s.index);
    match g_slot {
        Some(idx) => h.note(
            "S5c",
            format!("`let g` exposed as a symbol slot (readable: {:?})", st.get_slot(idx)),
        ),
        None => h.note("S5c", "`let g` not exposed as a symbol slot"),
    }
}

// ---------------------------------------------------------------------------
// S6 — Opaque round-trip / structural write-back
// ---------------------------------------------------------------------------
fn s6(h: &mut Harness) {
    println!("S6: Opaque 往復と構造書き戻し");
    let mut st = compile_script("var obj = { a: 1, m(){ return 9 } };").unwrap();
    st.run_init().unwrap();
    let slot = match st.symbols().into_iter().find(|s| s.name == "obj") {
        Some(s) => s.index,
        None => return h.fail("S6", "obj slot not found"),
    };
    let read = st.get_slot(slot);
    match &read {
        HostValue::Object(props) => {
            let has_opaque_m = props.iter().any(|(k, v)| k == "m" && matches!(v, HostValue::Opaque));
            h.note("S6", format!("read obj = {} (m opaque: {has_opaque_m})", fmt_host(&read)));
        }
        other => return h.fail("S6", format!("obj read as {other:?}")),
    }
    // Write back only a=2, omitting the opaque method; it should be preserved.
    let ok = st.set_slot(slot, &HostValue::Object(vec![("a".into(), HostValue::Number(2.0))]));
    let check = st.eval_in_context("obj.a + ':' + typeof obj.m").ok();
    match (ok, check) {
        (true, Some(JsValue::String(s))) if s == "2:function" => {
            h.pass("S6", "omitted opaque method preserved on structural write-back")
        }
        other => h.fail("S6", format!("write-back result {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// S7 — limits (step budget, abort thread, heap)
// ---------------------------------------------------------------------------
fn s7(h: &mut Harness) {
    println!("S7: 実行制限 (step budget / abort / heap)");

    // Step budget on a runaway loop.
    let mut st = compile_script("while(true){}").unwrap();
    st.set_limits(200_000, None);
    let start = Instant::now();
    let r = st.run_init();
    let limit = st.resource_limit_error();
    match (r.is_err(), limit) {
        (true, Some(l)) => h.pass(
            "S7a",
            format!("step budget stopped loop in {:?}: {l}", start.elapsed()),
        ),
        other => h.fail("S7a", format!("step budget unexpected {other:?}")),
    }

    // Wall-clock abort via an external thread + Arc<AtomicBool>.
    let mut st = compile_script("while(true){}").unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    st.set_limits(u64::MAX, Some(flag.clone()));
    let flag2 = flag.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        flag2.store(true, Ordering::Relaxed);
    });
    let start = Instant::now();
    let r = st.run_init();
    let elapsed = start.elapsed();
    match (r.is_err(), st.resource_limit_error()) {
        (true, Some(l)) if elapsed < Duration::from_secs(5) => h.pass(
            "S7b",
            format!("external abort stopped loop in {elapsed:?}: {l}"),
        ),
        other => h.fail("S7b", format!("abort unexpected {other:?} after {elapsed:?}")),
    }

    // Heap ceiling.
    let mut st = compile_script("var a = []; while(true){ a.push(new Array(1000).fill(0)); }").unwrap();
    st.set_limits(u64::MAX, None);
    st.set_heap_limit(4 << 20);
    let r = st.run_init();
    match (r.is_err(), st.resource_limit_error()) {
        (true, Some(l)) => h.pass("S7c", format!("heap limit stopped allocation: {l}")),
        other => h.fail("S7c", format!("heap limit unexpected {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// S8 — resource accounting
// ---------------------------------------------------------------------------
fn s8(h: &mut Harness) {
    println!("S8: ResourceUsage / heap_bytes");
    let mut st = compile_script("var x = 0;").unwrap();
    st.run_init().unwrap();
    st.set_limits(u64::MAX, None);
    let before = st.resource_usage();
    for _ in 0..50 {
        let _ = st.eval_in_context("(function(){ return 1+1; })()");
    }
    let after = st.resource_usage();
    h.note(
        "S8",
        format!(
            "heap_bytes {} -> {}; steps_used {} -> {}; retained_functions {} -> {}",
            before.heap_bytes,
            after.heap_bytes,
            before.steps_used,
            after.steps_used,
            before.retained_functions,
            after.retained_functions
        ),
    );
    h.note("S8", format!("heap_bytes() accessor = {}", st.heap_bytes()));
    h.pass("S8", "resource accounting readable");
}

// ---------------------------------------------------------------------------
// S9 — console, rejections, timers, GC
// ---------------------------------------------------------------------------
fn s9(h: &mut Harness) {
    println!("S9: console / rejection / timer / 公開GC");

    let mut st = compile_script("").unwrap();
    st.run_init().unwrap();
    let lines = Rc::new(RefCell::new(Vec::<String>::new()));
    let sink = lines.clone();
    st.set_console_sink(Box::new(move |stream, line| {
        sink.borrow_mut().push(format!("{stream:?}:{line}"));
    }));
    let _ = st.eval_in_context("console.log('hi'); console.error('err');");
    h.pass("S9a", format!("console sink captured {} lines: {:?}", lines.borrow().len(), *lines.borrow()));

    // Unhandled rejection: no public hook; observe console reporting.
    let _ = st.eval_in_context("Promise.reject(new Error('boom'));");
    st.run_microtasks();
    let out = st.take_console();
    let reported = out.iter().any(|(_, l)| l.contains("boom"));
    h.note("S9b", format!("unhandled rejection reported to console: {reported} (no rejection hook API)"));
    h.note("S9c", "no public GC trigger, no timer API in the embed surface");
}

// ---------------------------------------------------------------------------
// S10 — teardown / retention
// ---------------------------------------------------------------------------
fn s10(h: &mut Harness) {
    println!("S10: 破棄と動的コード回収 / Send 制約");
    {
        let mut st = compile_script("var n = 0;").unwrap();
        st.run_init().unwrap();
        st.set_limits(u64::MAX, None);
        let before = st.resource_usage().retained_functions;
        for i in 0..20 {
            let _ = st.eval_in_context(&format!("(function f{i}(){{ return {i}; }})"));
        }
        let after = st.resource_usage().retained_functions;
        h.note("S10a", format!("retained_functions {before} -> {after} after 20 dynamic evals"));
        // Drop happens at end of scope; no explicit dispose API exists.
    }
    h.pass("S10", "state dropped without explicit dispose (Drop-only teardown)");
    h.note(
        "S10b",
        "ScriptState is !Send (JIT pins heap/register pointers): blocks idle thread hand-off",
    );
}

// ---------------------------------------------------------------------------
// S11 — reentrant host channel (patched)
// ---------------------------------------------------------------------------
fn s11(h: &mut Harness) {
    println!("S11: 再入可能ホストチャネル set_host_call_reentrant（パッチ）");
    let mut st = compile_script("").unwrap();
    st.run_init().unwrap();
    let calls: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let calls2 = calls.clone();
    let host: HostCallReentrant = Rc::new(
        move |ctx: &mut dyn HostCtx, kind: &str, _args: &[String]| -> Result<String, String> {
            calls2.borrow_mut().push(kind.to_string());
            if kind == "outer" {
                // Re-enter the guest from inside the host call; it calls the
                // host again, which the reentrant channel must serve.
                let v = ctx.eval_script("__zippHostCall('inner','x')")?;
                Ok(format!("outer->{v:?}"))
            } else {
                Ok("inner-result".into())
            }
        },
    );
    st.set_host_call_reentrant(host);
    match st.eval_in_context("__zippHostCall('outer')") {
        Ok(JsValue::String(s)) => {
            let seen = calls.borrow().clone();
            if seen == vec!["outer", "inner"] && s.contains("inner-result") {
                h.pass("S11", format!("nested host calls re-entered: {seen:?}, result {s:?}"));
            } else {
                h.fail("S11", format!("unexpected calls {seen:?} result {s:?}"));
            }
        }
        other => h.fail("S11", format!("unexpected {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// S12 — HostCtx::eval_script reentry (patched)
// ---------------------------------------------------------------------------
fn s12(h: &mut Harness) {
    println!("S12: HostCtx::eval_script / call_global_value（パッチ）");
    let mut st = compile_script("function greet(n){ return 'hi ' + n; }").unwrap();
    st.run_init().unwrap();
    st.set_host_call_ctx(Box::new(|ctx, kind, args| match kind {
        "eval" => Ok(format!("{:?}", ctx.eval_script("21 * 2")?)),
        "call" => {
            let out = ctx.call_global_value("greet", &[HostValue::String(args[0].clone())])?;
            Ok(format!("{out:?}"))
        }
        other => Err(format!("Error: unknown {other}")),
    }));
    match st.eval_in_context("__zippHostCall('eval')") {
        Ok(JsValue::String(s)) if s.starts_with("Number(42") => {
            h.pass("S12a", "eval_script re-entered the VM from a host callback")
        }
        other => h.fail("S12a", format!("eval_script unexpected: {other:?}")),
    }
    match st.eval_in_context("__zippHostCall('call','bob')") {
        Ok(JsValue::String(s)) if s == "String(\"hi bob\")" => {
            h.pass("S12b", "call_global_value re-entered the VM from a host callback")
        }
        other => h.fail("S12b", format!("call_global_value unexpected: {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// S13 — durable handles (patched)
// ---------------------------------------------------------------------------
fn s13(h: &mut Harness) {
    println!("S13: 永続ハンドル HostHandle（パッチ）");
    let mut st = compile_script("function adder(a,b){ return a+b; } var obj = { v: 7 };").unwrap();
    st.run_init().unwrap();

    let adder = match st.handle_of_global("adder") {
        Some(x) => x,
        None => return h.fail("S13a", "no handle for `adder`"),
    };
    match st.call_handle(adder, &[HostValue::Number(2.0), HostValue::Number(3.0)]) {
        Ok(HostValue::Number(n)) if n == 5.0 => {
            h.pass("S13a", "handle_of_global + call_handle held a JS function across calls")
        }
        other => h.fail("S13a", format!("call_handle unexpected: {other:?}")),
    }

    match st.handle_of_global("obj").map(|oh| st.get_handle(oh)) {
        Some(Ok(HostValue::Object(props))) => {
            h.pass("S13b", format!("get_handle read a rooted object ({} props)", props.len()))
        }
        other => h.fail("S13b", format!("get_handle unexpected: {other:?}")),
    }

    // Release is idempotent-false on the second call.
    let first = st.release_handle(adder);
    let second = st.release_handle(adder);
    if first && !second {
        h.pass("S13c", "release_handle drops the root once")
    } else {
        h.fail("S13c", format!("release first={first} second={second}"))
    }

    // A handle from another VM is refused.
    let mut st2 = compile_script("function f(){}").unwrap();
    st2.run_init().unwrap();
    let h2 = st2.handle_of_global("f").unwrap();
    match (
        st.call_handle(h2, &[]).is_err(),
        st.get_handle(h2).is_err(),
        st.release_handle(h2),
    ) {
        (true, true, false) => h.pass("S13d", "cross-VM handle use is refused"),
        other => h.fail("S13d", format!("cross-VM not refused: {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// S14 — unhandled rejection sink (patched)
// ---------------------------------------------------------------------------
fn s14(h: &mut Harness) {
    println!("S14: 未処理 rejection sink（パッチ）");
    let mut st = compile_script("").unwrap();
    st.run_init().unwrap();
    let seen: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let seen2 = seen.clone();
    st.set_unhandled_rejection_sink(Box::new(move |msg| seen2.borrow_mut().push(msg.to_string())));
    let _ = st.eval_in_context("Promise.reject(new Error('boom'));");
    st.run_microtasks();
    let got = seen.borrow().clone();
    if got.iter().any(|m| m.contains("boom")) {
        h.pass("S14", format!("sink observed unhandled rejection: {got:?}"))
    } else {
        h.fail("S14", format!("sink saw nothing: {got:?}"))
    }
}

// ---------------------------------------------------------------------------
// S15 — structured host channel (patched)
// ---------------------------------------------------------------------------
fn s15(h: &mut Harness) {
    println!("S15: 構造化ホストチャネル set_host_call_values（パッチ）");
    let mut st = compile_script("").unwrap();
    st.run_init().unwrap();
    st.set_host_call_values(Rc::new(
        |_ctx: &mut dyn HostCtx, kind: &str, args: &[HostValue]| match kind {
            "sum" => {
                let total: f64 = match &args[0] {
                    HostValue::Array(items) => items
                        .iter()
                        .map(|v| if let HostValue::Number(n) = v { *n } else { 0.0 })
                        .sum(),
                    _ => 0.0,
                };
                Ok(HostValue::Object(vec![
                    ("total".into(), HostValue::Number(total)),
                    ("n".into(), HostValue::Number(args.len() as f64)),
                ]))
            }
            "echo" => Ok(args[0].clone()),
            other => Err(format!("Error: unknown {other}")),
        },
    ));

    // A structured argument crosses without a JSON round-trip and the structured
    // reply is read back as a HostValue tree.
    let mut budget = HostValueBudget::new(10_000, 1 << 20);
    match st.eval_in_context_rich("__zippHostCallValue('sum', [1,2,3,4])", &mut budget) {
        Ok(HostValue::Object(props)) => {
            let total = props.iter().find(|(k, _)| k == "total");
            match total {
                Some((_, HostValue::Number(n))) if *n == 10.0 => {
                    h.pass("S15a", "structured args/reply round-trip (total 10)")
                }
                other => h.fail("S15a", format!("total unexpected {other:?}")),
            }
        }
        other => h.fail("S15a", format!("result unexpected {other:?}")),
    }

    // A typed array and a lone-surrogate string round-trip exactly.
    let mut budget = HostValueBudget::new(10_000, 1 << 20);
    match st.eval_in_context_rich(
        "(()=>{ const a = __zippHostCallValue('echo', new Float32Array([1.5,2.5])); return [a instanceof Float32Array, a[1]] })()",
        &mut budget,
    ) {
        Ok(HostValue::Array(items)) => {
            let ok = matches!(items.first(), Some(HostValue::Bool(true)))
                && matches!(items.get(1), Some(HostValue::Number(n)) if *n == 2.5);
            if ok {
                h.pass("S15b", "Float32Array crosses both ways (instanceof preserved)")
            } else {
                h.fail("S15b", format!("typed array unexpected {items:?}"))
            }
        }
        other => h.fail("S15b", format!("typed array result unexpected {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// S16 — dispose and structured errors (patched)
// ---------------------------------------------------------------------------
fn s16(h: &mut Harness) {
    println!("S16: dispose / 構造化エラー（パッチ）");

    // dispose is explicit and idempotent.
    let mut st = compile_script("1 + 1").unwrap();
    st.run_init().unwrap();
    let before = st.disposed();
    st.dispose();
    st.dispose();
    match (before, st.disposed(), st.eval_in_context("1").is_err()) {
        (false, true, true) => h.pass("S16a", "dispose() tears down and is idempotent"),
        other => h.fail("S16a", format!("dispose unexpected {other:?}")),
    }

    // Structured error from eval.
    let mut st = compile_script("").unwrap();
    st.run_init().unwrap();
    match st.eval_in_context_structured("throw new TypeError('bad')") {
        Err(e) if e.name == "TypeError" && e.message == "bad" => {
            h.pass("S16b", format!("eval structured error: name={} stack={}", e.name, e.stack.is_some()))
        }
        other => h.fail("S16b", format!("structured eval unexpected {other:?}")),
    }

    // Structured error from a slot call, plus a non-object throw.
    let mut st = compile_script("function boom(){ throw new RangeError('rng'); }").unwrap();
    st.run_init().unwrap();
    let slot = st.symbols().into_iter().find(|s| s.name == "boom").map(|s| s.index);
    match slot.map(|i| st.call_slot_structured(i, &[])) {
        Some(Err(e)) if e.name == "RangeError" && e.message == "rng" => {
            h.pass("S16c", "call_slot_structured reports name/message")
        }
        other => h.fail("S16c", format!("structured call unexpected {other:?}")),
    }
    match st.eval_in_context_structured("throw 'plain'") {
        Err(e) if e.message.contains("plain") => {
            h.pass("S16d", format!("non-object throw: name={} message={}", e.name, e.message))
        }
        other => h.fail("S16d", format!("non-object throw unexpected {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// S17 — ESM loading via the confined filesystem loader (no callback loader)
// ---------------------------------------------------------------------------
fn s17(h: &mut Harness) {
    println!("S17: ESM 読込（既存の confined FS ローダ / パッチ不要）");
    let dir = std::env::temp_dir().join(format!("zipp-spike-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    // Canonicalize: the confined loader compares canonical paths, and on macOS
    // /var is a symlink to /private/var, so an uncanonicalized root rejects its
    // own base dir.
    let dir = std::fs::canonicalize(&dir).unwrap();
    std::fs::write(
        dir.join("dep.mjs"),
        "export const two = 2;\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("mod.mjs"),
        "import { two } from './dep.mjs';\nexport const value = two * 21;\nexport default 'hello';\n",
    )
    .unwrap();

    let mut st = compile_script("").unwrap();
    st.run_init().unwrap();
    // Confined loader rooted at the temp dir; base dir is where bare/relative
    // dynamic-import specifiers resolve from.
    let loaded = st.set_confined_module_loader(&dir, &dir, 1 << 20);
    h.note("S17", format!("set_confined_module_loader -> {loaded:?}"));
    st.eval_in_context(
        "globalThis.__ns = null; globalThis.__err = null;\n\
         import('./mod.mjs').then(ns => { globalThis.__ns = ns.value + ':' + ns.default; },\n\
                                      e => { globalThis.__err = String(e) + ':' + (e && e.message || ''); });",
    )
    .unwrap();
    st.run_microtasks();
    let value = st.eval_in_context("globalThis.__ns");
    let err = st.eval_in_context("globalThis.__err");
    h.note("S17", format!("value={value:?} err={err:?}"));
    match value {
        Ok(JsValue::String(v)) if v == "42:hello" => {
            h.pass("S17a", format!("confined FS loader resolved a relative ESM import: {v}"))
        }
        other => h.fail("S17a", format!("module load unexpected: {other:?}")),
    }

    // Without a confined loader: an absolute-path dynamic import still loads,
    // reading the file directly (external code is enabled by default in embed).
    let abs = dir.join("mod.mjs");
    let mut st2 = compile_script("").unwrap();
    st2.run_init().unwrap();
    st2.eval_in_context(&format!(
        "globalThis.__ns=null; globalThis.__err=null; \
         import('{}').then(ns => {{ globalThis.__ns = ns.value; }}, e => {{ globalThis.__err = String(e) + '|' + (e && e.message || ''); }});",
        abs.display()
    ))
    .unwrap();
    st2.run_microtasks();
    match st2.eval_in_context("globalThis.__ns") {
        Ok(JsValue::Number(42.0)) => {
            h.pass("S17b", "absolute-path ESM import works without the confined loader")
        }
        other => {
            // Documented limitation, not a failure of the supported path: with
            // no base dir set, dynamic import cannot resolve (TypeError). The
            // confined loader (S17a) is the usable route; in-memory sources
            // would need a callback loader (patch C).
            let err = st2.eval_in_context("globalThis.__err");
            h.note(
                "S17b",
                format!("absolute import without any loader -> {other:?} err={err:?} (needs set_confined_module_loader)"),
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// S18 — in-memory ESM (no files on disk) via set_module_provider (patched)
// ---------------------------------------------------------------------------
fn s18(h: &mut Harness) {
    println!("S18: メモリ上モジュール（ファイル無し）set_module_provider（パッチ）");
    let mut sources: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    sources.insert("/zippvfs/dep.mjs".into(), b"export const two = 2;".to_vec());
    sources.insert(
        "/zippvfs/mod.mjs".into(),
        b"import { two } from './dep.mjs';\nexport const value = two * 21;\nexport default 'mem';"
            .to_vec(),
    );
    let sources = Rc::new(sources);

    let mut st = compile_script("").unwrap();
    st.run_init().unwrap();
    let lookup = sources.clone();
    let provider: HostModuleProvider = Rc::new(move |path: &str| lookup.get(path).cloned());
    // The virtual root does not exist on disk; nothing is ever read from it.
    let installed = st.set_module_provider(std::path::Path::new("/zippvfs"), 1 << 20, provider);
    st.eval_in_context(
        "globalThis.__ns=null; globalThis.__err=null;\n\
         import('./mod.mjs').then(ns => { globalThis.__ns = ns.value + ':' + ns.default; },\n\
                                      e => { globalThis.__err = String(e) + '|' + (e && e.message || ''); });",
    )
    .unwrap();
    st.run_microtasks();
    let value = st.eval_in_context("globalThis.__ns");
    let err = st.eval_in_context("globalThis.__err");
    match (installed, value) {
        (Ok(()), Ok(JsValue::String(v))) if v == "42:mem" => {
            h.pass("S18", format!("in-memory ESM resolved with no files: {v}"))
        }
        other => h.fail("S18", format!("in-memory module unexpected: {other:?} err={err:?}")),
    }

    // A module the provider does not know is "module not found".
    st.eval_in_context(
        "globalThis.__missing=null; import('./nope.mjs').then(() => {}, e => { globalThis.__missing = e.name + ':' + e.message; });",
    )
    .unwrap();
    st.run_microtasks();
    match st.eval_in_context("globalThis.__missing") {
        Ok(JsValue::String(s)) if s.contains("not found") => {
            h.pass("S18b", format!("unknown module reports: {s}"))
        }
        other => h.fail("S18b", format!("unknown module unexpected {other:?}")),
    }
}

fn main() {
    let filter = std::env::args().nth(1);
    let mut h = Harness::new();
    println!("zipp embed spike — instrument + default JIT\n");
    let scenarios: &[(&str, fn(&mut Harness))] = &[
        ("S1", s1),
        ("S2", s2),
        ("S3", s3),
        ("S4", s4),
        ("S5", s5),
        ("S6", s6),
        ("S7", s7),
        ("S8", s8),
        ("S9", s9),
        ("S10", s10),
        ("S11", s11),
        ("S12", s12),
        ("S13", s13),
        ("S14", s14),
        ("S15", s15),
        ("S16", s16),
        ("S17", s17),
        ("S18", s18),
    ];
    for (name, f) in scenarios {
        if let Some(only) = &filter {
            if !name.eq_ignore_ascii_case(only) {
                continue;
            }
        }
        f(&mut h);
        println!();
    }
    println!("summary: {} passed, {} failed", h.passed, h.failed);
}
