# frozen_string_literal: true

require "test_helper"

# A page's Date works in the host's time zone, as a browser's does, unless
# DOMMY_JS_TIME_ZONE names another (UTC keeps results host-independent).
class Dommy::Js::TestTimeZone < Minitest::Test
  def with_env(value)
    original = ENV["DOMMY_JS_TIME_ZONE"]
    ENV["DOMMY_JS_TIME_ZONE"] = value
    yield
  ensure
    ENV["DOMMY_JS_TIME_ZONE"] = original
  end

  def offset_in(zone)
    with_env(zone) do
      rt = Dommy::Js::Zipp::Runtime.new
      rt.evaluate("new Date(2026, 0, 1, 12).getTimezoneOffset()")
    ensure
      rt&.dispose
    end
  end

  def test_the_env_names_the_zone
    assert_equal(-540, offset_in("Asia/Tokyo"))
    assert_equal 300, offset_in("America/New_York")
    assert_equal 0, offset_in("UTC")
  end

  def test_an_unknown_zone_falls_back_to_utc
    assert_equal 0, offset_in("Nowhere/City")
  end

  def test_without_the_env_the_host_zone_is_used
    with_env(nil) do
      assert_equal Dommy::Js::Zipp::Config.host_time_zone, Dommy::Js::Zipp::Config.time_zone
    end
  end
end
