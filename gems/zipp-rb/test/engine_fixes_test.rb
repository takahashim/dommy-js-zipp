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

  # `obj.hasOwnProperty(k)` on a Proxy asks its trap, as the
  # `Object.prototype.hasOwnProperty.call` form does (testharness.js's
  # assert_array_equals reads a NodeList this way).
  def test_has_own_property_method_on_a_proxy_uses_its_trap
    @vm.load("var p = new Proxy({}, { getOwnPropertyDescriptor: (t, k) => (k === '0' ? { value: 1, configurable: true } : undefined) });")
    assert_equal [true, true, false], @vm.eval("[p.hasOwnProperty(0), p.hasOwnProperty('0'), p.hasOwnProperty(1)]")
  end

  # A host's load/eval runs a classic script, as a browser runs <script>: its
  # declarations reach the global even when the script is strict, and its
  # completion value is the result.
  def test_a_strict_script_declares_into_the_global
    @vm.load('"use strict"; var v = 1; function f() {} const c = 3; let l = 4; class K {}')
    assert_equal %w[number function number number function], @vm.eval("[typeof v, typeof f, typeof c, typeof l, typeof K]")
    assert_equal 10, @vm.eval('"use strict"; var w = 5; w * 2')
    assert_raises(ZippRb::SyntaxError) { @vm.eval("const c = 1;") }
  end

  # A function's `prototype` made non-writable by defineProperty stays so
  # (WebIDL's legacy factory functions, `Image`, are defined this way).
  def test_a_function_prototype_can_be_made_non_writable
    @vm.load("var P = {}; function F() {} Object.defineProperty(F, 'prototype', { value: P, writable: false });")
    assert_equal [false, false, true], @vm.eval("var d = Object.getOwnPropertyDescriptor(F, 'prototype'); [d.writable, d.configurable, F.prototype === P]")
    assert_equal true, @vm.eval("F.prototype = {}; F.prototype === P")
    assert_raises(ZippRb::TypeError) { @vm.eval("'use strict'; F.prototype = {};") }
    assert_raises(ZippRb::TypeError) { @vm.eval("Object.defineProperty(F, 'prototype', { writable: true })") }
  end

  # A function whose [[Prototype]] is a class (or another function) inherits
  # its static members, as Stimulus's `bless` relies on.
  def test_a_function_inherits_the_statics_of_its_prototype_class
    @vm.load(<<~JS)
      class C { static s() { return "static"; } }
      C.data = [1];
      function f() {}
      Reflect.setPrototypeOf(f, C);
      function g() {}
      g.own = 2;
      function h() {}
      Object.setPrototypeOf(h, g);
    JS
    assert_equal [[1], "static", 2], @vm.eval("[f.data, f.s(), h.own]")
  end

  # A script's top-level `const` destructuring initializes its globals (the
  # pre-initialization a `let` needs made the const's write the second one).
  def test_top_level_const_destructuring
    @vm.load('const {a, b = a + 1, c: {d}} = {a: 1, c: {d: 4}}; const [x, y] = [5, 6];')
    assert_equal [1, 2, 4, 5, 6], @vm.eval("[a, b, d, x, y]")
    @vm.load('"use strict"; const {s} = {s: 7};')
    assert_equal 7, @vm.eval("s")
    # An eval's own lexicals are locals: its destructuring initializes them.
    assert_equal [3, 4], @vm.eval("(function () { eval('const {p, q} = {p: 3, q: 4}; globalThis.pq = [p, q];'); return pq; })()")
    assert_equal 9, @vm.eval("(0, eval)('const {r} = {r: 9}; r')")
    error = assert_raises(ZippRb::TypeError) { @vm.eval("a = 2") }
    assert_equal "Assignment to constant variable.", error.message
  end

  # A typed array's elements are own properties to hasOwnProperty too.
  def test_typed_array_indices_are_own_properties
    @vm.load("var u = new Uint8Array([7, 8]); u.named = 1;")
    assert_equal [true, true, false, false, true, true],
      @vm.eval("[u.hasOwnProperty(0), Object.prototype.hasOwnProperty.call(u, 1), u.hasOwnProperty(2), u.hasOwnProperty('-0'), Object.hasOwn(u, 0), u.hasOwnProperty('named')]")
  end

  # A Proxy can be constructed only when its target can.
  def test_new_on_a_proxy_of_a_non_constructor_throws
    @vm.load("var arrow = new Proxy(() => 1, { construct() { return {}; } }); var fn = new Proxy(function () {}, {});")
    assert_equal "TypeError", @vm.eval("try { new arrow(); 'constructed' } catch (e) { e.name }")
    assert_equal "object", @vm.eval("typeof new fn()")
  end

  # A derived class does not read newTarget.prototype itself; the base that
  # creates the object does, once — and a Proxy base with a construct trap
  # reads it only if the trap does (dommy's HTMLElement is such a Proxy).
  def test_a_derived_class_leaves_the_prototype_get_to_its_base
    @vm.load(<<~JS)
      function counter(C) {
        let n = 0;
        const p = new Proxy(C, { get(t, k, r) { if (k === "prototype") n++; return Reflect.get(t, k, r); } });
        return [p, () => n];
      }
      class A {} class B extends A {}
      const Base = new Proxy(function () {}, { construct(t, a, nt) { return Object.create(nt.prototype); } });
      class D extends Base {}
      var results = [B, D].map((K) => { const [p, n] = counter(K); const o = new p(); return [n(), Object.getPrototypeOf(o) === K.prototype]; });
    JS
    assert_equal [[1, true], [1, true]], @vm.eval("results")
  end
end
