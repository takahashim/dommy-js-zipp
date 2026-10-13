# frozen_string_literal: true

require "json"

module Dommy
  module Js
    module Zipp
      # Public entry point: a JS runtime on Zipp that can drive a Dommy DOM.
      #
      #   rt = Dommy::Js::Zipp::Runtime.new
      #   rt.define_host_object("document", win.document)
      #   rt.evaluate('document.querySelector("h1").textContent')  #=> "..."
      #
      # Implements the Dommy::Js::Runtime port by wiring the same collaborators
      # dommy-js-quickjs does:
      #
      #   Backend             zipp-rb — eval, call, the VM's state
      #   HostBridge          the engine-agnostic JS<->Ruby DOM bridge (core)
      #   BrowserEnvironment  the globals and polyfills a page expects to find
      #   EventLoop           microtasks + Dommy's scheduler, as one loop
      #   ErrorTranslator     an engine exception, as what the page should see
      class Runtime
        # How long the final await of #evaluate waits for a promise that the
        # event loop left pending. Nothing inside the engine can settle it at
        # that point (timers ride Dommy's scheduler), so this only bounds how
        # long a never-settling result takes to raise ZippRb::NoAwaitError.
        AWAIT_MAX_MS = 100

        # Register an ES module process-wide under its canonical name (the one
        # its relative imports resolve against, a page module's resolved URL),
        # for a later Runtime to preload. `source:` is a String or a Proc
        # returning one, read when a Runtime first preloads it.
        def self.register_module(name, source:)
          ModuleRegistry.register(name, source: source)
        end

        # `preload_modules:` names registered modules (see .register_module)
        # this Runtime finds by name before asking its module loader; a loader
        # answer of `{as: name}` lands on one too. A name not registered raises
        # ArgumentError. Other options go to the VM (timeout_msec:,
        # memory_limit:, step_limit:).
        def initialize(preload_modules: [], **vm_opts)
          preloaded = ModuleRegistry.sources_for(preload_modules)
          @backend = Backend.new(**vm_opts)
          @backend.preload_modules(preloaded) unless preloaded.empty?
          @bridge = Dommy::Js::HostBridge.new(@backend)
          @environment = BrowserEnvironment.new(@backend)
          @errors = ErrorTranslator.new(@backend, @bridge)
          @environment.install_error_rebuilder
          @loop = EventLoop.new(@backend, scheduler: -> { @window&.scheduler }) do |error|
            @callback_error_listener&.call(error)
          end
          @callback_error_listener = nil
        end

        def define_host_object(name, obj)
          @bridge.define_host_object(name, obj)
        end

        # Inject the Dommy window: seed it as the JS global, give the bridge the
        # realm it belongs to, and alias the bare timer globals onto it (so
        # `setTimeout(fn, ms)` runs through Dommy's deterministic scheduler —
        # drive it with `win.scheduler.advance_time(ms)`).
        def install_window(win)
          @window = win
          define_host_object("window", win)
          @bridge.window = win
          install_promise_rejection_hook
          wire_scheduler(win)
          @environment.install_timers
          win
        end

        # Expose the seeded interface constructors on a secondary window (an
        # iframe's contentWindow). Call after install_window.
        def expose_constructors_on(window_obj)
          @bridge.expose_constructors_on(window_obj)
        end

        # Wire the bare browser globals frameworks reach for (self / location /
        # history / navigator / storages / CSS / fetch / addEventListener / …),
        # aliased onto the installed window. Call after install_window.
        def install_browser_globals
          @environment.install_globals
          self
        end

        # WPT-only scaffolding (see dommy-js-quickjs): the WebAssembly stub's
        # Memory yields the SharedArrayBuffer constructor common/sab.js wants.
        def install_wasm_memory_shim
          @environment.install_wasm_stub
          self
        end

        # --- Running scripts ---

        # Run a script for side effects. Wrapped in a function so statements
        # are allowed and its declarations stay local. Drains microtasks.
        def execute(js)
          in_page_turn { @backend.load("(function () {\n#{js}\n})();") }
        end

        # Load a script the way a browser <script> does: in global scope, so
        # its top-level declarations become globals (a UMD bundle's `var Vue =
        # …`). The completion value is discarded. Drains microtasks.
        def load_script(js)
          in_page_turn { @backend.load(js) }
        end

        # An external script, compiled once per process for every Runtime that
        # loads the same source (zipp-rb's compile cache). `cache_key` is its
        # URL, which its Errors' stack frames report.
        def load_script_cached(js, cache_key: nil)
          in_page_turn { @backend.load(js, filename: cache_key, cache: true) }
        end

        # Install the ESM module resolver: a callable `(specifier, importer) ->
        # source | {code:, as:} | {as:} | nil`; `{as: name}` with no code
        # lands on a module already loaded under that name.
        def module_loader=(callable)
          @backend.module_loader = callable
        end

        # Evaluate an external module by URL (the loader fetches it); its
        # relative imports resolve against that URL. Drains microtasks.
        def load_module_url(url)
          in_page_turn { @backend.import_module_url(url) }
        end

        # Evaluate JS and return its value, with DOM nodes decoded to Dommy
        # objects. Accepts an expression (`document.title`) or a statement body
        # that uses `return`, run as an async function body; which one is
        # decided by parsing, not by running. The result is awaited.
        def evaluate(js)
          bump_dom_epoch
          evaluate_settled(expression?(js) ? parenthesized(js) : "(async () => {\n#{js}\n})()")
        end

        # Run a script with Ruby arguments as its `arguments`; DOM objects cross
        # as JS proxies.
        def execute_with_args(js, args)
          wire = encode_args(args)
          in_page_turn { @backend.load("(function () {\n#{with_arguments_js(js, wire)}\n})();") }
        end

        # Args-aware #evaluate.
        def evaluate_with_args(js, args)
          bump_dom_epoch
          body = expression?(js) ? "return #{parenthesized(js)};" : js
          evaluate_settled("(async function () {\n#{with_arguments_js(body, encode_args(args))}\n})()")
        end

        # Set `document.readyState` and fire the milestone events, then drain
        # microtasks so the listeners settle.
        def set_document_ready_state(state)
          @window&.document&.__internal_set_ready_state__(state)
          drain_microtasks
          self
        end

        # --- Driving the event loop (see EventLoop for what each one means) ---

        def drain_microtasks = @loop.drain_microtasks

        def settle(max_iterations: EventLoop::DEFAULT_MAX_ITERATIONS)
          @loop.settle(max_iterations: max_iterations)
          self
        end

        def run_until_idle(max_iterations: EventLoop::DEFAULT_MAX_ITERATIONS)
          @loop.run_until_idle(max_iterations: max_iterations)
          self
        end

        # --- Errors ---

        def rebuild_error(error) = @errors.rebuild_error(error)

        # Observe promise rejections no handler caught. The block receives a
        # ZippRb::Error carrying the rejection's message. Goes quiet once the
        # JS hook is installed (#install_window): both would report the same
        # rejection, and the hook carries the promise and the reason.
        def on_unhandled_rejection(&block)
          @backend.on_unhandled_rejection do |error|
            block.call(error) unless js_rejection_hook?
          end
          self
        end

        # Whether rejections reach the page through the JS hook. A host that
        # also relays #on_unhandled_rejection would otherwise report each twice.
        def js_rejection_hook? = !!@js_rejection_hook

        # Observe a timer/rAF callback that threw or was interrupted by the
        # deadline. Recorded by the host, not fatal.
        def on_callback_error(&block)
          @callback_error_listener = block
          self
        end

        # Observe console.* output (`log.severity` / `log.to_s`).
        def on_log(&block)
          @backend.on_log(&block)
          self
        end

        # --- Introspection and teardown ---

        # Handle-oriented JS access for a wasm guest (see WasmBridge). Memoized
        # so the guest's `__rbWasmInvoke` dispatcher stays registered for the
        # VM's lifetime.
        def wasm_bridge
          @wasm_bridge ||= WasmBridge.new(@backend)
        end

        # Run JS GC then drain, so FinalizationRegistry callbacks release the
        # handles of proxies no longer referenced.
        def collect_garbage
          @backend.run_gc
          @backend.drain_microtasks
        end

        def registered_count = @bridge.registered_count

        def bridge_crossing_counts(limit: nil) = @bridge.crossing_counts(limit: limit)

        def reset_bridge_crossing_counts
          @bridge.reset_crossing_counts
          self
        end

        def dispose = @backend.dispose

        private

        # A timer/rAF callback that throws (or runs past the deadline) surfaces
        # as a ZippRb::Error out of the scheduler; route it to the scheduler's
        # error hook so it is recorded and the timer dropped. The microtask
        # checkpoint runs after EACH task, as the event loop model requires.
        def wire_scheduler(win)
          return unless win.respond_to?(:scheduler) && win.scheduler

          win.scheduler.timer_error_handler = method(:handle_timer_error)
          win.scheduler.microtask_checkpoint = method(:drain_microtasks)
        end

        # Hand rejections to the bridge's JS hook, which reports them at the
        # window as `unhandledrejection` / `rejectionhandled` events carrying
        # the promise and the reason the page rejected with.
        def install_promise_rejection_hook
          @backend.promise_rejection_hook = "__rbHost.onPromiseRejection"
          @js_rejection_hook = true
        rescue ::StandardError
          @js_rejection_hook = false
        end

        # Scheduler hook: a timer/rAF callback raised. A JS error is recorded
        # and swallowed (truthy: the scheduler drops the timer and goes on); any
        # other error is a host bug and propagates.
        def handle_timer_error(error, timer)
          return false unless error.is_a?(::ZippRb::Error)

          @loop.note_halted_if_poisoned(error)
          @callback_error_listener&.call(@errors.with_timer_origin(error, timer))
          true
        end

        # One turn of handing the page something to run: invalidate the
        # bridge's attribute snapshots (Ruby may have mutated the DOM), run,
        # then let queued `.then` work land.
        def in_page_turn
          bump_dom_epoch
          yield
          drain_microtasks
          nil
        end

        def bump_dom_epoch
          @backend.load("globalThis.__rbHost && globalThis.__rbHost.bumpDomEpoch();")
          nil
        end

        # Evaluate `expr` to its awaited value, driving the event loop first so
        # a result that depends on a task (a setTimeout, a fetch delivery)
        # settles; then await the settled promise, which surfaces a rejection
        # as a Ruby raise.
        def evaluate_settled(expr)
          @backend.load("globalThis.__rbEvalP = Promise.resolve(#{expr});")
          @loop.drive_due_now
          @bridge.decode(@backend.eval_awaited("return __rbHost.tag(await globalThis.__rbEvalP);", max_ms: AWAIT_MAX_MS))
        end

        # Whether `js` is a single expression, found by parsing it — nothing
        # runs, so a script's side effects never run twice.
        def expression?(js) = @backend.parses?(parenthesized(js))

        # `js` as a parenthesized expression, a trailing `;` dropped. The
        # newlines keep a trailing line comment from swallowing the `)`.
        def parenthesized(js) = "(\n#{js.strip.sub(/;\s*\z/, "")}\n)"

        def encode_args(args) = JSON.generate(@bridge.encode(Array(args)))

        def with_arguments_js(js, wire_json)
          "var __rbScriptArgs = __rbHost.rehydrateArgs(#{wire_json});\n" \
            "return (function () {\n#{js}\n}).apply(this, __rbScriptArgs);"
        end
      end
    end
  end
end
