# zipp-rb

A Ruby binding for the [Zipp](https://github.com/f2i-com/zipp.org) JavaScript
engine. The engine is built from source with the gem (Rust stable required).
The API follows quickjs.rb where the two overlap.

```ruby
require "zipp_rb"

vm = ZippRb::VM.new(timeout_msec: 1_000)
vm.eval("[1, 2.5, 'x', { a: null }]")   # => [1, 2.5, "x", { "a" => nil }]

vm.define_function("add") { |a, b| a + b }
vm.eval("add(2, 3)")                     # => 5

vm.eval("function greet(name) { return `hi ${name}` }")
vm.call("greet", "zipp")                 # => "hi zipp"

vm.dispose
```

- `undefined` and `NaN` read as `:undefined` / `:NaN`; functions and other
  non-plain objects read as `nil` (hold them with `VM#handle`).
- A JS error raises `ZippRb::Error` or a subclass named after it
  (`ZippRb::TypeError`, …) with `#js_name`. A Ruby exception raised in a
  callback and not caught by JS is re-raised as itself.
- `timeout_msec:`, `memory_limit:` and `step_limit:` bound each evaluation
  (`ZippRb::InterruptedError` / `ZippRb::RangeError`). A bounded VM can also
  be interrupted from Ruby (`Thread#raise`, `Timeout`, signals); an unbounded
  one cannot, so give untrusted or long-running code a `timeout_msec`.
- `time_zone:` (or `VM#time_zone=`) names the IANA zone `Date` works in;
  UTC when omitted.
- A Ruby string that is not valid UTF-8 crosses with each invalid sequence
  replaced by U+FFFD.
- A VM belongs to the thread that created it. A VM without Ruby callbacks
  releases the GVL while it runs.
- The engine is built without `unsafe` code (its `safe-sandbox` profile, no
  JIT): an engine bug is a panic, after which that VM is discarded
  (`VM#poisoned?`), not memory corruption. A `SharedArrayBuffer` is local to
  its VM.

`VM#eval` and `#load` run their source as a classic script, as a browser
runs `<script>`: top-level declarations (strict code's included, and
`let`/`const`/`class`) are global and visible to later evaluations. They take
`filename:`, which an Error's `stack` (and the
backtrace of a `ZippRb::Error`) reports as the script of its frames.

`VM#load(src, cache: true)` compiles `src` once per process and runs that
compilation in every VM that loads the same source, on any thread; each VM
still gets globals of its own. Use it for scripts every VM loads (a bridge
runtime, a framework bundle). `ZippRb.precompile(src)` fills the cache without
a VM (before forking workers, so they share it); `ZippRb.compile_cache_stats`
reports it. The cache is keyed by source and bounded by total source bytes.

Other API: `VM#load` (run for side effects), `#eval_async` (top-level
`await`; with `features: [ZippRb::FEATURE_TIMEOUT]` for timers), `#invoke`
(`#call` that leaves microtasks for `#drain_jobs!`), `#handle`,
`#module_loader=` (quickjs.rb's `(specifier, importer)` loader; modules may be
named by URL) or `#set_module_provider`, and `#import`, `#on_log`
(quickjs.rb's severities), `#on_unhandled_rejection`,
`#promise_rejection_hook=`, `#drain_jobs!`, `#gc!`, `#memory_usage`.
