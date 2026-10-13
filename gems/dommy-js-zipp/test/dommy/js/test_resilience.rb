# frozen_string_literal: true

require "test_helper"

# What a page's runaway or memory-hungry JS does to the host. Zipp raises a
# catchable error at the heap ceiling and the VM stays usable (it is not
# poisoned, unlike QuickJS); a runaway loop is interrupted at the deadline. A
# VM that is gone (disposed, or discarded after an engine panic) answers nil
# at every drive point, so the host stops driving it instead of crashing.
class Dommy::Js::TestResilience < Minitest::Test
  Backend = Dommy::Js::Zipp::Backend
  Config = Dommy::Js::Zipp::Config

  def test_out_of_memory_is_an_error_and_the_vm_survives
    backend = Backend.new(memory_limit: 8 * 1024 * 1024)
    assert_raises(::ZippRb::Error) do
      backend.eval("var a = []; for (;;) { a.push(new Array(100000).fill(7)); }")
    end
    refute backend.poisoned?
    backend.load("a = null;")
    backend.run_gc
    assert_equal 2, backend.eval("1 + 1")
  ensure
    backend&.dispose
  end

  def test_runaway_js_is_interrupted_at_the_deadline
    backend = Backend.new(timeout_msec: 300)
    started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    assert_raises(::ZippRb::InterruptedError) { backend.eval("while (true) {}") }
    assert_operator Process.clock_gettime(Process::CLOCK_MONOTONIC) - started, :<, 5
    assert_equal 3, backend.eval("1 + 2"), "the VM is usable after the interruption"
  ensure
    backend&.dispose
  end

  # The guard covers every way of running something, so the answer does not
  # depend on which one the host reached for.
  def test_every_way_of_running_something_no_ops_on_a_dead_vm
    backend = Backend.new
    backend.dispose
    assert backend.poisoned?

    assert_nil backend.eval("1 + 1")
    assert_nil backend.load("globalThis.ran = true;")
    assert_nil backend.eval_awaited("return 1;"), "the awaited eval behind #evaluate"
    assert_nil backend.run_bundle("probe.js", "globalThis.ran = true;"), "an engine bundle"
    assert_nil backend.import_module_url("http://example.test/m.js"), "an ES module"
    assert_nil backend.call_js("Math.max", 1, 2)
    assert_nil backend.drain_microtasks
    assert_nil backend.run_gc
  end

  def test_the_memory_ceiling_honors_the_env_override
    original = ENV["DOMMY_JS_MEMORY_LIMIT_MB"]

    ENV.delete("DOMMY_JS_MEMORY_LIMIT_MB")
    assert_equal Config::DEFAULT_MEMORY_LIMIT, Config.memory_limit

    ENV["DOMMY_JS_MEMORY_LIMIT_MB"] = "16"
    assert_equal 16 * 1024 * 1024, Config.memory_limit

    ENV["DOMMY_JS_MEMORY_LIMIT_MB"] = "0" # junk/zero keeps the default, as the timeout does
    assert_equal Config::DEFAULT_MEMORY_LIMIT, Config.memory_limit
  ensure
    ENV["DOMMY_JS_MEMORY_LIMIT_MB"] = original
  end

  def test_eval_timeout_honors_the_env_override
    original = ENV["DOMMY_JS_TIMEOUT_MSEC"]

    ENV.delete("DOMMY_JS_TIMEOUT_MSEC")
    assert_equal Config::DEFAULT_TIMEOUT_MSEC, Config.timeout_msec

    ENV["DOMMY_JS_TIMEOUT_MSEC"] = "15000"
    assert_equal 15_000, Config.timeout_msec

    ENV["DOMMY_JS_TIMEOUT_MSEC"] = "0" # ignore a junk/zero value, keep the default
    assert_equal Config::DEFAULT_TIMEOUT_MSEC, Config.timeout_msec
  ensure
    ENV["DOMMY_JS_TIMEOUT_MSEC"] = original
  end
end
