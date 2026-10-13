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

  # Ruby exceptions raised by callbacks during the current entry into a VM,
  # kept (and GC-rooted) until the entry returns so an uncaught one can be
  # re-raised as itself. Internal.
  PENDING = {}

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

  # The process-wide compile cache behind VM#load's +cache: true+ (native):
  #
  #   ZippRb.precompile(src)     # compile +src+ into the cache without a VM;
  #                              # false when it does not compile. Call it
  #                              # before forking so workers share the result.
  #   ZippRb.compile_cache_stats # {entries:, bytes:, hits:, misses:}
  #
  # Entries are keyed by source and bounded by total source bytes.

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
end

require_relative "zipp_rb/handle"
require_relative "zipp_rb/module_paths"
require_relative "zipp_rb/vm"
