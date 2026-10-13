# frozen_string_literal: true

require "test_helper"

# The bridge's bundles are compiled once per process (zipp-rb's compile
# cache), not once per Runtime.
class Dommy::Js::TestCompileCache < Minitest::Test
  def test_after_prewarm_a_runtime_compiles_no_bundle
    Dommy::Js::Zipp.prewarm
    before = ZippRb.compile_cache_stats
    rt = Dommy::Js::Zipp::Runtime.new
    after = ZippRb.compile_cache_stats
    assert_equal before[:misses], after[:misses]
    assert_operator after[:hits] - before[:hits], :>=, Dommy::Js::HostBridge.constants.grep(/_JS\z/).size
    assert_equal 2, rt.evaluate("1 + 1")
  ensure
    rt&.dispose
  end
end
