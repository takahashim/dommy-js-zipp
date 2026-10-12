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
    def initialize(timeout_msec: nil, memory_limit: nil, step_limit: nil)
      @id = ZippRb._create
      @thread = Thread.current
      @disposed = false
      ObjectSpace.define_finalizer(self, self.class.finalizer(@id))
      if timeout_msec || memory_limit || step_limit
        ZippRb._configure_limits(@id, step_limit, memory_limit, timeout_msec)
      end
    end

    # Evaluate +src+ in the VM's global context and return the completion value.
    def eval(src)
      check_owner!
      ZippRb._eval(@id, src)
    end

    # Call the JS global function +name+ with +args+, converting both ways.
    def call(name, *args)
      check_owner!
      ZippRb._call(@id, name.to_s, args)
    end

    # Take a durable handle to the global +name+ (a function or any value), so
    # Ruby can hold it and use it later. Returns nil if the name is unbound.
    def handle(name)
      check_owner!
      hid = ZippRb._handle(@id, name.to_s)
      hid && Handle.new(@id, hid)
    end

    # Register +block+ as a JS global function named +name+. Arguments and the
    # return value cross between Ruby and JS (numbers, strings, booleans, nil,
    # Array and Hash).
    def define_function(name, &block)
      check_owner!
      raise ArgumentError, "block required" unless block

      ZippRb._define_function(@id, name.to_s, block)
    end

    # Register in-memory ES module sources. +sources+ maps a resolved module
    # path (String) to its source: a String, or a Proc called with the path
    # (for lazy loading). Dynamic `import()` of a path in +sources+ resolves
    # with no file on disk; a path not present is "module not found".
    #
    #   vm.set_module_provider("/app", { "/app/mod.mjs" => "export const x = 1;" })
    #   vm.eval("import('/app/mod.mjs').then(ns => ...)")
    def set_module_provider(root, sources, max_module_bytes: 64 * 1024 * 1024)
      check_owner!
      ZippRb._set_module_provider(@id, root.to_s, sources, max_module_bytes)
      nil
    end

    # Evaluate the ES module +specifier+ (via dynamic import) and return its
    # namespace exports as a Hash. Requires a module loader, usually
    # #set_module_provider.
    def import(specifier)
      check_owner!
      ZippRb._import(@id, specifier.to_s)
    end

    # Register +block+ to receive every console line as `(severity, message)`,
    # where severity is :log or :error. Replaces any previous listener.
    def on_log(&block)
      check_owner!
      ZippRb._on_log(@id, block)
      nil
    end

    # Register +block+ to receive the message of every unhandled Promise
    # rejection. Replaces any previous listener.
    def on_unhandled_rejection(&block)
      check_owner!
      ZippRb._on_unhandled_rejection(@id, block)
      nil
    end

    # Run queued microtasks (promise reactions, queueMicrotask). Returns nil.
    def drain_jobs!
      check_owner!
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
  end
end
