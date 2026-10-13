//! Break down Zipp's cost for a source file the way the Ruby binding uses the
//! engine: `run_init` (bundle + boot) and repeated `eval_in_context` /
//! `eval_in_context_structured` on a warm VM. This attributes the "load is Nx
//! slower" number to the engine vs the binding.
//!
//!   cargo run --release --manifest-path benchmarks/compile_bench/Cargo.toml -- benchmarks/fixtures/react.umd.js

use std::time::Instant;
use zipp_vm::embed::{compile_script, HostValueBudget, ScriptState};

fn main() {
    let path = std::env::args().nth(1).expect("usage: zipp_compile_bench <file.js>");
    let raw = std::fs::read_to_string(&path).expect("read source");
    let boot = "globalThis.self = globalThis; globalThis.process = { env: {} };\n";
    let n: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(30);

    // compile_script alone: parse + compile + boot the VM (no run).
    let compile_only = med(n, || {
        let _st: ScriptState = compile_script(&format!("{boot}{raw}")).unwrap();
    });

    // Fresh VM: compile boot+bundle, run it (what a first `load` costs).
    let fresh = med(n, || {
        let mut st: ScriptState = compile_script(&format!("{boot}{raw}")).unwrap();
        st.run_init().unwrap();
    });

    // Warm VM: repeated eval_in_context (what `load` does after the first call).
    let mut st = compile_script(&format!("{boot}{raw}")).unwrap();
    st.run_init().unwrap();
    let eval = med(n, || {
        let _ = st.eval_in_context(&raw);
    });

    // Warm VM: eval_in_context_structured (what `eval` does).
    let mut st2 = compile_script(&format!("{boot}{raw}")).unwrap();
    st2.run_init().unwrap();
    let rich = med(n, || {
        let mut budget = HostValueBudget::default();
        let _ = st2.eval_in_context_rich(&raw, &mut budget);
    });

    println!("source: {path} ({} bytes), median of {n}", raw.len());
    println!("  compile_script only (parse+compile+VMnew) {:8.3} ms", compile_only);
    println!("  fresh VM compile(boot+bundle)+run_init {:8.3} ms", fresh);
    println!("  run only (~fresh - compile)              {:8.3} ms", fresh - compile_only);
    println!("  warm eval_in_context(bundle)           {:8.3} ms", eval);
    println!("  warm eval_in_context_rich(bundle)      {:8.3} ms", rich);
}

fn med<F: FnMut()>(n: usize, mut f: F) -> f64 {
    let mut xs: Vec<f64> = (0..n)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}
