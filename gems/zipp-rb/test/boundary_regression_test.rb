# frozen_string_literal: true

require "test_helper"
require "timeout"

# Regressions for boundary defects found in the 2026-10-13 review: value
# conversion, exceptions and non-local exits crossing a callback, interrupts
# around the GVL-free path, and per-call retention leaks.
class BoundaryRegressionTest < Minitest::Test
  def setup
    @vm = ZippRb::VM.new
  end

  def teardown
    @vm.dispose
  end

  # --- value conversion ------------------------------------------------------

  def test_floats_are_not_truncated
    @vm.eval("function id(x) { return x }")
    assert_equal 1.5, @vm.call("id", 1.5)
    assert_equal(-2.25, @vm.call("id", -2.25))
    assert_equal 2**70, @vm.call("id", 2**70).to_i
  end

  def test_sentinels_round_trip_through_a_callback
    @vm.define_function("echo") { |x| x }
    assert_equal %w[undefined number], @vm.eval("[typeof echo(undefined), typeof echo(NaN)]")
    assert_equal true, @vm.eval("Number.isNaN(echo(NaN))")
  end

  def test_cyclic_ruby_data_becomes_null_instead_of_crashing
    @vm.eval("function id(x) { return x }")
    a = [1]
    a << a
    assert_equal [1, nil], @vm.call("id", a)
    h = { "k" => 1 }
    h["self"] = h
    assert_equal({ "k" => 1, "self" => nil }, @vm.call("id", h))
  end

  def test_deep_ruby_data_is_cut_off_instead_of_crashing
    @vm.eval("function id(x) { return x }")
    deep = []
    100_000.times { deep = [deep] }
    result = @vm.call("id", deep)
    depth = 0
    while result.is_a?(Array) && !result.empty?
      result = result[0]
      depth += 1
    end
    assert_operator depth, :<=, 64
  end

  # --- exceptions and non-local exits ---------------------------------------

  class CustomError < StandardError; end

  def test_uncaught_callback_exception_is_reraised_as_itself
    @vm.define_function("boom") { raise CustomError, "custom" }
    err = assert_raises(CustomError) { @vm.eval("boom()") }
    assert_equal "custom", err.message
    assert_equal 2, @vm.eval("1 + 1"), "the VM stays usable"
  end

  def test_js_can_still_catch_a_standard_error
    @vm.define_function("boom") { raise CustomError, "custom" }
    assert_equal "caught:custom", @vm.eval("try { boom() } catch (e) { 'caught:' + e.message }")
  end

  def test_timeout_inside_a_callback_cannot_be_swallowed_by_js
    @vm.define_function("slow") { sleep 1 }
    assert_raises(Timeout::Error) do
      Timeout.timeout(0.05) { @vm.eval("let r; try { slow() } catch (e) { r = 'swallowed' }; r") }
    end
    assert_equal 2, @vm.eval("1 + 1")
  end

  def test_thread_kill_during_a_callback_kills_the_thread
    log = []
    thread = Thread.new do
      vm = ZippRb::VM.new
      vm.define_function("block") { sleep 1 }
      begin
        vm.eval("for (let i = 0; i < 3; i++) { try { block() } catch (e) {} }")
        log << :returned
      ensure
        log << :ensure
      end
    end
    sleep 0.05
    thread.kill
    thread.join
    assert_equal [:ensure], log
    assert_equal false, thread.status
  end

  def test_throw_through_a_callback_reaches_its_catch_not_js
    @vm.define_function("thrower") { throw :done, :thrown }
    result = catch(:done) { @vm.eval("try { thrower() } catch (e) {}; 'after'") }
    assert_equal :thrown, result
    assert_equal 2, @vm.eval("1 + 1"), "the VM stays usable"
  end

  # --- interrupts around the GVL-free path ----------------------------------

  # A signal pending when a GVL-free eval starts makes Ruby skip the call
  # (rb_thread_call_without_gvl2 returns NULL); that used to be dereferenced.
  # The signals come from a child process, as a real signal storm would.
  def test_pending_signal_on_gvl_free_entry_does_not_crash
    count = 0
    old = trap("USR1") { count += 1 }
    parent = Process.pid
    child = Process.fork do
      20_000.times do
        Process.kill("USR1", parent)
      rescue StandardError
        break
      end
      exit!(0)
    end
    deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + 2
    evals = 0
    wrong = 0
    while Process.clock_gettime(Process::CLOCK_MONOTONIC) < deadline
      wrong += 1 unless @vm.eval("1 + 1") == 2
      evals += 1
    end
    Process.wait(child)
    assert_equal 0, wrong
    assert_operator evals, :>, 0
    assert_operator count, :>, 0
  ensure
    trap("USR1", old || "DEFAULT")
  end

  # --- event loop ------------------------------------------------------------

  def test_eval_async_honours_max_ms_with_a_busy_interval
    vm = ZippRb::VM.new(features: [ZippRb::FEATURE_TIMEOUT])
    started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    assert_raises(ZippRb::NoAwaitError) do
      vm.eval_async(<<~JS, max_ms: 200)
        setInterval(() => { const end = Date.now() + 2; while (Date.now() < end) {} }, 1);
        await new Promise(() => {});
      JS
    end
    assert_operator Process.clock_gettime(Process::CLOCK_MONOTONIC) - started, :<, 2
  ensure
    vm&.dispose
  end

  def test_eval_async_releases_the_gvl_while_waiting
    vm = ZippRb::VM.new(features: [ZippRb::FEATURE_TIMEOUT])
    progress = 0
    stop = false
    other = Thread.new { progress += 1 until stop }
    sleep 0.02
    before = progress
    assert_equal 1, vm.eval_async("await new Promise(r => setTimeout(r, 200)); return 1")
    assert_operator progress - before, :>, 10_000
  ensure
    stop = true
    other&.join
    vm&.dispose
  end

  # --- retention -------------------------------------------------------------

  def test_fired_timers_and_settled_results_are_not_retained
    vm = ZippRb::VM.new(features: [ZippRb::FEATURE_TIMEOUT])
    vm.eval_async("for (let i = 0; i < 100; i++) setTimeout(() => {}, 0); await new Promise(r => setTimeout(r, 5)); return 1")
    20.times { vm.eval_async("return [1, 2, 3]") }
    assert_equal 0, vm.eval("Object.keys(__zipp_timer_cbs).length")
    assert_equal 0, vm.eval("Object.keys(globalThis).filter(k => k.startsWith('__zipp_async') || k.startsWith('__zipp_import')).length")
  ensure
    vm&.dispose
  end

  def test_handles_work_from_inside_a_callback
    @vm.eval("function f(x) { return x * 2 }")
    handle = @vm.handle("f")
    @vm.define_function("via_handle") { |x| handle.call(x) }
    @vm.define_function("read_handle") { handle.value.nil? }
    @vm.define_function("rel") { handle.release }
    assert_equal 42, @vm.eval("via_handle(21)")
    assert_equal true, @vm.eval("read_handle()"), "a function reads as nil"
    assert_equal true, @vm.eval("rel()")
    assert_raises(RuntimeError) { handle.call(1) }
  end

  def test_collected_handles_release_their_roots
    @vm.eval("function f() { return 1 }")
    vm_id = @vm.instance_variable_get(:@id)
    ids = Array.new(200) { @vm.handle("f").id }
    4.times { GC.start }
    @vm.eval("0") # the owning thread's next entry releases the queued roots
    live = ids.count do |hid|
      ZippRb._get_handle(vm_id, hid)
      true
    rescue RuntimeError
      false
    end
    assert_operator live, :<, ids.size / 2, "most collected handles are released"
  end
end
