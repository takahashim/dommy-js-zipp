# frozen_string_literal: true

require_relative "lib/zipp_rb/version"

Gem::Specification.new do |spec|
  spec.name = "zipp-rb"
  spec.version = ZippRb::VERSION
  spec.authors = ["takahashim"]
  spec.email = ["takahashimm@gmail.com"]

  spec.summary = "Ruby binding for the Zipp JavaScript engine."
  spec.description = <<~DESC
    zipp-rb embeds the Zipp JavaScript engine (a from-scratch Rust engine) in Ruby
    through a Magnus/rb-sys native extension. The engine is built from source with
    the gem. Its API follows quickjs.rb where the two overlap.
  DESC
  spec.homepage = "https://github.com/takahashim/dommy-js-zipp/tree/main/gems/zipp-rb"
  spec.metadata["source_code_uri"] = spec.homepage
  # All three apply, to different parts: zipp-rb's own code is MIT, the
  # engine sources it ships are Apache-2.0 (regress-fork: MIT or Apache-2.0),
  # and the engine's Unicode data is Unicode-3.0. See NOTICE. (RubyGems takes
  # no SPDX `AND` expression, so they are listed.)
  spec.licenses = ["MIT", "Apache-2.0", "Unicode-3.0"]
  spec.required_ruby_version = ">= 3.2.0"

  # The engine sources ship with the gem so the extension builds anywhere; its
  # Rust test suites and build output do not. A compiled extension left in
  # lib/ by `rake compile` is platform-specific and is rebuilt on install.
  spec.files = Dir.glob(%w[lib/**/* ext/**/* engine/**/* README.md LICENSE.txt NOTICE], base: __dir__)
                 .select { |f| File.file?(File.join(__dir__, f)) }
                 .reject { |f| f.match?(%r{\Aengine/crates/[^/]+/tests/|(\A|/)target/}) }
                 .reject { |f| f.match?(/\.(bundle|so|dll)\z/) }
  spec.require_paths = ["lib"]
  spec.extensions = ["ext/zipp_rb/extconf.rb"]

  # rb_sys is needed to build the Rust extension.
  spec.add_dependency "rb_sys", "~> 0.9"
end
