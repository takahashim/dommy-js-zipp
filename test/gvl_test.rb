# frozen_string_literal: true

require "test_helper"

# GVL behaviour: a VM with no Ruby bridge releases the GVL during evaluation, so
# other Ruby threads keep running; a bridged VM (callbacks/listeners/timers/
# module provider) keeps it.
class GvlTest < Minitest::Test
  LONG = "let s = 0; for (let i = 0; i < 10000000; i++) s = (s + i) | 0; s"

  def other_thread_progress(vm)
    progress = 0
    stop = false
    thread = Thread.new { progress += 1 until stop }
    sleep 0.02
    before = progress
    vm.eval(LONG)
    after = progress - before
    stop = true
    thread.join
    after
  end

  def test_pure_eval_releases_the_gvl
    vm = ZippRb::VM.new
    assert_operator other_thread_progress(vm), :>, 10_000,
                    "another thread should run during a GVL-free eval"
  ensure
    vm.dispose
  end

  def test_bridged_eval_keeps_the_gvl
    vm = ZippRb::VM.new
    vm.define_function("noop") { 1 }
    assert_operator other_thread_progress(vm), :<, 10_000,
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
