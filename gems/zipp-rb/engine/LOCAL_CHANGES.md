# Local changes to the Zipp engine

`crates/` is upstream Zipp (revision in `UPSTREAM`) plus the changes below.
The branch `vendor/zipp-upstream` holds the pristine imports, so the exact
local diff is always:

    git diff vendor/zipp-upstream HEAD -- gems/zipp-rb/engine/crates

Keep this list current when you change the engine: it is what a merge of a new
upstream import is resolved against (`bin/engine-import`). Each entry says what
the change is for, so a conflict can be settled by keeping the behaviour, not
the exact lines — upstream may have grown its own equivalent, in which case
prefer upstream's and adapt `ext/zipp_rb`. Everything here except the compiler
fix is additive embed API, and all of it is a candidate to propose upstream.

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
- **In-memory modules** — `HostModuleProvider`,
  `ScriptState::set_module_provider` (vm/engine/modules.rs): modules under a
  virtual root served from host memory, sharing the confined loader's budgets.

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

## Tests

`cargo test -p zipp-vm --lib --features instrument` (`rake engine:test` in
gems/zipp-rb). Local additions include `deadline_stops_execution_like_abort`
and `host_ctx_reaches_handles_of_its_own_vm_only`.
