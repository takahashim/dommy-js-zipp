# frozen_string_literal: true

require "test_helper"

class ZippRbTest < Minitest::Test
  def test_eval_number
    vm = ZippRb::VM.new
    assert_equal 3, vm.eval("1 + 2")
  end

  def test_eval_float
    vm = ZippRb::VM.new
    assert_equal 1.5, vm.eval("1.5")
  end

  def test_eval_string
    vm = ZippRb::VM.new
    assert_equal "hi", vm.eval("'hi'")
  end

  def test_eval_array
    vm = ZippRb::VM.new
    assert_equal [1, 2, 3], vm.eval("[1, 2, 3]")
  end

  def test_eval_hash
    vm = ZippRb::VM.new
    assert_equal({ "a" => 1, "b" => "x" }, vm.eval("({ a: 1, b: 'x' })"))
  end

  def test_nested_structures
    vm = ZippRb::VM.new
    assert_equal({ "list" => [1, { "deep" => true }] }, vm.eval("({ list: [1, { deep: true }] })"))
  end

  def test_state_persists_across_evals
    vm = ZippRb::VM.new
    vm.eval("var x = 41")
    assert_equal 42, vm.eval("x + 1")
  end

  def test_undefined_and_null
    vm = ZippRb::VM.new
    assert_equal ZippRb::Value::UNDEFINED, vm.eval("undefined")
    assert_nil vm.eval("null")
  end

  def test_throw_raises_mapped_error
    vm = ZippRb::VM.new
    err = assert_raises(ZippRb::TypeError) { vm.eval("throw new TypeError('boom')") }
    assert_equal "boom", err.message
    assert_kind_of ZippRb::Error, err
    assert_kind_of RuntimeError, err
  end

  def test_syntax_error_maps
    vm = ZippRb::VM.new
    assert_raises(ZippRb::SyntaxError) { vm.eval("not valid js (") }
  end

  def test_call_ruby_function_from_js
    vm = ZippRb::VM.new
    vm.define_function("add") { |a, b| a + b }
    assert_equal 7, vm.eval("add(3, 4)")
  end

  def test_callback_receives_js_values
    vm = ZippRb::VM.new
    seen = nil
    vm.define_function("capture") { |v| seen = v }
    vm.eval("capture([1, 2, 3])")
    assert_equal [1, 2, 3], seen
  end

  def test_callback_returns_structures
    vm = ZippRb::VM.new
    vm.define_function("make") { { "a" => 1, "b" => [true, nil] } }
    assert_equal({ "a" => 1, "b" => [true, nil] }, vm.eval("make()"))
  end

  def test_callback_error_propagates_to_js
    vm = ZippRb::VM.new
    vm.define_function("boom") { raise "kaboom" }
    err = assert_raises(RuntimeError) { vm.eval("boom()") }
    assert_match(/kaboom/, err.message)
  end

  def test_in_memory_esm_from_string_sources
    vm = ZippRb::VM.new
    vm.set_module_provider("/app", {
      "/app/dep.mjs" => "export const two = 2;",
      "/app/mod.mjs" => "import { two } from './dep.mjs'; export const value = two * 21;",
    })
    vm.eval("globalThis.__v = null; import('./mod.mjs').then(ns => { globalThis.__v = ns.value; });")
    assert_equal 42, vm.eval("globalThis.__v")
  end

  def test_in_memory_esm_from_proc_source
    vm = ZippRb::VM.new
    vm.set_module_provider("/app", { "/app/lazy.mjs" => ->(path) { "export const p = 7;" } })
    vm.eval("globalThis.__p = null; import('./lazy.mjs').then(ns => { globalThis.__p = ns.p; });")
    assert_equal 7, vm.eval("globalThis.__p")
  end

  def test_reentry_from_callback_evaluates_same_vm
    vm = ZippRb::VM.new
    vm.define_function("reenter") { vm.eval("1 + 2") }
    assert_equal 3, vm.eval("reenter()")
  end

  def test_nested_callback_calls_js
    vm = ZippRb::VM.new
    vm.define_function("double") { |x| x * 2 }
    vm.define_function("nested") { vm.call("double", 21) }
    assert_equal 42, vm.eval("nested()")
  end

  # A re-entrant call leaves queued microtasks alone: they run once the
  # outermost entry's stack is empty, as in a browser.
  def test_reentry_does_not_run_microtasks_early
    vm = ZippRb::VM.new
    vm.load(<<~JS)
      globalThis.order = [];
      globalThis.queue = () => { Promise.resolve().then(() => order.push("microtask")); };
      globalThis.noop = () => {};
    JS
    handle = vm.handle("noop")
    vm.define_function("reenter") do
      vm.call("queue")
      vm.call("noop")
      vm.eval("order.push('eval')")
      handle.call
      nil
    end
    vm.eval("reenter(); order.push('after host call');")
    assert_equal ["eval", "after host call", "microtask"], vm.eval("order")
  end

  # A listener runs while a nested evaluation holds the VM's reentry context,
  # so its own evaluation of the same VM is refused, as it is at top level,
  # rather than handed a second reference to that context.
  def test_listener_cannot_reenter_a_nested_evaluation
    vm = ZippRb::VM.new
    seen = []
    vm.on_log do |_severity, message|
      seen << message
      seen << (vm.eval("40 + 2") rescue $!.class)
    end
    vm.define_function("nested") { vm.eval("console.log('inside'); 1") }
    assert_equal 1, vm.eval("nested()")
    assert_equal ["inside", RuntimeError], seen
    assert_equal 42, vm.eval("40 + 2")
  end

  def test_invoke_leaves_microtasks_for_the_host
    vm = ZippRb::VM.new
    vm.load("globalThis.order = []; globalThis.step = (n) => { order.push(n); Promise.resolve().then(() => order.push('micro' + n)); };")
    vm.invoke("step", 1)
    vm.invoke("step", 2)
    assert_equal [1, 2], vm.eval("order.slice()")
    vm.drain_jobs!
    assert_equal [1, 2, "micro1", "micro2"], vm.eval("order")
    vm.call("step", 3)
    assert_equal [1, 2, "micro1", "micro2", 3, "micro3"], vm.eval("order")
  end

  def test_call_global_from_ruby
    vm = ZippRb::VM.new
    vm.eval("function add(a, b) { return a + b }")
    assert_equal 5, vm.call("add", 2, 3)
  end

  def test_handle_holds_js_function
    vm = ZippRb::VM.new
    vm.eval("function adder(a, b) { return a + b }")
    handle = vm.handle("adder")
    assert_instance_of ZippRb::Handle, handle
    assert_equal 5, handle.call(2, 3)
    assert handle.release
    refute handle.release
  end

  def test_handle_survives_allocations
    vm = ZippRb::VM.new
    vm.eval("function f() { return 99 }")
    handle = vm.handle("f")
    200.times { vm.eval("[1,2,3,4,5,6,7,8].map(x => ({ x }))") }
    assert_equal 99, handle.call
  end

  def test_on_log_receives_console_lines
    vm = ZippRb::VM.new
    lines = []
    vm.on_log { |severity, message| lines << [severity, message] }
    vm.eval("console.log('hi'); console.info('i'); console.debug('d'); console.warn('w'); console.error('bad');")
    assert_equal [[:info, "hi"], [:info, "i"], [:verbose, "d"], [:warning, "w"], [:error, "bad"]], lines
  end

  # A page may replace a console method; the replacement is what runs.
  def test_a_replaced_console_method_is_called
    vm = ZippRb::VM.new
    lines = []
    vm.on_log { |severity, message| lines << [severity, message] }
    vm.eval("const warn = console.warn; console.warn = (m) => warn('wrapped ' + m); console.warn('x');")
    assert_equal [[:warning, "wrapped x"]], lines
  end

  def test_on_unhandled_rejection
    vm = ZippRb::VM.new
    seen = []
    vm.on_unhandled_rejection { |message| seen << message }
    vm.eval("Promise.reject(new Error('boom'));")
    vm.drain_jobs!
    assert(seen.any? { |m| m.include?("boom") }, "seen #{seen.inspect}")
  end

  # The listener runs once the VM is free again, so it may drive the VM.
  def test_an_unhandled_rejection_listener_may_reenter_the_vm
    vm = ZippRb::VM.new
    vm.load("globalThis.reported = [];")
    vm.on_unhandled_rejection { |message| vm.eval("reported.push(#{message.dump})") }
    vm.eval("Promise.reject(new Error('boom'));")
    vm.drain_jobs!
    assert_equal ["Error: boom"], vm.eval("reported")
  end

  def test_promise_rejection_hook_sees_the_promise_and_the_reason
    vm = ZippRb::VM.new
    vm.load(<<~JS)
      globalThis.events = [];
      globalThis.hook = (type, promise, reason) => events.push([type, promise === globalThis.p, reason === globalThis.reason]);
    JS
    vm.promise_rejection_hook = "hook"
    vm.eval("globalThis.reason = { why: 1 }; globalThis.p = Promise.reject(reason);")
    vm.drain_jobs!
    assert_equal [["unhandledrejection", true, true]], vm.eval("events")
    vm.eval("p.catch(() => {});")
    vm.drain_jobs!
    assert_equal [["unhandledrejection", true, true], ["rejectionhandled", true, true]], vm.eval("events")
  end

  # A Ruby string that is not valid UTF-8 crosses with each invalid sequence
  # as U+FFFD (quickjs.rb's behaviour), not as an empty string.
  def test_invalid_utf8_becomes_replacement_characters
    vm = ZippRb::VM.new
    vm.define_function("binary") { "\xFFa".b }
    vm.define_function("broken") { "ok\xE3\x81".dup.force_encoding("UTF-8") }
    assert_equal ["\uFFFDa", 2, "ok\uFFFD"], vm.eval("[binary(), binary().length, broken()]")
  end

  def test_drain_jobs_runs_promise_callbacks
    vm = ZippRb::VM.new
    vm.eval("globalThis.__v = 0; Promise.resolve().then(() => { globalThis.__v = 42; });")
    vm.drain_jobs!
    assert_equal 42, vm.eval("globalThis.__v")
  end

  def test_import_returns_namespace_exports
    vm = ZippRb::VM.new
    vm.set_module_provider("/app", {
      "/app/dep.mjs" => "export const two = 2;",
      "/app/mod.mjs" => "import { two } from './dep.mjs'; export const value = two * 21; export default 'x';",
    })
    ns = vm.import("/app/mod.mjs")
    assert_equal 42, ns["value"]
    assert_equal "x", ns["default"]
  end

  def test_import_reports_missing_module
    vm = ZippRb::VM.new
    vm.set_module_provider("/app", {})
    err = assert_raises(ZippRb::Error) { vm.import("/app/nope.mjs") }
    assert_match(/not found/, err.message)
  end

  def test_load_runs_for_side_effects_and_discards_the_value
    vm = ZippRb::VM.new
    assert_nil vm.load("globalThis.x = 41")
    assert_equal 42, vm.eval("x + 1")
    # A large completion value (a bundle's factory result) is not walked.
    assert_nil vm.load("var big = {}; for (let i = 0; i < 5000; i++) big['k' + i] = i; big")
  ensure
    vm.dispose
  end

  def test_eval_code_module_function
    assert_equal 3, ZippRb.eval_code("1 + 2")
  end

  def test_define_function_array_path
    vm = ZippRb::VM.new
    assert_equal %i[helpers double], vm.define_function(%w[helpers double]) { |x| x * 2 }
    assert_equal 42, vm.eval("helpers.double(21)")
  ensure
    vm.dispose
  end

  def test_memory_usage
    vm = ZippRb::VM.new
    vm.load("var big = []; for (let i = 0; i < 1000; i++) big.push({ i: i });")
    usage = vm.memory_usage
    assert_operator usage[:heap_bytes], :>, 0
    assert_includes usage.keys, :retained_functions
  ensure
    vm.dispose
  end

  def test_define_function_async
    vm = ZippRb::VM.new(features: [ZippRb::FEATURE_TIMEOUT])
    vm.define_function("later", :async) { 42 }
    assert vm.eval("later() instanceof Promise")
    assert_equal 42, vm.eval_async("return await later();")
    vm.define_function("boom", :async) { raise "nope" }
    assert_equal "caught:nope",
                 vm.eval_async('try { await boom(); return "no"; } catch (e) { return "caught:" + e.message; }')
  ensure
    vm.dispose
  end

  def test_gc_and_poisoned
    vm = ZippRb::VM.new
    vm.eval("var a = [1, 2, 3]")
    assert_nil vm.gc!
    refute vm.poisoned?
    refute vm.memory_poisoned?
  ensure
    vm.dispose
  end

  def test_dispose_is_idempotent
    vm = ZippRb::VM.new
    refute vm.disposed?
    vm.dispose
    vm.dispose
    assert vm.disposed?
  end
end
