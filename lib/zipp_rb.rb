# frozen_string_literal: true

require_relative "zipp_rb/version"
require_relative "zipp_rb/zipp_rb"

module ZippRb
  # Ruby procs registered as JS functions, keyed by an internal kind string.
  # Kept as a Ruby constant so the Ruby GC roots every proc for the lifetime of
  # the process; the native handler looks them up here on each call.
  CALLBACKS = {}

  # In-memory module sources per VM, keyed by an internal provider id. Also a
  # constant so the GC roots the hashes (and any Procs inside them).
  MODULES = {}

  # Console / rejection listener blocks per VM, keyed by an internal string.
  # A constant so the GC roots the procs.
  LISTENERS = {}

  # Reopen the native base error to expose the JS error name (quickjs.rb's
  # `#js_name`).
  class Error
    # The JS error name (e.g. "TypeError"), or nil when the throw was not an
    # Error object. Set by the native layer.
    attr_reader :js_name
  end

  # Values that are not ordinary Ruby values, mirroring Quickjs::Value.
  module Value
    UNDEFINED = :undefined
    NAN = :NaN
  end

  # Feature flag: provide setTimeout/setInterval (mirrors Quickjs::FEATURE_TIMEOUT).
  FEATURE_TIMEOUT = :feature_timeout

  # Evaluate +code+ on a fresh VM and return the completion value, disposing the
  # VM afterwards (quickjs.rb's `Quickjs.eval_code`). +opts+ are the VM options.
  def self.eval_code(code, **opts)
    vm = VM.new(**opts)
    begin
      vm.eval(code)
    ensure
      vm.dispose
    end
  end

  # A durable handle to a JS value (usually a function) the VM keeps alive, so
  # Ruby can hold it and call it later (e.g. an event listener). Returned by
  # VM#handle.
  class Handle
    attr_reader :id

    def initialize(vm_id, id)
      @vm_id = vm_id
      @id = id
    end

    # Call the JS function this handle roots.
    def call(*args)
      ZippRb._call_handle(@vm_id, @id, args)
    end

    # Read the value this handle roots, as a Ruby value.
    def value
      ZippRb._get_handle(@vm_id, @id)
    end

    # Release the handle (drops its root). Returns whether it was live.
    def release
      ZippRb._release_handle(@vm_id, @id)
    end
  end

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
    def initialize(timeout_msec: nil, memory_limit: nil, step_limit: nil, features: [])
      @id = ZippRb._create
      @thread = Thread.current
      @disposed = false
      ObjectSpace.define_finalizer(self, self.class.finalizer(@id))
      if timeout_msec || memory_limit || step_limit
        ZippRb._configure_limits(@id, step_limit, memory_limit, timeout_msec)
      end
      ZippRb._enable_timers(@id) if features.include?(FEATURE_TIMEOUT)
    end

    # Evaluate +src+ in the VM's global context and return the completion value.
    def eval(src)
      check_usable!
      ZippRb._eval(@id, src)
    end

    # Evaluate +src+ for its side effects, discarding the completion value.
    # Prefer this to #eval when a script's last expression is a large object
    # (loading a bundle): the completion value is not marshalled into Ruby.
    def load(src)
      check_usable!
      ZippRb._load(@id, src)
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
      ZippRb._call(@id, name.to_s, args)
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
    def define_function(name, &block)
      check_usable!
      raise ArgumentError, "block required" unless block

      result = ZippRb._define_function(@id, name, block)
      name.is_a?(Array) ? name.map(&:to_sym) : result
    end

    # Resource-usage counters (heap bytes, instruction steps, retained dynamic
    # code) as a Hash — the closest analogue of quickjs.rb's memory_usage.
    def memory_usage
      check_usable!
      ZippRb._memory_usage(@id)
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

    # Evaluate the ES module +specifier+ (via dynamic import) and return its
    # namespace exports as a Hash. Requires a module loader, usually
    # #set_module_provider.
    def import(specifier)
      check_usable!
      ZippRb._import(@id, specifier.to_s)
    end

    # Register +block+ to receive every console line as `(severity, message)`,
    # where severity is :log or :error. Replaces any previous listener.
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
