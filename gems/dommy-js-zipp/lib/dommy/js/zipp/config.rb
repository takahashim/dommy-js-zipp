# frozen_string_literal: true

module Dommy
  module Js
    module Zipp
      # Every environment variable this backend reads. The names are shared with
      # dommy-js-quickjs, so a host tuning one backend tunes the other. Read at
      # the point of use: a test changes them between runtimes.
      module Config
        # zipp-rb has no default deadline. A browser-grade VM gets a generous one
        # so a runaway script is interrupted instead of hanging the process (an
        # unbounded Zipp VM cannot be interrupted at all).
        DEFAULT_TIMEOUT_MSEC = 60_000

        DEFAULT_MEMORY_LIMIT = 512 * 1024 * 1024

        module_function

        # The per-evaluation deadline, in ms: each script, and each timer/rAF
        # callback, is interrupted (ZippRb::InterruptedError) after this long.
        def timeout_msec
          env = ENV["DOMMY_JS_TIMEOUT_MSEC"].to_i
          env.positive? ? env : DEFAULT_TIMEOUT_MSEC
        end

        # The VM's heap ceiling, in bytes (set in MB). Zipp raises a catchable
        # RangeError at the ceiling and the VM stays usable.
        def memory_limit
          mb = ENV["DOMMY_JS_MEMORY_LIMIT_MB"].to_i
          mb.positive? ? mb * 1024 * 1024 : DEFAULT_MEMORY_LIMIT
        end
      end
    end
  end
end
