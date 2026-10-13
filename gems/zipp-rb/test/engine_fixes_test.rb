# frozen_string_literal: true

require "test_helper"

# Behaviour of the local engine fixes listed in engine/LOCAL_CHANGES.md.
class EngineFixesTest < Minitest::Test
  def setup
    @vm = ZippRb::VM.new(timeout_msec: 5_000)
  end

  def teardown
    @vm.dispose
  end

  # A promise's resolving functions inherit call/apply/bind like any function;
  # dommy's bridge settles a host promise with `resolve.apply(…)`.
  def test_promise_resolving_functions_inherit_function_prototype
    @vm.load(<<~JS)
      var resolve, reject;
      var promise = new Promise((a, b) => { resolve = a; reject = b; });
      var settled;
      promise.then((v) => { settled = v; });
    JS
    assert_equal %w[function function function function],
      @vm.eval("[typeof resolve.call, typeof resolve.apply, typeof resolve.bind, typeof reject.apply]")
    @vm.eval("resolve.apply(undefined, [42])")
    @vm.drain_jobs!
    assert_equal 42, @vm.eval("settled")
  end

  # The same for Promise.all's per-element resolve functions.
  def test_combinator_resolving_functions_inherit_function_prototype
    @vm.load(<<~JS)
      var element;
      var thenable = { then(resolve) { element = resolve; } };
      var all = Promise.all([thenable]);
    JS
    @vm.drain_jobs!
    assert_equal "function", @vm.eval("typeof element.apply")
  end

  # A builtin replaced through the global object before any script names it
  # bare is the binding later scripts see. dommy installs its timer globals
  # this way, and an htmx swap scheduled through the engine's own setTimeout
  # never settled.
  def test_a_builtin_replaced_on_the_global_object_is_seen_by_later_scripts
    @vm.load("globalThis.setTimeout = function () { return 'replaced'; }; globalThis.Map = function () { return 'map'; };")
    assert_equal %w[replaced map], @vm.eval("[setTimeout(), Map()]")
    @vm.load("(function () { 'use strict'; globalThis.strictRead = function () { return setTimeout(); }; })();")
    assert_equal "replaced", @vm.eval("strictRead()")
  end

  # Once bound, the bare name and the global property stay one binding.
  def test_a_replaced_builtin_stays_in_sync_with_the_global_object
    @vm.load("globalThis.setTimeout = function () { return 'first'; };")
    assert_equal "first", @vm.eval("setTimeout()")
    @vm.load("setTimeout = function () { return 'bare'; };")
    assert_equal %w[bare bare], @vm.eval("[setTimeout(), globalThis.setTimeout()]")
    @vm.load("globalThis.setTimeout = function () { return 'prop'; };")
    assert_equal %w[prop prop], @vm.eval("[setTimeout(), globalThis.setTimeout()]")
  end

  # performance.now() is a method of a real `performance` global, so a host's
  # replacement (dommy's virtual clock) is what runs.
  def test_performance_is_a_replaceable_global
    assert_equal ["object", true], @vm.eval("[typeof performance, performance.now() >= 0]")
    @vm.load("globalThis.performance = { now() { return 42; } };")
    assert_equal 42, @vm.eval("performance.now()")
  end

  # `typeof name` inside `with` sees the with-object's binding (dommy compiles
  # inline event handlers inside `with` scopes for element/form/document).
  def test_typeof_inside_with_sees_the_with_object
    @vm.load(<<~JS)
      var scope = new Proxy({}, { has: (t, k) => k === "cellIndex", get: (t, k) => (k === "cellIndex" ? 3 : undefined) });
      var doc = { getElementById() {} };
      var handler = (0, eval)("(function (a, b) { with (a) with (b) { return function () { return [typeof cellIndex, typeof getElementById, typeof missing, typeof Math]; }; } })")(doc, scope);
    JS
    assert_equal %w[number function undefined object], @vm.eval("handler()")
  end

  # A builtin deleted before any script names it stays deleted, and `typeof`
  # agrees with a plain read once the global object owns the binding.
  def test_typeof_agrees_with_a_read_of_a_deleted_then_replaced_builtin
    @vm.load("delete globalThis.performance;")
    @vm.load("globalThis.probe = () => typeof performance; globalThis.read = () => performance;")
    assert_equal "undefined", @vm.eval("probe()")
    @vm.load("globalThis.performance = { now() { return 1; } };")
    assert_equal "object", @vm.eval("probe()")
    @vm.load("(0, eval)('performance = 1');")
    assert_equal ["number", 1], @vm.eval("[probe(), read()]")
  end
end
