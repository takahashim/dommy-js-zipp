# frozen_string_literal: true

module Dommy
  module Js
    module Zipp
      # Turns what the ENGINE raised into what the page (or the host) should see.
      #
      # zipp-rb converts a JS throw into a Ruby exception (ZippRb::Error or a
      # subclass named after the JS error) before the host hears about it, which
      # loses the thrown object. What survives is its name and message.
      class ErrorTranslator
        # An engine frame in a converted exception's backtrace (`at f (…)`).
        JS_FRAME = /\A\s*at\s/
        private_constant :JS_FRAME

        def initialize(backend, bridge)
          @backend = backend
          @bridge = bridge
        end

        # Rebuild a script's thrown value as a real Error inside the realm, with
        # the same name and message, so a handler reading `event.error.message`
        # gets what the script threw. Identity (`e.error === thrown`) is lost:
        # the original was freed before we were told about it.
        #
        # Returns nil for anything that did not come from JS (a host bug) and for
        # any failure to rebuild, so the caller falls back to what it caught.
        def rebuild_error(error)
          return nil unless error.is_a?(::ZippRb::Error)

          # Not through an evaluate that drives the event loop: this runs while
          # the page is still booting its scripts, and a due-now timer must not
          # fire before the next `<script>`.
          @bridge.decode(
            @backend.call_js("__rbDommyRebuildError", error.js_name || "Error", error.message.to_s, js_frames(error))
          )
        rescue ::StandardError
          nil
        end

        # The JS half of a converted exception's backtrace, as the page's own
        # `stack` string; host frames are not the page's business.
        def js_frames(error)
          Array(error.backtrace).grep(JS_FRAME).join("\n")
        end

        # Attach the timer's scheduling stack (where the page set the timer up)
        # to the error, so a callback that throws a stackless value is still
        # traceable. A no-op when no origin was captured — which, while Zipp
        # leaves `Error.prototype.stack` empty, is always.
        def with_timer_origin(error, timer)
          frames = timer_origin(timer)
          error.set_backtrace(frames) unless frames.empty?
          error
        rescue ::StandardError
          error
        end

        private

        # The scheduling stack for `timer`, as cleaned frame strings (or []). The
        # origin Error lives JS-side (see js/timer_globals.js); fetching it also
        # clears it.
        def timer_origin(timer)
          return [] unless timer.respond_to?(:id)

          stack = @backend.call_js("__rbFetchTimerOrigin", timer.id).to_s
          stack.split("\n").map(&:strip).reject(&:empty?)
               .reject { |line| line.include?("__rbDefer") }
        rescue ::StandardError
          []
        end
      end
    end
  end
end
