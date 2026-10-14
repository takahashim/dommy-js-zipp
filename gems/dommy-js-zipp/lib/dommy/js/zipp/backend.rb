# frozen_string_literal: true

require "json"
require "zipp_rb"

module Dommy
  module Js
    module Zipp
      # Binds HostBridge's abstract backend contract (eval / define_host_function
      # / call_js / run_bundle) to a ZippRb::VM, plus what Runtime asks of an
      # engine beyond that.
      #
      # #run_bundle compiles each bundle once per process (zipp-rb's compile
      # cache) and runs that compilation in every VM: parsing and compiling
      # the bridge's runtime is most of a VM's start-up.
      class Backend
        # A console line, shaped like quickjs.rb's log object so a host's
        # `on_log` block works with either backend.
        Log = Struct.new(:severity, :message) do
          def to_s = message.to_s
          def raw = [message]
        end

        # Run on every VM before anything else. `__dommyZippCall` resolves a
        # dotted path (`__rbHost.defineGlobal`) and calls it with its owner as
        # `this` (ZippRb::VM#call takes a global name only); `__dommyZippParses`
        # is the syntax check behind Runtime#evaluate. The engine's own
        # `performance` goes, so the window's is the one the page finds.
        # `__rbEvalP` (Runtime#evaluate's result slot) is the global's from the
        # start, so setting it never asks a window below the global (see
        # Runtime's unified_global) whether the host has the name.
        BOOT_JS = <<~JS
          globalThis.__dommyZippCall = function (path, args) {
            var parts = path.split("."), owner = globalThis;
            for (var i = 0; i < parts.length - 1; i++) owner = owner[parts[i]];
            return owner[parts[parts.length - 1]].apply(owner, args);
          };
          delete globalThis.performance; // real-clock; the window's runs on Dommy's scheduler
          Object.defineProperty(globalThis, "__rbEvalP", { value: undefined, writable: true, enumerable: false, configurable: true });
          globalThis.__dommyZippParses = function (source) {
            try {
              new Function(source);
              return true;
            } catch (error) {
              if (error instanceof SyntaxError) return false;
              throw error;
            }
          };
        JS
        private_constant :BOOT_JS

        def initialize(timeout_msec: Config.timeout_msec, memory_limit: Config.memory_limit,
                       time_zone: Config.time_zone, **vm_opts)
          @vm = ::ZippRb::VM.new(timeout_msec: timeout_msec, memory_limit: memory_limit, **vm_opts)
          # An unnameable host zone leaves the VM in UTC rather than failing to
          # build a page.
          begin
            @vm.time_zone = time_zone
          rescue ArgumentError
            nil
          end
          @vm.load(BOOT_JS, cache: true)
        end

        # True once the VM can no longer run JS: it was discarded after an engine
        # panic, or disposed. (Zipp does not poison a VM on out-of-memory.)
        def poisoned?
          @vm.disposed? || @vm.poisoned?
        end

        # Every entry point that runs JS goes through #guarded, so a dead VM
        # answers nil whichever one a host reached for.
        def eval(js) = guarded { @vm.eval(js) }

        # Run `js` in global scope for its side effects; the completion value is
        # not converted (a UMD bundle's last statement can be a large object).
        # `filename` names it in its Errors' stack frames. With `cache`, the
        # compilation comes from zipp-rb's process-wide compile cache.
        def load(js, filename: nil, cache: false) = guarded { @vm.load(js, filename: filename, cache: cache) }

        # The cache key names the bundle's frames (`host_runtime.js`), which is
        # how a host tells the bridge's own frames from the page's. The compile
        # cache itself is keyed by the source.
        def run_bundle(cache_key, source) = load(source, filename: cache_key.to_s, cache: true)

        # Run `body` as an async function body, awaiting its result for at most
        # `max_ms`.
        def eval_awaited(body, max_ms: 5000) = guarded { @vm.eval_async(body, max_ms: max_ms) }

        # Whether `source` parses as a function body. Nothing runs.
        def parses?(source) = guarded { @vm.call("__dommyZippParses", source) }

        # Install the ESM resolver: `(specifier, importer) -> source | {code:,
        # as:} | {as:} | nil`, consulted for every static and dynamic import.
        # A preloaded module (see #preload_modules) is found by its name before
        # the loader is asked, and a loader's `{as: name}` lands on it.
        def module_loader=(callable)
          preloaded = @preloaded_modules || {}
          @vm.module_loader = lambda do |specifier, importer|
            next {code: preloaded[specifier], as: specifier} if preloaded.key?(specifier)

            answer = callable&.call(specifier, importer)
            name = answer.is_a?(Hash) && !answer.key?(:code) && !answer.key?("code") && (answer[:as] || answer["as"])
            name && preloaded.key?(name.to_s) ? {code: preloaded[name.to_s], as: name.to_s} : answer
          end
        end

        # Make the modules `sources` (name => source) available to this VM's
        # imports by name, without asking the loader (Runtime#initialize).
        def preload_modules(sources)
          @preloaded_modules = sources
          self.module_loader = nil
        end

        # Evaluate the module named `url` (resolved and fetched by the module
        # loader) for its side effects, awaiting a top-level await.
        def import_module_url(url)
          guarded { @vm.eval_async("await import(#{JSON.generate(url.to_s)});") }
        end

        def define_host_function(name, &block)
          @vm.define_function(name, &block)
        end

        # Microtasks the call queues are left for the event loop's checkpoint
        # (rAF callbacks of one frame share one), as with quickjs.rb's call.
        def call_js(path, *args) = guarded { @vm.invoke("__dommyZippCall", path.to_s, args) }

        def drain_microtasks = guarded { @vm.drain_jobs! }

        # The block receives a ZippRb::Error carrying the rejection's message
        # (zipp-rb reports the message only, not the reason itself).
        def on_unhandled_rejection(&block)
          @vm.on_unhandled_rejection { |message| block.call(::ZippRb::Error.new(message.to_s)) }
        end

        # Point the engine's promise-rejection hook at a JS function, named by
        # an expression evaluated once in global scope.
        def promise_rejection_hook=(expression)
          @vm.promise_rejection_hook = expression
        end

        # The block receives a Log (#severity / #to_s / #raw).
        def on_log(&block)
          @vm.on_log { |severity, message| block.call(Log.new(severity, message)) }
        end

        def run_gc = guarded { @vm.gc! }

        def dispose
          @vm.dispose
        end

        private

        def guarded
          return if poisoned?

          yield
        end
      end
    end
  end
end
