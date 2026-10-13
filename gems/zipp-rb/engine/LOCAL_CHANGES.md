# Local changes to the Zipp engine

`crates/` is upstream Zipp (revision in `UPSTREAM`) plus the changes below.
The branch `vendor/zipp-upstream` holds the pristine imports, so the exact
local diff is always:

    git diff vendor/zipp-upstream HEAD -- gems/zipp-rb/engine/crates

Keep this list current when you change the engine: it is what a merge of a new
upstream import is resolved against (`bin/engine-import`). Each entry says what
the change is for, so a conflict can be settled by keeping the behaviour, not
the exact lines — upstream may have grown its own equivalent, in which case
prefer upstream's and adapt `ext/zipp_rb`. Everything here except the
performance and correctness fixes is additive embed API, and all of it is a
candidate to propose upstream.

## Embed API (crates/zipp-vm/src/embed.rs, vm/host_api.rs, vm/natives.rs, vm/mod.rs, vm/engine/boot.rs)

- **Structured host channel** — `__zippHostCallValue` (`HOST_CALL_VALUE`,
  vm/native.rs + natives.rs), `HostCallValues`,
  `ScriptState::set_host_call_values`. The host closure is an `Rc` called
  without being taken out of the VM, so a nested call re-enters it; arguments
  and reply cross as `HostValue` trees (no JSON). The channel every zipp-rb
  callback uses.
- **`HostCtx` re-entry** — `HostCtx::eval_script`, `call_global_value`, and
  `call_handle_value` / `read_handle_value` / `release_handle_value` (handles
  checked against `Vm::host_owner_id`). Lets a host callback drive the same VM.
- **Durable handles** — `HostHandle`, `ScriptState::handle_of_global`,
  `call_handle`, `get_handle`, `release_handle`; the handle table is a GC root
  (vm/gc.rs `mark_roots`).
- **Structured calls and errors** — `call_global_structured`,
  `eval_in_context_structured`, `JsError { name, message, stack }`; global
  name resolution unified on `host_resolve_global_by_name` so a function
  installed as `globalThis[name]` is found.
- **Unhandled-rejection sink** — `set_unhandled_rejection_sink`
  (vm/async_runtime.rs: with a sink installed, every unhandled rejection is
  reported, not only Test262's narrow case).
- **Lifecycle** — `ScriptState::dispose` / `disposed` (idempotent teardown),
  `gc_now` (`Vm::force_gc`, a no-op while a host conversion holds the GC lock).
- **Module resolver and `import.meta.url`** — `ModuleResolver`,
  `join_module_specifier` (vm/mod.rs), `ScriptState::set_module_resolver` /
  `set_module_meta_url`; every `dir.join(spec)` in vm/engine/modules.rs,
  vm/dispatch.rs (dynamic import) and vm/construct/modules_dispose.rs goes
  through `join_module_specifier`, and `Vm::fill_module_meta_url` gives a
  module's `import.meta` a `url` (it had none). Lets zipp-rb's
  `VM#module_loader=` load modules named by URL or bare name.
- **Non-draining call** — `ScriptState::call_global_structured_opts`
  (`host_call_global_structured_opts`): a call that leaves the microtasks it
  queued for the host's next `run_microtasks`, for a host with its own
  checkpoints. zipp-rb's `VM#invoke`.
- **Local time zone** — `Vm::local_tz` and vm/local_time.rs
  (`set_local_time_zone`, `local_offset_ms`, `local_to_utc_ms`, over
  temporal/tzdb.rs), `ScriptState::set_time_zone`. `Date`'s local getters,
  setters, `new Date(y, m, …)`, zoneless `Date.parse` forms
  (vm/helpers_num2.rs `parse_date` takes the conversion) and `toString` forms
  (vm/indexing_date.rs, coerce.rs) work in it, and Intl's DefaultTimeZone
  (vm/intl/mod.rs) is it. Unset, local time is UTC, as before. Test:
  `test/time_zone_test.rb`.
- **Console levels** — `ConsoleLevel`, `ScriptState::set_console_level_sink`
  (vm/mod.rs `push_console_line_at`, vm/natives.rs `CONSOLE_*`, vm/engine/boot.rs):
  a sink told which `console` method produced a line, since `warn` and
  `error` share a stream. zipp-rb's `on_log` severities come from it.
- **In-memory modules** — `HostModuleProvider`,
  `ScriptState::set_module_provider` (vm/engine/modules.rs): modules under a
  virtual root served from host memory, sharing the confined loader's budgets.

## Error stacks (crates/zipp-vm/src)

`Error.prototype.stack` was always empty. Now it is V8's format
(`Name: message` then `    at f (script:line:col)`):

- **Positions** — `Stmt::Expr` / `Return` / `Throw` carry their start offset
  and `VarDecl` a `pos` (parse/ast.rs, parse/stmt.rs, parse/funcs.rs); the
  compiler records `FuncProto::positions` (`SourcePos { ip, line, col }`) in
  `FnCompiler::note_stmt_pos` (compile/decls.rs), with the function's own
  start at ip 0 (`Compiler::default_position`, compile/funcs.rs) and
  `Compiler::line_col` (compile/compiler.rs). Matches on those variants in
  capture.rs, parse/limits.rs, compile/decls.rs and vm/engine/run.rs gained
  the extra field. bytecode_codec.rs encodes both new FuncProto fields.
- **Script names** — `FuncProto::script`, stamped in `prepare_eval_program`
  (vm/engine/eval_prog.rs) from `Vm::script_name`
  (`ScriptState::set_script_name`, `HostCtx::set_script_name`) or, for a
  module, `module_name_for_stack` (vm/engine/modules.rs).
- **Capture** — `Vm::interp_pc` (written per instruction in
  vm/dispatch.rs; measured at no cost) gives the innermost frame's pc;
  `capture_error_frames` (vm/error_stack.rs) runs wherever an object gains
  [[ErrorData]] (vm/values.rs, async_runtime.rs, construct/inherit.rs,
  construct/modules_dispose.rs) into `Vm::error_frames`, pruned by both GC
  paths (vm/gc.rs). The `stack` getter (vm/natives.rs) formats it.
- **Host** — a JsError's `stack` is the formatted stack (vm/host_api.rs;
  `read_data_prop` had returned the inherited accessor's getter), and
  `ScriptState::load_in_context` reports a throw structured. The
  `FuncProto` size pin is 312 (bytecode.rs).

## Resource limits (crates/zipp-vm/src/vm/instrument.rs, embed.rs)

- **Run deadline** — `Recorder::deadline`, `ScriptState::set_deadline`. Every
  abort-flag poll goes through `Recorder::abort_requested()` (flag OR passed
  deadline); the interpreter fast path defers to the slow path when a deadline
  is set. zipp-rb's `timeout_msec` uses this instead of a timer thread.

## Performance (crates/zipp-vm/src/parse/parser.rs, src/compile/*)

- **Scope lookups indexed** — parser `Scope` gains `lex_all` / `lex_nonfn` /
  `var_all` sets; the compiler keeps `scope_index` (name → register per scope)
  beside `scopes`, used by `resolve`, `resolve_existing`, declaration checks,
  `with_obj_regs` and `bound_anywhere`. Fixes O(n²) compile time in the number
  of locals of one function (6400 `var`s: 173 ms → 13 ms). Behaviour-neutral;
  if upstream rewrites scope handling, re-measure before re-porting.

## Correctness (crates/zipp-vm/src)

- **A top-level `const` destructuring initializes its globals** —
  `var_decl` (compile/decls.rs) pre-initialized a script's destructured
  leaves to undefined (strict stores need the TDZ over), which made the
  extraction a const's second write under script GDI (`const {a} = o` threw
  "Assignment to constant variable"). A global const's leaves are now
  extracted into locals and each slot written once. A const reassignment's
  TypeError also carries V8's message (compile/bindings.rs). Test:
  `test/engine_fixes_test.rb`.
- **A typed array's indices are own properties** — `has_own_property`
  (vm/construct/iterate.rs) had no TypedArray arm, so `u8.hasOwnProperty(0)`
  was false. Test: `test/engine_fixes_test.rb`.
- **A Proxy is constructible only when its target is** — the Proxy
  `[[Construct]]` path (vm/construct/construct.rs) ran the trap for any
  target, and `is_constructor` (vm/coerce.rs) counted an arrow as one. Test:
  `test/engine_fixes_test.rb`.
- **A host runs classic scripts, not evals** — `Vm::host_run_script`
  (vm/engine/eval_prog.rs, with `compile_script_with_completion` /
  `Compiler::script_completion` in compile/) runs a script with script
  GlobalDeclarationInstantiation (the `$262.evalScript` path) and its
  completion value. `ScriptState::load_in_context`,
  `eval_in_context_structured` (`host_eval_structured`) and
  `HostCtx::eval_script` use it (vm/host_api.rs, embed.rs): as indirect
  evals, a strict script's `var`s and functions stayed inside it, and no
  script's `let`/`const`/`class` reached later ones (WPT's `common.js`
  files are strict). Test: `test/engine_fixes_test.rb`.
- **`obj.hasOwnProperty(k)` on a Proxy runs its trap** — the builtin-method
  lane (vm/builtins.rs) used the trap-less `has_own_property`; it now uses
  `has_own_property_dyn`, as the native does (testharness.js's
  `assert_array_equals` reads a NodeList this way).
- **A function's `prototype` can be made non-writable** — `fn_proto_readonly`
  (vm/mod.rs; vm/props/define.rs, descriptors.rs, vm/access.rs): a
  `defineProperty(fn, "prototype", {writable: false})` was accepted and
  dropped, so WebIDL's `Image` reported a writable prototype.
- **A class or function in a [[Prototype]] chain is searched** —
  `proto_member_get` (vm/props/member.rs) skipped class statics and a
  callable's own properties when one was spliced into a chain
  (`Reflect.setPrototypeOf(fn, SomeClass)`, Stimulus's `bless`), so
  `fn.staticOfTheClass` read undefined.
- **A builtin replaced on the global object is the binding later scripts
  see** — `eval_global_slot` seeded a later script's fresh slot for a builtin
  name (`setTimeout`, `Map`, …) from the boot intrinsic, ignoring an own data
  property an earlier script had set (`globalThis.setTimeout = f` before any
  code named it bare). It now seeds from that property. dommy installs its
  timer globals this way. Test: `test/engine_fixes_test.rb`.
- **`typeof` of a global agrees with a read** — `LoadGlobalOrUndefined` /
  `LoadGlobalOrUndefinedDyn` (vm/dispatch.rs) returned a live slot even when
  the global object shadowed it with a real own property, which `LoadGlobal`
  reads instead; and `eval_global_slot` (vm/engine/run.rs) re-seeded a builtin
  deleted from the global object before any code named it. Test:
  `test/engine_fixes_test.rb`.
- **Re-entry does not drain microtasks** — `HostCtx::eval_script`,
  `call_global_value` and `call_handle_value` drained the job queue on return,
  running promise jobs while the guest's stack (the host call) was still
  live; dommy's custom-element reactions then ran before the element's
  upgrade. They now leave the queue to the outermost entry
  (`host_eval_rich_opts`). Test: `test/zipp_rb_test.rb`
  `test_reentry_does_not_run_microtasks_early`.

- **`typeof name` inside `with`** — `typeof_operand` (src/compile/exprs.rs)
  loaded a global-resolving name straight from the global, skipping active
  `with` objects, so `with (o) typeof o.member` read "undefined". It now
  probes them first (`emit_with_get_chain`). Test: `test/engine_fixes_test.rb`.
- **`performance` is a real global** — `performance.now()` was a compile-time
  lowering to `Instr::Now` with no `performance` object behind it, so a host's
  replacement (dommy's virtual-time `performance`) was never called. Now
  `performance` is a builtin object (vm/setup.rs) whose `now` is the
  `PERFORMANCE_NOW` native (vm/native.rs, vm/natives.rs), and the lowering is
  gone (src/compile/calls.rs). Test: `test/engine_fixes_test.rb`.
- **A replaced `console` method is called** — the compiler lowered
  `console.log/info/warn/error/debug(…)` to the `Print` instruction whenever
  `console` was not shadowed, so `console.warn = wrapper` was never called
  (test frameworks and pages wrap console methods). The lowering is removed;
  the calls go through the `CONSOLE_*` natives. Test:
  `test/zipp_rb_test.rb` `test_a_replaced_console_method_is_called`.
- **Promise resolving functions inherit `Function.prototype`** — the
  `BoundResolver` / `CombinatorResolver` heap kinds (a `new Promise`
  executor's resolve/reject, `Promise.all`'s element functions) were missing
  from `get_member_slow`'s callable arm, so `resolve.apply` / `.call` /
  `.bind` read `undefined`. dommy's bridge settles host promises with
  `resolve.apply(…)`. Test: `test/engine_fixes_test.rb`.

## Tests

`cargo test -p zipp-vm --lib --features instrument` (`rake engine:test` in
gems/zipp-rb). Local additions include `deadline_stops_execution_like_abort`
and `host_ctx_reaches_handles_of_its_own_vm_only`.

`codegen/plan_region.rs` `local_sroa_tests` (x86-64 only): the fixture's
post-loop `console.log(checksum)` became `const result = checksum;`. With the
`Print` lowering removed (see "A replaced `console` method is called") the
call compiles to `LoadGlobal` + `CallMethod`, which local SROA rejects
anywhere in the function, so the fixture no longer exercised the planner.
