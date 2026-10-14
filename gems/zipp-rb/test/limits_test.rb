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

  # A bounded VM is interruptible from Ruby while it runs without the GVL:
  # Ruby's unblocking function raises the engine's abort flag, and the
  # interrupt itself (not an InterruptedError) reaches the caller.
  def test_thread_raise_interrupts_a_bounded_eval
    vm = ZippRb::VM.new(timeout_msec: 60_000)
    main = Thread.current
    raiser = Thread.new do
      sleep 0.1
      main.raise(ArgumentError, "stop")
    end
    started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    err = assert_raises(ArgumentError) { vm.eval("while (true) {}") }
    assert_equal "stop", err.message
    assert_operator Process.clock_gettime(Process::CLOCK_MONOTONIC) - started, :<, 5.0
    raiser.join
    assert_equal 2, vm.eval("1 + 1"), "the VM stays usable after the interrupt"
  ensure
    vm.dispose
  end

  def test_timeout_block_interrupts_a_bounded_eval
    require "timeout"
    vm = ZippRb::VM.new(timeout_msec: 60_000)
    assert_raises(Timeout::Error) { Timeout.timeout(0.1) { vm.eval("while (true) {}") } }
    assert_equal 2, vm.eval("1 + 1")
  ensure
    vm.dispose
  end

  def test_thread_kill_stops_a_bounded_eval
    ran_ensure = false
    thread = Thread.new do
      vm = ZippRb::VM.new(timeout_msec: 60_000)
      begin
        vm.eval("while (true) {}")
      ensure
        ran_ensure = true
      end
    end
    sleep 0.1
    thread.kill
    assert thread.join(5), "the killed thread must finish"
    assert ran_ensure
  end

  # The timeout is an engine deadline, not a timer thread per VM.
  def test_timeouts_start_no_threads
    skip "needs ps -M" unless RUBY_PLATFORM.include?("darwin")
    count = -> { `ps -M #{Process.pid}`.lines.size }
    before = count.call
    vms = Array.new(20) { ZippRb::VM.new(timeout_msec: 1000) }
    vms.each { |vm| vm.eval("1") }
    assert_equal before, count.call
  ensure
    vms&.each(&:dispose)
  end

  def test_no_limits_runs_normally
    vm = ZippRb::VM.new
    assert_equal 42, vm.eval("40 + 2")
  ensure
    vm.dispose
  end

  # Filling the heap is reported as the heap budget even when the first
  # check to see it is a linear regex's (whose storage is capped at the
  # heap's headroom too), not as a regex backtrack-memory runaway.
  def test_a_full_heap_is_reported_as_the_memory_budget_even_from_a_regex
    vm = ZippRb::VM.new(memory_limit: 64 * 1024 * 1024)
    err = assert_raises(ZippRb::RangeError) do
      vm.eval(<<~JS)
        var keep = [];
        for (let i = 0; i < 3e6; i++) {
          keep.push({ a: ["x" + i, "y" + i, "z" + i], n: i });
          if (i % 1000 === 0) /^(0|[1-9][0-9]*)$/.test(String(i));
        }
      JS
    end
    assert_includes err.message, "memory budget"
  ensure
    vm&.dispose
  end
end
