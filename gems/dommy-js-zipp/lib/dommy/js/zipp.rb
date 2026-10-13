# frozen_string_literal: true

require "dommy"
require_relative "zipp/version"

module Dommy
  module Js
    module Zipp
      class Error < StandardError; end
    end
  end
end

# The engine-agnostic host layer (HostBridge, ScriptBoot, the Runtime port and
# registry) lives in the `dommy` gem. This gem provides only the Zipp backend
# that plugs in underneath, mirroring dommy-js-quickjs.
require_relative "zipp/config"
require_relative "zipp/backend"
require_relative "zipp/browser_environment"
require_relative "zipp/event_loop"
require_relative "zipp/error_translator"
require_relative "zipp/module_registry"
require_relative "zipp/wasm_bridge"
require_relative "zipp/runtime"

module Dommy
  module Js
    module Zipp
      # Compile the JS every Runtime loads (the bridge's bundles and the
      # browser environment's payloads) into zipp-rb's process-wide compile
      # cache now rather than in the first Runtime. Call it before forking
      # workers (parallel tests, a preforking server) so they share the
      # compilation instead of each compiling its own. Optional: without it
      # the first Runtime fills the cache.
      def self.prewarm
        bridge = HostBridge.constants.grep(/_JS\z/).map { HostBridge.const_get(_1) }.grep(String)
        (bridge + BrowserEnvironment.payloads).each { ::ZippRb.precompile(_1) }
        nil
      end
    end
  end
end

Dommy::Js.register_runtime(:zipp) { |**opts| Dommy::Js::Zipp::Runtime.new(**opts) }
