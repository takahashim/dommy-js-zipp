# dommy-js-zipp

A monorepo for running JavaScript in Ruby on the Zipp engine (a from-scratch
JavaScript engine in Rust):

- `gems/zipp-rb` — the Ruby binding (`ZippRb::VM`): a Magnus/rb-sys native
  extension over the in-tree engine.
- `gems/dommy-js-zipp` — (planned) a [Dommy](https://github.com/takahashim/dommy)
  JavaScript backend on zipp-rb, the counterpart of dommy-js-quickjs.

## Layout

```
Gemfile, Rakefile         # root: path gems; `rake` fans out to every gem
bin/engine-import         # import an upstream Zipp revision (see below)
gems/zipp-rb/
  engine/                 # Zipp engine sources, in-tree (edit them directly)
    crates/               # upstream crates + local changes
    UPSTREAM              # upstream repository, commit and imported crates
    LOCAL_CHANGES.md      # what differs from upstream, and why
    Cargo.toml            # workspace root for the vendored crates (ours)
  ext/zipp_rb/            # the Rust extension (path-depends on engine/)
    src/lib.rs            # `init`: Ruby classes and the `_xxx` entry points
    src/registry.rs       # VM registry, `Entry` (every host entry), lifecycle
    src/nogvl.rs          # GVL release, interrupt delivery
    src/errors.rs         # JS errors -> Ruby; callback exceptions -> re-raise
    src/convert.rs        # HostValue <-> Ruby values
    src/evaluate.rs       # eval / load / call / gc / memory_usage
    src/callbacks.rs      # define_function, listeners, same-VM re-entry
    src/event_loop.rs     # timers, eval_async, import, drain_jobs
    src/handles.rs        # durable JS handles held by Ruby
    src/limits.rs         # step / heap / deadline limits, interruption
    src/modules.rs        # in-memory ES module provider
  lib/zipp_rb.rb          # ZippRb module, errors, constants
  lib/zipp_rb/{vm,handle}.rb  # ZippRb::VM, ZippRb::Handle
  test/                   # minitest
  benchmarks/             # Ruby benchmarks + compile_bench (Rust)
notes/                    # local design notes; git-ignored, not shipped
```

## Setup and commands

Ruby is pinned by `mise.toml`; Rust is the stable toolchain (rustup).

```sh
bundle install
bundle exec rake                 # every gem: compile + test
bundle exec rake engine:test     # the engine's Rust unit tests

cd gems/zipp-rb
bundle exec rake compile         # build the extension into lib/zipp_rb/
bundle exec rake test
bundle exec ruby -Ilib -Itest test/limits_test.rb -n test_timeout_interrupts_an_infinite_loop
```

The first compile builds the engine in release mode (about a minute); later
ones are incremental. `rake test` does not rebuild: run `rake compile` after a
Rust change.

A JS infinite loop on a VM without `timeout_msec` cannot be interrupted (not by
SIGINT/SIGTERM either), so wrap ad-hoc experiments in `timeout -k 5 60 …`.

## The engine

`gems/zipp-rb/engine/crates` is ordinary tracked source. Change it like any
other code, and add the change to `LOCAL_CHANGES.md` (what it is for, which
files). The exact local diff is `git diff vendor/zipp-upstream HEAD -- gems/zipp-rb/engine/crates`.

Upstream updates are rare but can be large. They arrive through the branch
`vendor/zipp-upstream`, which holds pristine imports only — never commit to it
by hand:

```sh
bin/engine-import [REF]        # default origin/main; commits REF on vendor/zipp-upstream
git merge vendor/zipp-upstream # resolve conflicts against LOCAL_CHANGES.md
bundle exec rake engine:test test
```

Resolve a conflict by keeping the behaviour LOCAL_CHANGES.md describes; if
upstream gained an equivalent API, prefer upstream's and adapt
`ext/zipp_rb`. `bin/engine-import` also reports when an imported crate needs a
crate that is not vendored, or when upstream's `[workspace.package]` /
`[workspace.dependencies]` changed (mirror those in `engine/Cargo.toml`).

## Extension invariants (ext/zipp_rb)

- `ScriptState` is `!Send`: a VM lives in a thread-local registry keyed by id
  and is usable only from the thread that created it.
- Every host entry goes through `Entry` (take the VM out, put it back on every
  path, discard it on an engine panic) and then `finish` (re-raise what a Ruby
  callback raised).
- Code run with the GVL released must not touch the Ruby API. Only VMs with no
  Ruby bridge (no callbacks, listeners, module provider or timers) release it.
- Ruby is only ever called under `rb_protect` (Magnus does this); never let a
  Ruby exception longjmp through engine frames.
- A Ruby value kept across calls must be rooted in a Ruby-side constant
  (`ZippRb::CALLBACKS`, `MODULES`, `LISTENERS`, `PENDING`), not held in Rust.
