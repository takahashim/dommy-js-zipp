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

## Differences from dommy-js-quickjs

- **No `Error.stack`.** Zipp leaves it empty, so an error carries no JS frames
  and `ErrorEvent` has no file or position. The five skipped tests are these.
- **No bytecode cache.** `load_script_cached` and the bridge's runtime bundles
  are parsed on every Runtime (Zipp parses the bundles in tens of ms), and
  there is no `register_module` / `preload_modules:`.
- **Out of memory is not fatal.** Zipp raises a catchable `RangeError` at the
  heap ceiling and the VM stays usable; a VM is only "poisoned" once disposed
  or discarded after an engine panic.
- **Errors** are `ZippRb::Error` and its subclasses (`ZippRb::TypeError`, …,
  `ZippRb::InterruptedError` at the deadline).

`DOMMY_JS_TIMEOUT_MSEC` and `DOMMY_JS_MEMORY_LIMIT_MB` work as in
dommy-js-quickjs.

## Tests

```sh
bundle exec rake test   # from this directory; compile zipp-rb first
```

The suite is ported from dommy-js-quickjs; the vendored framework bundles live
in `test/fixtures`. The WPT, Capybara and Rack suites are not ported yet.
