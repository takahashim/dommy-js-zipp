# frozen_string_literal: true

require "test_helper"

# GVL behaviour: a VM with no Ruby bridge releases the GVL during evaluation, so
# other Ruby threads keep running; a bridged VM (callbacks/listeners/timers/
# module provider) keeps it.
class GvlTest < Minitest::Test
  LONG = "{ let s = 0; for (let i = 0; i < 10000000; i++) s = (s + i) | 0; s }"

  # The longest stretch in which another Ruby thread could not run while +vm+
  # evaluated LONG, as a fraction of how long LONG takes on its own. Measuring
  # the stall rather than counting the other thread's iterations keeps a thread
  # switch just before or after the native call (Ruby 3.2 switches on a pending
  # timer interrupt at a method boundary) from looking like progress during it.
  def other_thread_stall(vm)
    clock = -> { Process.clock_gettime(Process::CLOCK_MONOTONIC) }
    vm.eval(LONG)
    started = clock.call
    vm.eval(LONG)
    alone = clock.call - started

    max_gap = 0.0
    stop = false
    thread = Thread.new do
      last = clock.call
      until stop
        now = clock.call
        max_gap = now - last if now - last > max_gap
        last = now
      end
      # The stall may end just before the thread sees +stop+ and leaves.
      max_gap = [max_gap, clock.call - last].max
    end
    sleep 0.02
    vm.eval(LONG)
    stop = true
    thread.join
    max_gap / alone
  end

  def test_pure_eval_releases_the_gvl
    vm = ZippRb::VM.new
    assert_operator other_thread_stall(vm), :<, 0.5,
                    "another thread should run during a GVL-free eval"
  ensure
    vm.dispose
  end

  def test_bridged_eval_keeps_the_gvl
    vm = ZippRb::VM.new
    vm.define_function("noop") { 1 }
    assert_operator other_thread_stall(vm), :>, 0.8,
                    "a bridged VM should hold the GVL during eval"
  ensure
    vm.dispose
  end

  def test_async_exception_during_gvl_free_eval_leaves_vm_usable
    vm = ZippRb::VM.new
    main = Thread.current
    # A cross-thread Thread#raise lands during the GVL-free eval. It must not
    # strand the VM out of the registry: a later eval and dispose must work.
    raiser = Thread.new do
      sleep 0.05
      main.raise("killed")
    end
    assert_raises(RuntimeError) do
      vm.eval("let s = 0; for (let i = 0; i < 200000000; i++) s = (s + i) | 0; s")
    end
    raiser.join
    assert_equal 42, vm.eval("42"), "the VM must be restored after the async exception"
    assert_nil vm.dispose, "dispose must not report a VM still executing"
  ensure
    vm.dispose rescue nil
  end

  def test_pure_eval_still_correct
    vm = ZippRb::VM.new
    assert_equal 42, vm.eval("40 + 2")
  ensure
    vm.dispose
  end
end
