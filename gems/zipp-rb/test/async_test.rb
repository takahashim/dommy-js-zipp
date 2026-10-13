# frozen_string_literal: true

require "test_helper"

# eval_async must not report an unsettled top-level promise as a normal result:
# a pending promise or an elapsed max_ms is an explicit error.
class AsyncTest < Minitest::Test
  def test_pending_promise_raises_no_await_error
    vm = ZippRb::VM.new
    assert_raises(ZippRb::NoAwaitError) do
      vm.eval_async("return await new Promise(() => {});", max_ms: 10)
    end
    # The VM stays usable.
    assert_equal 1, vm.eval_async("return 1")
  ensure
    vm.dispose
  end

  def test_timed_out_promise_raises_no_await_error
    vm = ZippRb::VM.new(features: [ZippRb::FEATURE_TIMEOUT])
    assert_raises(ZippRb::NoAwaitError) do
      vm.eval_async("return await new Promise(r => setTimeout(() => r(42), 100));", max_ms: 5)
    end
  ensure
    vm.dispose
  end

  def test_settled_promise_returns_its_value
    vm = ZippRb::VM.new(features: [ZippRb::FEATURE_TIMEOUT])
    assert_equal 42, vm.eval_async("return await new Promise(r => setTimeout(() => r(42), 1));")
  ensure
    vm.dispose
  end
end
