# frozen_string_literal: true

require "test_helper"

# A ported subset of quickjs.rb's test/quickjs_test.rb, adapted to ZippRb's
# API and semantics. Differences (and unimplemented items) are recorded in
# notes/quickjs-compat-table.ja.md.
class QuickjsCompatTest < Minitest::Test
  def vm = ZippRb::VM.new

  # ---- ResultConversion -------------------------------------------------

  def test_null_is_nil
    assert_nil vm.eval("null")
  end

  def test_undefined_is_a_specific_constant
    v = vm
    assert_equal ZippRb::Value::UNDEFINED, v.eval("undefined")
    assert_equal ZippRb::Value::UNDEFINED, v.eval("const o = {}; o.missing;")
  end

  def test_nan_is_a_specific_constant
    assert_equal ZippRb::Value::NAN, vm.eval("Number('whatever')")
  end

  def test_string_becomes_string
    v = vm
    assert_equal "1", v.eval("'1'")
    assert_equal "ボーナス", v.eval("'ボーナス'")
    assert_equal "🆔", v.eval("'🆔'")
  end

  def test_integer_becomes_integer
    assert_equal 5, vm.eval("2+3")
  end

  def test_float_becomes_float
    v = vm
    assert_equal 1.0, v.eval("1.0")
    assert_equal 2**0.5, v.eval("2 ** 0.5")
  end

  def test_boolean_becomes_true_or_false
    v = vm
    assert_equal false, v.eval("false")
    assert_equal true, v.eval("1 === 1")
    assert_equal false, v.eval("1 == 3")
  end

  def test_plain_object_becomes_hash
    v = vm
    assert_equal({}, v.eval("const o = {}; o"))
    assert_equal({ "a" => "1", "b" => 1 }, v.eval("const o = { a: '1', b: 1 }; o;"))
  end

  def test_plain_array_becomes_array
    v = vm
    assert_equal [1, 2, 3], v.eval("[1, 2, 3]")
    assert_equal ["a", 2, "third"], v.eval("['a', 2, 'third']")
    assert_equal [1, 2, { "third" => "sad" }], v.eval("[1, 2, { third: 'sad' }]")
  end

  def test_undefined_nested_is_preserved
    v = vm
    assert_equal({ "a" => ZippRb::Value::UNDEFINED, "b" => 1 }, v.eval("({ a: undefined, b: 1 })"))
    assert_equal [1, ZippRb::Value::UNDEFINED, 3], v.eval("[1, undefined, 3]")
  end

  def test_nan_nested_is_preserved
    assert_equal({ "a" => ZippRb::Value::NAN }, vm.eval("({ a: NaN })"))
  end

  def test_circular_object_becomes_nil_entry
    assert_equal({ "self" => nil }, vm.eval("(() => { const c = {}; c.self = c; return c })()"))
  end

  # ---- Exceptions -------------------------------------------------------

  def test_syntax_error
    err = assert_raises(ZippRb::SyntaxError) { vm.eval("}{") }
    assert_equal "SyntaxError", err.js_name
  end

  def test_type_error
    err = assert_raises(ZippRb::TypeError) { vm.eval("globalThis.func()") }
    assert_equal "TypeError", err.js_name
  end

  def test_reference_error
    err = assert_raises(ZippRb::ReferenceError) { vm.eval("undefinedVariable;") }
    assert_equal "ReferenceError", err.js_name
  end

  def test_range_error
    err = assert_raises(ZippRb::RangeError) { vm.eval("throw new RangeError('out of range')") }
    assert_equal "out of range", err.message
    assert_equal "RangeError", err.js_name
  end

  def test_eval_error
    assert_raises(ZippRb::EvalError) { vm.eval("throw new EvalError('I am old')") }
  end

  def test_uri_error
    assert_raises(ZippRb::URIError) { vm.eval("decodeURIComponent('%')") }
  end

  def test_aggregate_error
    err = assert_raises(ZippRb::AggregateError) do
      vm.eval("throw new AggregateError([new Error('some error')], 'aggregated')")
    end
    assert_equal "aggregated", err.message
  end

  def test_custom_error_is_base_class_with_js_name
    code = "class MyError extends Error { constructor(m) { super(m); this.name = 'CustomError'; } }; " \
           "throw new MyError('my error')"
    err = assert_raises(ZippRb::Error) { vm.eval(code) }
    assert_equal "my error", err.message
    assert_equal "CustomError", err.js_name
  end

  # ---- VM ---------------------------------------------------------------

  def test_maintains_the_same_context_within_a_vm
    v = vm
    v.eval("var x = 10")
    assert_equal 11, v.eval("x + 1")
  end

  def test_dispose_is_idempotent
    v = vm
    refute v.disposed?
    v.dispose
    v.dispose
    assert v.disposed?
  end

  # ---- GlobalFunction ---------------------------------------------------

  def test_define_function_returns_a_symbol
    assert_equal :my_func, vm.define_function(:my_func) { 1 }
  end

  def test_multiple_functions_and_overwrite
    v = vm
    v.define_function("a") { 1 }
    v.define_function("b") { 2 }
    assert_equal 3, v.eval("a() + b()")
    v.define_function("a") { 10 }
    assert_equal 10, v.eval("a()")
  end

  def test_ruby_return_value_conversion
    v = vm
    v.define_function("give") { { "a" => [1, true, nil] } }
    assert_equal({ "a" => [1, true, nil] }, v.eval("give()"))
  end

  def test_ruby_exception_is_visible_to_js
    v = vm
    v.define_function("boom") { raise "kaboom" }
    # An uncaught callback exception reaches Ruby as itself (quickjs.rb does
    # the same), not as a ZippRb::Error.
    err = assert_raises(RuntimeError) { v.eval("boom()") }
    assert_equal "kaboom", err.message
  end
end
