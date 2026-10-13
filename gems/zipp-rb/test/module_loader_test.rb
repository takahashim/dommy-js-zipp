# frozen_string_literal: true

require "test_helper"
require "uri"

class ModuleLoaderTest < Minitest::Test
  SOURCES = {
    "https://example.test/app/main.js" => <<~JS,
      import { greet } from "./lib/greet.js";
      import { VERSION } from "pkg";
      export const message = greet("zipp") + " " + VERSION;
      export const url = import.meta.url;
    JS
    "https://example.test/app/lib/greet.js" => <<~JS,
      import { exclaim } from "../util.js";
      export function greet(name) { return exclaim("hi " + name); }
    JS
    "https://example.test/app/util.js" => "export const exclaim = (s) => s + '!';",
    "https://cdn.test/pkg@1/index.js" => "export const VERSION = 'v1';"
  }.freeze

  def setup
    @vm = ZippRb::VM.new(timeout_msec: 5_000)
    @calls = []
    @vm.module_loader = lambda do |specifier, importer|
      @calls << [specifier, importer]
      url = specifier == "pkg" ? "https://cdn.test/pkg@1/index.js" : resolve(specifier, importer)
      { code: SOURCES.fetch(url), as: url } if SOURCES.key?(url)
    end
  end

  def teardown
    @vm.dispose
  end

  def resolve(specifier, importer)
    importer ? URI.join(importer, specifier).to_s : specifier
  end

  def test_imports_resolve_against_the_importing_module
    ns = @vm.import("https://example.test/app/main.js")
    assert_equal "hi zipp! v1", ns["message"]
    assert_includes @calls, ["./lib/greet.js", "https://example.test/app/main.js"]
    assert_includes @calls, ["../util.js", "https://example.test/app/lib/greet.js"]
    assert_includes @calls, ["pkg", "https://example.test/app/main.js"]
    assert_includes @calls, ["https://example.test/app/main.js", nil]
  end

  def test_import_meta_url_is_the_module_name
    assert_equal "https://example.test/app/main.js", @vm.import("https://example.test/app/main.js")["url"]
  end

  def test_a_module_is_loaded_once_per_canonical_name
    @vm.load("globalThis.loads = 0;")
    sources = { "a" => "globalThis.loads++; export const a = 1;" }
    @vm.module_loader = ->(specifier, _importer) { { code: sources.fetch("a"), as: "a" } if %w[a alias].include?(specifier) }
    @vm.import("a")
    @vm.import("alias")
    assert_equal 1, @vm.eval("loads")
  end

  def test_as_without_code_lands_on_a_loaded_module
    @vm.module_loader = lambda do |specifier, _importer|
      case specifier
      when "real" then { code: "export const v = 42;", as: "real" }
      when "alias" then { as: "real" }
      end
    end
    @vm.import("real")
    assert_equal 42, @vm.import("alias")["v"]
  end

  def test_a_string_answer_is_the_source_of_the_specifier
    @vm.module_loader = ->(specifier, _importer) { "export const s = #{specifier.dump};" if specifier == "x" }
    assert_equal "x", @vm.import("x")["s"]
  end

  def test_an_unknown_module_is_not_found
    error = assert_raises(ZippRb::Error) { @vm.import("https://example.test/missing.js") }
    assert_equal "module not found: https://example.test/missing.js", error.message
  end

  def test_dynamic_import_inside_a_module_resolves_against_it
    @vm.module_loader = lambda do |specifier, importer|
      case [specifier, importer]
      in ["https://example.test/dyn.js", nil] then { code: "export const p = import('./target.js');", as: specifier }
      in ["./target.js", "https://example.test/dyn.js"] then { code: "export const t = 7;", as: "https://example.test/target.js" }
      else nil
      end
    end
    @vm.load("globalThis.got = null; import('https://example.test/dyn.js').then(ns => ns.p).then(t => { globalThis.got = t.t; });")
    @vm.drain_jobs!
    assert_equal 7, @vm.eval("got")
  end
end
