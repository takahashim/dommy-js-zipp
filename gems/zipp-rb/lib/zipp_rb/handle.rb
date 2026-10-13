# frozen_string_literal: true

module ZippRb
  # A durable handle to a JS value (usually a function) the VM keeps alive, so
  # Ruby can hold it and call it later (e.g. an event listener). Returned by
  # VM#handle.
  #
  # A handle that is garbage-collected without #release has its engine root
  # released on the owning thread's next use of the VM.
  class Handle
    attr_reader :id

    # Finalizer body; captures only ids, never the handle.
    def self.finalizer(vm_id, id)
      proc { ZippRb._queue_release(vm_id, id) }
    end

    def initialize(vm_id, id)
      @vm_id = vm_id
      @id = id
      ObjectSpace.define_finalizer(self, self.class.finalizer(vm_id, id))
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
      released = ZippRb._release_handle(@vm_id, @id)
      ObjectSpace.undefine_finalizer(self)
      released
    end
  end
end
