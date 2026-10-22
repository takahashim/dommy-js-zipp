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

## Embed API (crates/zipp-vm/src/embed.rs, vm/host_api.rs, vm/natives.rs, vm/mod.rs, vm/engine/boot.rs, vm/engine/eval_prog.rs)

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
- **Scripts for their side effects** — `ScriptState::load_in_context`:
  upstream's `run_script` (`Vm::eval_script`) without marshalling the
  completion value, reporting a throw as a structured `JsError`. zipp-rb's
  `VM#load`; `HostCtx::eval_script` runs the same path.
- **Structured calls and errors** — `call_global_structured`,
  `eval_in_context_structured`, `JsError { name, message, stack }`; global
  name resolution unified on `host_resolve_global_by_name` so a function
  installed as `globalThis[name]` is found.
- **Lifecycle** — `ScriptState::dispose` / `disposed` (idempotent teardown),
  `gc_now` (`Vm::force_gc`, a no-op while a host conversion holds the GC lock).
- **Module resolver and module names** — `ModuleResolver`,
  `join_module_specifier` (vm/mod.rs), `ScriptState::set_module_resolver` /
  `set_module_meta_url`; every `dir.join(spec)` in vm/engine/modules.rs,
  vm/dispatch_cold.rs (dynamic import) and vm/construct/modules_dispose.rs goes
  through `join_module_specifier`, and upstream's `Vm::module_url` (what
  `import.meta.url` and a module's stack frames report) answers the host's
  name for a module when `set_module_meta_url` gives one, before the file URL
  of its path. Lets zipp-rb's `VM#module_loader=` load modules named by URL
  or bare name, and name them so. Test: `test/module_loader_test.rb`.
- **Non-draining call** — `ScriptState::call_global_structured_opts`
  (`host_call_global_structured_opts`): a call that leaves the microtasks it
  queued for the host's next `run_microtasks`, for a host with its own
  checkpoints. zipp-rb's `VM#invoke`.
- **Compiled scripts** — `CompiledScript::compile` (embed.rs, over
  vm/engine/eval_prog.rs `compile_host_script`) and
  `ScriptState::load_compiled` (`Vm::host_run_compiled`, the compiled half
  of upstream's `Vm::eval_script`): a classic script compiled once, needing no VM, and
  loaded into any number of VMs on any thread. Each load installs its own copy
  (`prepare_eval_program` remaps slots and ids on it). zipp-rb's compile cache
  (`VM#load(cache: true)`). Test: `a_compiled_script_loads_into_many_vms_independently`.
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
  `error` share a stream. zipp-rb's `on_log` severities come from it. The
  `Print` lowering of `console.*(…)` prints at its method's level too
  (vm/helpers_misc.rs `console_print_level`, in place of upstream's
  stream-only `console_print_stream`; vm/dispatch_cold.rs `Instr::Print`).
- **In-memory modules** — `HostModuleProvider`,
  `ScriptState::set_module_provider` (vm/engine/modules.rs): modules under a
  virtual root served from host memory, sharing the confined loader's budgets.

## Script names in stacks (crates/zipp-vm/src/vm/error_stack.rs, embed.rs, vm/engine/eval_prog.rs, bytecode.rs)

Upstream's `Error.prototype.stack` names the main program's script
(`set_script_name`) and a module's URL, and every later script `<anonymous>`.
A host that loads many scripts into one VM wants each named:

- **`ScriptState::name_scripts(Option<&str>)`** (and `HostCtx::set_script_name`)
  sets `Vm::script_name` for the code installed from then on;
  `prepare_eval_program` stamps it on each function as `FuncProto::script`
  (encoded by bytecode_codec.rs; `FuncProto` is 376 bytes, upstream's 360
  plus the field), and `stack_script_name` uses a function's stamp before
  falling back to upstream's rule. zipp-rb sets it around each
  `load(src, filename:)`. Proposed upstream as D46 in the notes. Test:
  `test/error_stack_test.rb`.

## Resource limits (crates/zipp-vm/src/vm/instrument.rs, embed.rs)

- **Run deadline** — `Recorder::deadline`, `ScriptState::set_deadline`. Every
  abort-flag poll goes through `Recorder::abort_requested()` (flag OR passed
  deadline), and `Recorder::schedule` ends the step countdown on the
  `ABORT_CHECK_MASK` stride while a deadline is set, as upstream does for an
  abort flag (f2i-com/zipp.org#62); `set_deadline` settles the countdown so
  that stride applies at once. zipp-rb's `timeout_msec` uses this instead of
  a timer thread. Test: `deadline_stops_execution_like_abort`.
## Memory (crates/zipp-vm/src/heap.rs, capture/value.rs, lang/js/compile/*, vm/dispatch.rs)

- **Unwritten captured bindings are captured by value** — a captured
  binding took a heap cell (a whole slot) even when nothing ever wrote it,
  and dommy's per-node proxy handler closes over dozens of `const`s.
  `capture::value_captures` (capture/value.rs) picks the captured
  `let`/`const`s and simple parameters that are declared once, never
  assigned anywhere (nested functions included), initialized before any
  closure over them is made, outside `switch` cases, in a function with no
  `eval`/`with` (and, for a parameter, no `arguments`). Those stay plain
  registers (`FnCompiler::value_captured`); a nested function captures them
  through `UpvalSource::ParentValue`, for which `Heap::capture_value` hands
  the closure the value's own slot (with its cell-value mirror entry set to
  itself, so every cell read — interpreter and JIT — yields the value) or,
  for a primitive, an immutable cell shared per value (`shared_cells`,
  dropped when the cell is freed). `pin_mirror_dict` no longer clears the
  cell-value mirror. A function making such closures is not JIT-eligible
  (`FuncProto::makes_value_captures`, vm/engine/boot.rs): the native tiers
  see a capture source only through its `MakeCell`. gethtml.html's heap:
  548MB → 326MB. Beside upstream's late boxing (`capture::late_box_names`):
  the by-value bindings are taken out of `captured` first, and late boxing
  applies to the rest; `EnclosingFn` carries `value_locals` (indexed like
  upstream's shared `cell_locals`) and both ride `snapshot`'s cache. Tests: vm/gc.rs `value_capture_tests`, heap.rs,
  `test/engine_fixes_test.rb`.

## Correctness (crates/zipp-vm/src)

- **Re-entry does not drain microtasks** — `HostCtx::eval_script`,
  `call_global_value` and `call_handle_value` drained the job queue on return,
  running promise jobs while the guest's stack (the host call) was still
  live; dommy's custom-element reactions then ran before the element's
  upgrade. They now leave the queue to the outermost entry
  (`host_eval_rich_opts`). Test: `test/zipp_rb_test.rb`
  `test_reentry_does_not_run_microtasks_early`.


## Tests

`cargo test -p zipp-vm --lib --features instrument` (`rake engine:test` in
gems/zipp-rb). Local additions include `deadline_stops_execution_like_abort`
and `host_ctx_reaches_handles_of_its_own_vm_only`. codegen_aarch64.rs's test
`proto` gains the `call_sites` field upstream's FuncProto has (the aarch64 JIT
tests do not compile upstream, whose CI is x86_64).
