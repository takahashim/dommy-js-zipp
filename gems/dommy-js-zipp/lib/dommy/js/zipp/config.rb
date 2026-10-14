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

        # The IANA time zone `Date` works in: DOMMY_JS_TIME_ZONE when set (`UTC`
        # makes a page's dates independent of the host), else the host's own,
        # as a browser's is. nil (UTC) when the host's cannot be named.
        def time_zone
          env = ENV["DOMMY_JS_TIME_ZONE"].to_s
          env.empty? ? host_time_zone : env
        end

        # The host's zone: TZ when it names one (`Asia/Tokyo`, `:Asia/Tokyo`),
        # else what /etc/localtime links to, else a whole-hour fixed offset
        # (`Etc/GMT-9` for +09:00; the Etc sign is inverted).
        def host_time_zone
          tz = ENV["TZ"].to_s.delete_prefix(":")
          return tz if tz.include?("/") || tz == "UTC"

          link = ::File.readlink("/etc/localtime") if ::File.symlink?("/etc/localtime")
          zone = link&.split("zoneinfo/")&.last
          return zone if zone && !zone.empty? && zone != link

          offset = Time.now.utc_offset
          return nil if offset.zero? || offset % 3600 != 0

          hours = offset / 3600
          "Etc/GMT#{hours.positive? ? "-" : "+"}#{hours.abs}"
        rescue SystemCallError
          nil
        end

        # The VM's heap ceiling, in bytes (set in MB). Zipp raises a catchable
        # RangeError at the ceiling and the VM stays usable.
        def memory_limit
          mb = ENV["DOMMY_JS_MEMORY_LIMIT_MB"].to_i
          mb.positive? ? mb * 1024 * 1024 : DEFAULT_MEMORY_LIMIT
        end

        # Whether the engine's global object is the window (`window ===
        # globalThis`, as in a browser), as against a separate window proxy
        # whose namespace is kept in step with the global's (dommy-js-quickjs's
        # model). On unless DOMMY_JS_UNIFIED_GLOBAL is 0, and only against a
        # dommy whose bridge can (HostBridge#unify_global_window).
        def unified_global
          ENV["DOMMY_JS_UNIFIED_GLOBAL"] != "0"
        end
      end
    end
  end
end
