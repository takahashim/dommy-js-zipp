# frozen_string_literal: true

require "test_helper"

# The local time zone Date works in (VM#time_zone=). UTC by default.
class TimeZoneTest < Minitest::Test
  def test_utc_by_default
    vm = ZippRb::VM.new
    assert_equal [0, "Thu Jan 01 2026 12:00:00 GMT+0000"],
      vm.eval("var d = new Date(2026, 0, 1, 12); [d.getTimezoneOffset(), d.toString()]")
  ensure
    vm&.dispose
  end

  def test_local_getters_setters_and_strings_follow_the_zone
    vm = ZippRb::VM.new(time_zone: "Asia/Tokyo")
    assert_equal [-540, 12, 3, "2026-01-01T03:00:00.000Z", "Thu Jan 01 2026 12:00:00 GMT+0900"],
      vm.eval("var d = new Date(2026, 0, 1, 12); [d.getTimezoneOffset(), d.getHours(), d.getUTCHours(), d.toISOString(), d.toString()]")
    assert_equal "1969-12-31T20:00:00.000Z", vm.eval("var e = new Date(0); e.setHours(5); e.toISOString()")
    assert_equal "Asia/Tokyo", vm.eval("Intl.DateTimeFormat().resolvedOptions().timeZone")
  ensure
    vm&.dispose
  end

  # An ISO date-time or a legacy string without an offset is local time; an
  # ISO date is UTC; an explicit offset wins.
  def test_parsing_reads_zoneless_forms_as_local
    vm = ZippRb::VM.new(time_zone: "Asia/Tokyo")
    assert_equal %w[2026-01-01T03:00:00.000Z 2026-01-01T00:00:00.000Z 2026-01-01T03:00:00.000Z 2026-01-01T12:00:00.000Z],
      vm.eval(%w[2026-01-01T12:00:00 2026-01-01 Thu\ Jan\ 01\ 2026\ 12:00:00 Thu\ Jan\ 01\ 2026\ 12:00:00\ GMT+0000]
                .map { |s| "new Date(#{s.dump}).toISOString()" }.join(", ").then { |js| "[#{js}]" })
  ensure
    vm&.dispose
  end

  # Daylight saving time: the offset changes with the date; a wall clock in
  # the spring-forward gap moves forward, one in the fall-back repeat is the
  # earlier instant.
  def test_daylight_saving_time
    vm = ZippRb::VM.new(time_zone: "America/New_York")
    assert_equal [240, 300, 3, "2026-11-01T05:30:00.000Z"],
      vm.eval("[new Date(2026, 6, 1).getTimezoneOffset(), new Date(2026, 0, 1).getTimezoneOffset(), new Date(2026, 2, 8, 2, 30).getHours(), new Date(2026, 10, 1, 1, 30).toISOString()]")
  ensure
    vm&.dispose
  end

  def test_an_unknown_zone_raises
    assert_raises(ArgumentError) { ZippRb::VM.new(time_zone: "Nowhere/City") }
  end
end
