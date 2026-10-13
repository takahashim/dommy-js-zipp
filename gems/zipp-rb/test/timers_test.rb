# frozen_string_literal: true

require "test_helper"

# setTimeout/setInterval (opt-in via FEATURE_TIMEOUT) driven by the binding's
# event loop, and await-capable eval. Mirrors quickjs.rb's feature_timeout model
# (timers run while an await drains the loop).
class TimersTest < Minitest::Test
  def vm
    @vm ||= ZippRb::VM.new(features: [ZippRb::FEATURE_TIMEOUT])
  end

  def teardown
    @vm&.dispose
  end

  def test_await_set_timeout
    assert_equal "done",
                 vm.eval_async("return await new Promise(r => setTimeout(() => r('done'), 20));")
  end

  def test_timer_resolves_a_value
    assert_equal 42, vm.eval_async("return await new Promise(r => setTimeout(() => r(42), 5));")
  end

  def test_microtask_runs_before_timer
    out = vm.eval_async(<<~JS)
      const seq = [];
      Promise.resolve().then(() => seq.push("micro"));
      await new Promise(r => setTimeout(r, 0));
      seq.push("timer");
      return seq.join(",");
    JS
    assert_equal "micro,timer", out
  end

  def test_clear_timeout
    out = vm.eval_async(<<~JS)
      let fired = false;
      const id = setTimeout(() => { fired = true; }, 0);
      clearTimeout(id);
      await new Promise(r => setTimeout(r, 10));
      return fired;
    JS
    assert_equal false, out
  end

  def test_set_interval_until_cleared
    out = vm.eval_async(<<~JS)
      let n = 0;
      const id = setInterval(() => { n++; if (n === 3) clearInterval(id); }, 2);
      return await new Promise(r => setTimeout(() => r(n), 60));
    JS
    assert_equal 3, out
  end

  def test_timer_callback_can_call_ruby
    vm.define_function("note") { |v| @seen = v }
    vm.eval_async("setTimeout(() => note('timed'), 3); await new Promise(r => setTimeout(r, 15)); return 0;")
    assert_equal "timed", @seen
  end
end
