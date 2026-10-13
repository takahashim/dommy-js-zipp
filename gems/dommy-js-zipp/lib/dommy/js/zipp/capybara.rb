# frozen_string_literal: true

require "capybara/dommy"
require "dommy/rack"
require_relative "../zipp"
# A `javascript: true` session needs the runtime factory, which dommy-rack
# would otherwise look for in dommy-js-quickjs.
require_relative "rack"

module Dommy
  module Js
    module Zipp
      # evaluate_async_script's callback was not called within the wait budget.
      class ScriptTimeoutError < Error; end

      # Capybara integration. capybara-dommy's driver runs page JavaScript
      # itself when built with `javascript: true` (through the session's own
      # SessionRuntime, which this gem's rack.rb supplies); this adds what the
      # driver leaves to a JS backend: script results as Capybara nodes,
      # scripts run in the realm of the driver's current frame, and
      # evaluate_async_script.
      #
      # It runs every script in the session's realm for that document, never a
      # second runtime of its own: a second one would boot the page's scripts
      # in a realm the page never sees (and execute_script would not find the
      # page's globals).
      module CapybaraDriver
        # The in-realm slot an async script's callback reports to. Each call
        # installs a fresh state object, so a late callback from an earlier call
        # writes into its own, now-orphaned one.
        ASYNC_SCRIPT_STATE = "__rbAsyncScriptState"

        def execute_script(script, *args)
          return super unless javascript?

          runtime = dommy_js_runtime
          if args.empty?
            runtime.execute(script)
          else
            runtime.execute_with_args(script, capybara_script_args(args))
          end
          rack_session.check_js_errors!
          nil
        end

        def evaluate_script(script, *args)
          return super unless javascript?

          runtime = dommy_js_runtime
          value = args.empty? ? runtime.evaluate(script) : runtime.evaluate_with_args(script, capybara_script_args(args))
          rack_session.check_js_errors!
          decode_for_capybara(value)
        end

        # WebDriver's executeAsyncScript: the script gets a callback as its last
        # argument, and its result is whatever that callback is called with.
        # Virtual time is pumped until it is, for at most
        # Capybara.default_max_wait_time of VIRTUAL time — so a timer the script
        # waits on fires, while the budget stays deterministic.
        def evaluate_async_script(script, *args)
          return super unless javascript?

          dommy_js_runtime.execute_with_args(<<~JS, capybara_script_args(args))
            var state = { settled: false, value: undefined };
            globalThis.#{ASYNC_SCRIPT_STATE} = state;
            var args = Array.prototype.slice.call(arguments);
            args.push(function (value) {
              if (state.settled) return;
              state.settled = true;
              state.value = value;
            });
            (function () {
            #{script}
            }).apply(this, args);
          JS
          await_async_script
          decode_for_capybara(dommy_js_runtime.evaluate("globalThis.#{ASYNC_SCRIPT_STATE}.value"))
        end

        private

        def await_async_script
          budget_ms = ::Capybara.default_max_wait_time * 1000
          slice = ::Dommy::Rack::SessionRuntime::PUMP_SLICE_MS
          settled = "(globalThis.#{ASYNC_SCRIPT_STATE} || {}).settled === true"
          (budget_ms / slice).ceil.times do
            return if dommy_js_runtime.evaluate(settled)

            rack_session.advance_time(slice)
          end
          return if dommy_js_runtime.evaluate(settled)

          raise ScriptTimeoutError,
                "evaluate_async_script did not call its callback within #{budget_ms.round}ms of virtual time"
        end

        # A Capybara node argument becomes the Dommy element it wraps (so it
        # crosses to JS as a proxy), at any depth inside arrays and hashes.
        # Capybara::Session unwraps only top-level elements, so a nested
        # Capybara::Node::Element is unwrapped here too.
        def capybara_script_args(args)
          args.map { |arg| capybara_script_arg(arg) }
        end

        def capybara_script_arg(arg)
          case arg
          when ::Capybara::Node::Element then capybara_script_arg(arg.base)
          when ::Capybara::Dommy::Node then arg.native
          when Array then capybara_script_args(arg)
          when Hash then arg.transform_values { |value| capybara_script_arg(value) }
          else arg
          end
        end

        # The realm of the driver's current document (its frame-aware
        # `document`), from the session's own SessionRuntime.
        def dommy_js_runtime
          rack_session.__internal_js_runtime.runtime_for(document)
        end

        # Map an evaluate() result to what Capybara expects:
        #   - Array            -> recurse (so element collections wrap per item)
        #   - JS undefined      -> nil
        #   - Dommy::Element    -> Capybara::Dommy::Node (covers HTML/SVG subclasses)
        #   - other host object -> nil (Document/Text/Comment/Fragment/NodeList/
        #                          Window/Blob/FormData have no Capybara
        #                          representation)
        #   - primitive/Hash    -> as-is
        def decode_for_capybara(value)
          return nil if js_undefined?(value)

          case value
          when Array then value.map { |element| decode_for_capybara(element) }
          when ::Dommy::Element then ::Capybara::Dommy::Node.new(self, value)
          else host_object?(value) ? nil : value
          end
        end

        # JS `undefined` arrives as either the engine-neutral sentinel the bridge
        # decodes to, or — on the paths that skip that decode — the gem's own.
        def js_undefined?(value)
          value.equal?(::Dommy::Bridge::UNDEFINED) || value.equal?(::ZippRb::Value::UNDEFINED)
        end

        # Whether `value` is a Dommy object the page can read properties off
        # (`__js_get__` is what the bridge calls to do that). Everything that
        # answers to it is a DOM-side object, and the ones Capybara can represent
        # were already taken by the branches above — so what is left has no
        # representation and becomes nil rather than leaking a host object into a
        # Capybara assertion.
        def host_object?(value)
          value.respond_to?(:__js_get__)
        end
      end

      # Prepend the adapter onto Capybara::Dommy::Driver. JavaScript stays
      # opt-in, as capybara-dommy has it: a driver built with `javascript:
      # true` (or under `Capybara::Dommy.configuration.javascript = true`)
      # runs it. Requiring this file calls it; it is idempotent.
      def self.install_capybara!
        return if ::Capybara::Dommy::Driver.ancestors.include?(CapybaraDriver)

        ::Capybara::Dommy::Driver.prepend(CapybaraDriver)
      end
    end
  end
end

Dommy::Js::Zipp.install_capybara!
