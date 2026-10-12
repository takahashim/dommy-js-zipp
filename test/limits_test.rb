# frozen_string_literal: true

require "test_helper"

# Resource limits wired to the engine's instrument API: a wall-clock timeout, an
# instruction budget, and a heap ceiling. A timed-out VM stays usable; the
# timeout surfaces as InterruptedError (via the engine's host abort).
class LimitsTest < Minitest::Test
  def test_timeout_interrupts_an_infinite_loop
    vm = ZippRb::VM.new(timeout_msec: 100)
    start = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    assert_raises(ZippRb::InterruptedError) { vm.eval("while (true) {}") }
    elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC) - start
    assert_operator elapsed, :<, 5.0, "timeout should fire promptly"
    assert_equal 2, vm.eval("1 + 1"), "the VM stays usable after a timeout"
  ensure
    vm.dispose
  end

  def test_step_limit_interrupts_an_infinite_loop
    vm = ZippRb::VM.new(step_limit: 100_000)
    assert_raises(ZippRb::RangeError) { vm.eval("while (true) {}") }
  ensure
    vm.dispose
  end

  def test_memory_limit_interrupts_runaway_allocation
    vm = ZippRb::VM.new(memory_limit: 4 * 1024 * 1024)
    assert_raises(ZippRb::RangeError) do
      vm.eval("var a = []; while (true) { a.push(new Array(1000).fill(0)); }")
    end
  ensure
    vm.dispose
  end

  def test_no_limits_runs_normally
    vm = ZippRb::VM.new
    assert_equal 42, vm.eval("40 + 2")
  ensure
    vm.dispose
  end
end
