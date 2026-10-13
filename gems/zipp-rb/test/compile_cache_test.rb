# frozen_string_literal: true

require "test_helper"

# VM#load(cache: true): a script compiled once per process, loaded into any VM.
class CompileCacheTest < Minitest::Test
  def setup
    @vms = []
  end

  def teardown
    @vms.each(&:dispose)
  end

  def vm(**opts)
    ZippRb::VM.new(timeout_msec: 5_000, **opts).tap { |v| @vms << v }
  end

  # Unique per test, so another test's entry is never what is hit.
  def source(body)
    "/* #{name} */\n#{body}"
  end

  def test_each_vm_runs_the_shared_compilation_with_its_own_globals
    src = source("var hits = 0; function bump() { return ++hits; }")
    a = vm
    b = vm
    before = ZippRb.compile_cache_stats
    a.load(src, cache: true)
    b.load(src, cache: true)
    after = ZippRb.compile_cache_stats
    assert_equal 1, after[:misses] - before[:misses]
    assert_equal 1, after[:hits] - before[:hits]

    a.eval("bump(); bump()")
    assert_equal 1, b.eval("bump()"), "a's calls leave b's globals alone"
  end

  def test_a_bridged_vm_loads_from_the_cache_too
    src = source("var viaHost = twice(21);")
    v = vm
    v.define_function("twice") { |n| n * 2 }
    v.load(src, cache: true)
    assert_equal 42, v.eval("viaHost")
  end

  def test_the_loading_vms_filename_names_the_stack_frames
    src = source("function fail() { throw new TypeError('bundle'); }")
    vm.load(src, filename: "https://example.test/first.js", cache: true)
    v = vm
    v.load(src, filename: "https://example.test/second.js", cache: true)
    err = assert_raises(ZippRb::TypeError) { v.eval("fail()") }
    assert_includes err.message, "bundle"
    stack = v.eval("try { fail() } catch (e) { e.stack }")
    assert_includes stack, "https://example.test/second.js"
    refute_includes stack, "first.js"
  end

  def test_a_syntax_error_is_reported_as_without_the_cache
    src = source("let = ;")
    uncached = assert_raises(ZippRb::SyntaxError) { vm.load(src) }
    cached = assert_raises(ZippRb::SyntaxError) { vm.load(src, cache: true) }
    assert_equal uncached.message, cached.message
  end

  def test_precompile_fills_the_cache_without_a_vm
    src = source("var pre = 'compiled ahead';")
    assert ZippRb.precompile(src)
    before = ZippRb.compile_cache_stats
    v = vm
    v.load(src, cache: true)
    assert_equal 1, ZippRb.compile_cache_stats[:hits] - before[:hits]
    assert_equal "compiled ahead", v.eval("pre")
    refute ZippRb.precompile(source("let = ;"))
  end

  def test_a_cached_script_can_be_loaded_from_another_thread
    src = source("var answer = 6 * 7;")
    vm.load(src, cache: true)
    result = Thread.new do
      v = ZippRb::VM.new(timeout_msec: 5_000)
      v.load(src, cache: true)
      v.eval("answer").tap { v.dispose }
    end.value
    assert_equal 42, result
  end
end
