# dommy-js-zipp

Run JavaScript against a [Dommy](https://github.com/takahashim/dommy) DOM on
the Zipp engine, through [zipp-rb](../zipp-rb). It is the Zipp counterpart of
[dommy-js-quickjs](https://github.com/takahashim/dommy-js-quickjs): the same
engine-agnostic bridge from the `dommy` gem (HostBridge, ScriptBoot) on a
different engine, with the same Runtime API.

```ruby
require "dommy"
require "dommy/js/zipp"

win = Dommy.parse("<h1 class='title'>Hi</h1>")

rt = Dommy::Js::Zipp::Runtime.new
rt.define_host_object("document", win.document)
rt.install_window(win)

rt.evaluate('document.querySelector(".title").textContent')        # => "Hi"
rt.execute('document.querySelector(".title").textContent = "Bye"') # mutates the DOM
win.document.query_selector(".title").text_content                 # => "Bye"
```

Requiring the gem registers the backend as `:zipp` (`Dommy::Js.register_runtime`),
so `Dommy::Browser` can run on it too.

The real Turbo, Stimulus, React, Vue, Alpine, htmx, Lit and Solid bundles run
unmodified (`test/dommy/js/test_*_integration.rb`), as do classic and module
scripts, import maps and dynamic `import()`.

## Integrations

- `require "dommy/js/zipp/rack"` — `Dommy::Rack::Session.new(app, javascript: true)`
  on Zipp.
- `require "dommy/js/zipp/capybara"` — script results as Capybara nodes, scripts
  run in the driver's current frame, and `evaluate_async_script`, for a
  `Capybara::Dommy::Driver` built with `javascript: true`.
- `Runtime.register_module(name, source:)` / `Runtime.new(preload_modules:)` —
  modules kept per process and found by name before the module loader is
  asked (dommy's `ModulePreload` uses them).
- `Runtime#wasm_bridge` — handle-oriented JS access for a wasm guest.

## Differences from dommy-js-quickjs

- **The window is the global object.** `window === globalThis`, and `self`,
  `top`, `parent`, `frames`, `document.defaultView` and a frame's `parent`
  are all `globalThis`; a sloppy function's `this` is the window, and the
  window's operations and attributes are own properties of the global, as in
  a browser. It needs a dommy whose bridge has `HostBridge#unify_global_window`
  (dommy 0.17 and later; with dommy 0.16 the window is a separate proxy, as
  in dommy-js-quickjs);
  `DOMMY_JS_UNIFIED_GLOBAL=0` or `Runtime.new(unified_global: false)` keeps
  them apart. Known differences: the global's own keys include the bridge's
  `__rb*` names, and `Object.preventExtensions(window)` succeeds.

- **Stack positions are per statement.** An Error's `stack` names the
  statement each frame was executing (and the start of an expression-bodied
  arrow), not the exact expression; names come from declarations, so an
  arrow assigned to a variable reports as anonymous.
- **The compile cache is per process and in memory.** The bridge's runtime
  bundles and `load_script_cached` scripts are compiled once per process and
  shared by every Runtime (zipp-rb's `VM#load(cache: true)`); nothing is
  written to disk. `Dommy::Js::Zipp.prewarm` compiles the bundles up front,
  before forking workers, say. A preloaded module saves the fetch, not the
  parse.
- **Out of memory is not fatal.** Zipp raises a catchable `RangeError` at the
  heap ceiling and the VM stays usable; a VM is only "poisoned" once disposed
  or discarded after an engine panic.
- **Intl is native**, so pages get a browser's formatting (`"a, b, and c"`).
- **Errors** are `ZippRb::Error` and its subclasses (`ZippRb::TypeError`, …,
  `ZippRb::InterruptedError` at the deadline).

`DOMMY_JS_TIMEOUT_MSEC` and `DOMMY_JS_MEMORY_LIMIT_MB` work as in
dommy-js-quickjs. A page's `Date` works in the host's time zone, as a
browser's does; `DOMMY_JS_TIME_ZONE` names another (`UTC` keeps results
independent of the host).

## Tests

```sh
bundle exec rake test              # from this directory; compile zipp-rb first
bundle exec rake test:all          # plus the heavy WPT files
bundle exec rake wpt:conformance   # the vendored WPT corpus, as a pass rate
bundle exec rake turbo:conformance stimulus:conformance
bundle exec rake capybara:compliance
```

The suite is ported from dommy-js-quickjs; the vendored framework bundles and
the WPT corpus live in `test/fixtures` (`script/` rebuilds them). Capybara's
compliance suite still fails where capybara-dommy lacks a feature (geometry,
drag and drop), a page needs a server (puma) or loads a library from a CDN;
Stimulus's three ApplicationStartTests need per-iframe realms.

## License

MIT (`LICENSE.txt`). The Zipp engine it runs on comes with zipp-rb, under
its own licenses (see zipp-rb's `NOTICE`).
