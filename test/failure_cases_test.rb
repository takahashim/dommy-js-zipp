# frozen_string_literal: true

require "test_helper"

# Stage-3 boundary failure cases: exceptions across the callback boundary,
# module errors, deep reentry, disposed-VM use, and surviving Ruby GC
# compaction.
class FailureCasesTest < Minitest::Test
  def test_js_exception_in_callback_surfaces_and_vm_survives
    vm = ZippRb::VM.new
    vm.define_function("boom") { raise "kaboom" }
    err = assert_raises(ZippRb::Error) { vm.eval("boom()") }
    assert_match(/kaboom/, err.message)
    assert_equal 1, vm.eval("1"), "the VM is usable after a callback error"
  ensure
    vm.dispose
  end

  def test_js_can_catch_a_ruby_callback_error
    vm = ZippRb::VM.new
    vm.define_function("boom") { raise "kaboom" }
    out = vm.eval("try { boom(); 'no' } catch (e) { 'caught:' + e.message }")
    assert_match(/caught:.*kaboom/, out)
  ensure
    vm.dispose
  end

  def test_invalid_module_source_is_a_syntax_error
    vm = ZippRb::VM.new
    vm.set_module_provider("/app", { "/app/bad.mjs" => "export const = ;" })
    err = assert_raises(ZippRb::Error) { vm.import("/app/bad.mjs") }
    assert_match(/Syntax|unexpected|Error/, err.message)
  ensure
    vm.dispose
  end

  def test_module_proc_that_raises_does_not_crash
    vm = ZippRb::VM.new
    vm.set_module_provider("/app", { "/app/x.mjs" => ->(_path) { raise "module boom" } })
    assert_raises(ZippRb::Error) { vm.import("/app/x.mjs") }
  ensure
    vm.dispose
  end

  def test_handle_for_unknown_name_is_nil
    vm = ZippRb::VM.new
    assert_nil vm.handle("does_not_exist")
  ensure
    vm.dispose
  end

  def test_using_a_disposed_vm_raises
    vm = ZippRb::VM.new
    vm.dispose
    assert_raises(ZippRb::Error) { vm.eval("1") }
  end

  def test_deep_reentry
    vm = ZippRb::VM.new
    vm.define_function("down") { |n| n <= 0 ? 0 : vm.eval("down(#{n - 1})") }
    assert_equal 0, vm.eval("down(25)")
  ensure
    vm.dispose
  end

  def test_survives_gc_compaction
    vm = ZippRb::VM.new
    vm.define_function("f") { |x| x * 2 }
    vm.eval("function g() { return 7 }")
    handle = vm.handle("g")
    GC.compact if GC.respond_to?(:compact)
    assert_equal 10, vm.eval("f(5)")
    assert_equal 7, handle.call
  ensure
    vm.dispose
  end
end
