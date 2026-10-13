# frozen_string_literal: true

module ZippRb
  # A persistent Zipp JavaScript VM.
  #
  # The engine state lives in a thread-local registry inside the native
  # extension (the VM is not `Send`); this object holds its id and the thread
  # that created it, and delegates. A VM may only be used from its owning
  # thread; another thread raises a ThreadError rather than reaching an
  # unrelated VM.
  class VM
    # Finalizer body: drop the native VM and the bindings retained for it.
    # Captures only the id, never the VM (a finalizer that captured `self`
    # would keep it alive forever).
    def self.finalizer(id)
      proc { ZippRb._drop(id) }
    end

    # Create a VM. Optional resource limits:
    #   timeout_msec:  wall-clock deadline for each evaluation (ms)
    #   memory_limit:  heap ceiling in bytes
    #   step_limit:    instruction budget per evaluation
    #   features:      [:feature_timeout] enables setTimeout/setInterval
    #   time_zone:     the IANA zone `Date` works in ("Asia/Tokyo"; see
    #                  #time_zone=). UTC when omitted.
    def initialize(timeout_msec: nil, memory_limit: nil, step_limit: nil, features: [], time_zone: nil)
      @id = ZippRb._create
      @thread = Thread.current
      @disposed = false
      ObjectSpace.define_finalizer(self, self.class.finalizer(@id))
      if timeout_msec || memory_limit || step_limit
        ZippRb._configure_limits(@id, step_limit, memory_limit, timeout_msec)
      end
      ZippRb._enable_timers(@id) if features.include?(FEATURE_TIMEOUT)
      self.time_zone = time_zone if time_zone
    end

    # Set the local time zone `Date` (and Intl's default time zone) works in to
    # the IANA zone +zone+ (`"Asia/Tokyo"`), nil for UTC — the default, which
    # keeps a script's results independent of the host. Daylight saving time
    # follows the tz database. An unknown zone raises ArgumentError.
    def time_zone=(zone)
      check_usable!
      ZippRb._set_time_zone(@id, zone&.to_s)
    end

    # Evaluate +src+ in the VM's global context and return the completion value.
    # +filename+ names the script in its Errors' stack frames (`at f
    # (filename:3:7)`); unnamed code reports `<anonymous>`.
    def eval(src, filename: nil)
      check_usable!
      with_script_name(filename) { ZippRb._eval(@id, src) }
    end

    # Evaluate +src+ for its side effects, discarding the completion value.
    # Prefer this to #eval when a script's last expression is a large object
    # (loading a bundle): the completion value is not marshalled into Ruby.
    def load(src, filename: nil)
      check_usable!
      with_script_name(filename) { ZippRb._load(@id, src) }
      nil
    end

    # Await-capable eval (quickjs.rb's default shape): +src+ runs as an async
    # function body, so it may use top-level +await+; the event loop drains
    # microtasks and fires timers until the top-level promise settles. Requires
    # +features: [FEATURE_TIMEOUT]+ for setTimeout.
    def eval_async(src, max_ms: 5000)
      check_usable!
      ZippRb._eval_async(@id, src, max_ms)
    end

    # Call the JS global function +name+ with +args+, converting both ways.
    def call(name, *args)
      check_usable!
      ZippRb._call(@id, name.to_s, args, true)
    end

    # Like #call, but leaves the microtasks the call queued for the next
    # #drain_jobs! — for a host that runs its own microtask checkpoints (as
    # quickjs.rb's `call` does). Inside a callback #call behaves this way too.
    def invoke(name, *args)
      check_usable!
      ZippRb._call(@id, name.to_s, args, false)
    end

    # Take a durable handle to the global +name+ (a function or any value), so
    # Ruby can hold it and use it later. Returns nil if the name is unbound.
    def handle(name)
      check_usable!
      hid = ZippRb._handle(@id, name.to_s)
      hid && Handle.new(@id, hid)
    end

    # Register +block+ as a JS function. +name+ is a String/Symbol (a global
    # name) or an Array of segments (a nested path, `["a", "b"]` → `a.b`).
    # Arguments and the return value cross between Ruby and JS (numbers,
    # strings, booleans, nil, Array and Hash). Returns a Symbol, or an Array of
    # Symbols for a path (quickjs.rb's shape).
    # A trailing `:async` flag makes the JS function return a Promise (resolved
    # with the block's value, rejected when it raises) — quickjs.rb's shape.
    def define_function(name, *flags, &block)
      check_usable!
      raise ArgumentError, "block required" unless block

      result = ZippRb._define_function(@id, name, block, flags.include?(:async))
      name.is_a?(Array) ? name.map(&:to_sym) : result
    end

    # Resource-usage counters (heap bytes, instruction steps, retained dynamic
    # code) as a Hash — the closest analogue of quickjs.rb's memory_usage.
    def memory_usage
      check_usable!
      ZippRb._memory_usage(@id)
    end

    # Collect garbage now. Returns nil.
    def gc!
      check_usable!
      ZippRb._gc(@id)
      nil
    end

    # Whether the VM was discarded after an internal engine panic; every later
    # use raises. (Zipp does not poison a VM on out-of-memory: a bounded VM
    # raises a catchable error and stays usable.)
    def poisoned?
      ZippRb._poisoned?(@id)
    end

    # See #poisoned? — Zipp has no separate memory-poison latch.
    def memory_poisoned?
      false
    end

    # Register in-memory ES module sources. +sources+ maps a resolved module
    # path (String) to its source: a String, or a Proc called with the path
    # (for lazy loading). Dynamic `import()` of a path in +sources+ resolves
    # with no file on disk; a path not present is "module not found".
    #
    #   vm.set_module_provider("/app", { "/app/mod.mjs" => "export const x = 1;" })
    #   vm.eval("import('/app/mod.mjs').then(ns => ...)")
    def set_module_provider(root, sources, max_module_bytes: 64 * 1024 * 1024)
      check_usable!
      ZippRb._set_module_provider(@id, root.to_s, sources, max_module_bytes)
      nil
    end

    # Resolve and load ES modules by name (quickjs.rb's module loader). The
    # +loader+ is called as `loader.call(specifier, importer)` for every
    # static and dynamic import — +importer+ is the importing module's name,
    # nil for code outside a module — and answers one of:
    #
    #   "source"                 the module named +specifier+
    #   { code: "…", as: name }  the module, under the canonical name +name+
    #   { as: name }             a module already loaded under +name+
    #   nil                      not found
    #
    # A module is loaded once per canonical name, and its relative imports and
    # `import.meta.url` are its name. Names need not be paths (URLs and bare
    # names work). Replaces #set_module_provider.
    #
    #   vm.module_loader = ->(spec, importer) {
    #     { code: SOURCES.fetch(spec), as: spec } if SOURCES.key?(spec)
    #   }
    #   vm.import("https://example.test/app.js")
    def module_loader=(loader)
      check_usable!
      paths = ModulePaths.new
      set_module_provider(ModulePaths::ROOT, paths.sources)
      ZippRb._set_module_resolver(@id, ->(dir, specifier) { paths.resolve(loader, dir, specifier) })
      ZippRb._set_module_meta_url(@id, ->(path) { paths.name_of(path) })
    end

    # Evaluate the ES module +specifier+ (via dynamic import) and return its
    # namespace exports as a Hash. Requires a module loader (#module_loader=
    # or #set_module_provider).
    def import(specifier)
      check_usable!
      ZippRb._import(@id, specifier.to_s)
    end

    # Register +block+ to receive every console line as `(severity, message)`.
    # Severities follow quickjs.rb: :info (console.log / info), :verbose
    # (debug), :warning (warn) and :error (error). Replaces any previous
    # listener.
    def on_log(&block)
      check_usable!
      ZippRb._on_log(@id, block)
      nil
    end

    # Register +block+ to receive the message of every unhandled Promise
    # rejection. Replaces any previous listener.
    def on_unhandled_rejection(&block)
      check_usable!
      ZippRb._on_unhandled_rejection(@id, block)
      nil
    end

    # Point the promise rejection hook at the JS function +expression+
    # evaluates to (once, in global scope; quickjs.rb's shape). At each
    # microtask checkpoint it is called as `hook("unhandledrejection", promise,
    # reason)` for a rejection no handler caught, and later as
    # `hook("rejectionhandled", promise, reason)` if one is attached — the
    # promise and the reason themselves, unlike #on_unhandled_rejection.
    def promise_rejection_hook=(expression)
      check_usable!
      ZippRb._set_promise_rejection_hook(@id, expression.to_s)
    end

    # Run queued microtasks (promise reactions, queueMicrotask). Returns nil.
    def drain_jobs!
      check_usable!
      ZippRb._drain_jobs(@id)
      nil
    end

    # Tear the VM down. Idempotent. Raises ThreadError if called from inside a
    # callback while the VM is executing (the native layer refuses it), so the
    # VM is never torn down under a running evaluation.
    def dispose
      check_owner!
      return nil if @disposed

      ZippRb._dispose(@id)
      @disposed = true
      nil
    end

    def disposed?
      @disposed || ZippRb._disposed?(@id)
    end

    private

    # Run the block with the code it evaluates named +filename+, restoring the
    # previous name afterwards (an evaluation may nest inside a callback).
    def with_script_name(filename)
      return yield if filename.nil?

      previous = @script_name
      ZippRb._set_script_name(@id, filename.to_s)
      @script_name = filename.to_s
      begin
        yield
      ensure
        ZippRb._set_script_name(@id, previous) unless disposed?
        @script_name = previous
      end
    end

    def check_owner!
      return if Thread.current.equal?(@thread)

      raise ThreadError, "ZippRb::VM can only be used from the thread that created it"
    end

    # Usable from this thread and not yet disposed. Raises ZippRb::Error on a
    # disposed VM (quickjs.rb raises on disposal too).
    def check_usable!
      check_owner!
      raise ZippRb::Error, "the VM has been disposed" if @disposed
    end
  end
end
