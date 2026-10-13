# frozen_string_literal: true

module Dommy
  module Js
    module Zipp
      # ES modules registered once per process under their canonical names
      # (Runtime.register_module), for a Runtime to preload
      # (Runtime.new(preload_modules:)). Zipp has no bytecode to share, so what
      # a registration saves is fetching the source again; each Runtime still
      # evaluates its own instance.
      module ModuleRegistry
        @sources = {}
        @mutex = Mutex.new

        class << self
          # `source` is a String, or a Proc returning one, read on the first
          # preload and kept from then on.
          def register(name, source:)
            @mutex.synchronize { @sources[name.to_s] = source }
            nil
          end

          def unregister(name)
            @mutex.synchronize { @sources.delete(name.to_s) }
            nil
          end

          def registered?(name) = @mutex.synchronize { @sources.key?(name.to_s) }

          # The sources of `names`, by name. A name not registered raises
          # ArgumentError, as dommy's ModulePreload expects of an engine.
          def sources_for(names)
            names.to_h do |name|
              name = name.to_s
              source = @mutex.synchronize do
                raise ArgumentError, "no module registered as #{name}" unless @sources.key?(name)

                value = @sources[name]
                value = @sources[name] = value.call.to_s if value.respond_to?(:call)
                value
              end
              [name, source]
            end
          end
        end
      end
    end
  end
end
