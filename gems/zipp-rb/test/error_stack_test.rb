# frozen_string_literal: true

require "test_helper"

# Error.prototype.stack: the frames an Error was created in, V8's format.
class ErrorStackTest < Minitest::Test
  def setup
    @vm = ZippRb::VM.new(timeout_msec: 5_000)
    @vm.load(<<~JS, filename: "https://example.test/lib.js")
      function inner() { throw new TypeError("deep"); }
      function outer() {
        return inner();
      }
      function viaNative() {
        return [1].map(function cb(v) { return v.missing.prop; });
      }
    JS
  end

  def teardown
    @vm.dispose
  end

  def test_stack_names_each_frame_with_its_script_line_and_column
    stack = @vm.eval("try { outer(); } catch (e) { e.stack }", filename: "https://example.test/page")
    assert_equal <<~STACK.chomp, stack
      TypeError: deep
          at inner (https://example.test/lib.js:1:20)
          at outer (https://example.test/lib.js:3:3)
          at https://example.test/page:1:7
    STACK
  end

  def test_an_engine_error_through_a_native_call_has_the_frames_too
    stack = @vm.eval("try { viaNative(); } catch (e) { e.stack }")
    lines = stack.lines.map(&:strip)
    assert_match(/\ATypeError: Cannot read properties of undefined/, lines[0])
    assert_equal ["at cb (https://example.test/lib.js:6:35)", "at viaNative (https://example.test/lib.js:6:3)",
                  "at <anonymous>:1:7"], lines[1..]
  end

  def test_a_js_error_raised_into_ruby_carries_the_frames_as_its_backtrace
    error = assert_raises(ZippRb::TypeError) { @vm.eval("outer()") }
    assert_equal ["at inner (https://example.test/lib.js:1:20)", "at outer (https://example.test/lib.js:3:3)",
                  "at <anonymous>:1:1"], error.backtrace
  end

  def test_a_load_that_throws_carries_the_frames_too
    error = assert_raises(ZippRb::Error) { @vm.load("\n  outer();", filename: "boot.js") }
    assert_equal "at boot.js:2:3", error.backtrace.last
  end

  def test_an_assigned_stack_wins
    assert_equal "custom", @vm.eval("var e = new Error('x'); e.stack = 'custom'; e.stack")
  end

  def test_a_non_error_has_no_stack
    assert_equal :undefined, @vm.eval("Error.prototype.stack")
  end

  def test_a_subclass_and_an_arrow_report_positions
    @vm.load("class MyError extends Error {}\nglobalThis.make = () => new MyError('m');", filename: "c.js")
    assert_equal "Error: m\n    at c.js:2:19\n    at <anonymous>:1:1", @vm.eval("make().stack")
  end

  def test_the_name_lasts_only_for_its_evaluation
    @vm.load("globalThis.later = function later() { return new Error('l').stack; };", filename: "named.js")
    assert_includes @vm.eval("later()"), "at later (named.js:1:"
    assert_includes @vm.eval("new Error('x').stack"), "at <anonymous>:1:1"
  end
end
