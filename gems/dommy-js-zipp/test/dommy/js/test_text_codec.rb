# frozen_string_literal: true

require "test_helper"

# TextEncoder and TextDecoder are the window's (Dommy's), which know every
# WHATWG encoding, not the engine's built-in pair (UTF-8 and UTF-16 only),
# whichever window model the runtime uses.
class Dommy::Js::TestTextCodec < Minitest::Test
  def runtime(unified_global:)
    rt = Dommy::Js::Zipp::Runtime.new(unified_global: unified_global)
    rt.install_window(Dommy.parse("<html><body></body></html>"))
    rt.install_browser_globals
    rt
  end

  def test_text_decoder_reads_legacy_encodings_in_both_window_models
    [true, false].each do |unified|
      rt = runtime(unified_global: unified)
      assert_equal %w[あ あ あ], rt.evaluate(<<~JS), "unified_global: #{unified}"
        [new TextDecoder().decode(new Uint8Array([0xe3, 0x81, 0x82])),
         new TextDecoder("shift_jis").decode(new Uint8Array([0x82, 0xa0])),
         new TextDecoder("euc-jp").decode(new Uint8Array([0xa4, 0xa2]))]
      JS
      assert_equal [227, 129, 130], rt.evaluate('Array.from(new TextEncoder().encode("あ"))')
      assert rt.evaluate("globalThis.TextDecoder === window.TextDecoder && globalThis.TextEncoder === window.TextEncoder")
    ensure
      rt&.dispose
    end
  end
end
