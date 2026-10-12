# frozen_string_literal: true

require_relative "lib/zipp_rb/version"

Gem::Specification.new do |spec|
  spec.name = "zipp-rb"
  spec.version = ZippRb::VERSION
  spec.authors = ["takahashim"]
  spec.email = ["takahashimm@gmail.com"]

  spec.summary = "Ruby binding for the Zipp JavaScript engine (prototype)."
  spec.description = <<~DESC
    zipp-rb embeds the Zipp JavaScript engine (a from-scratch Rust engine) in Ruby
    via a Magnus/Rust native extension. This is a Phase-0/1 prototype exploring a
    Rust glue layer as an alternative to the QuickJS-based quickjs.rb.
  DESC
  spec.homepage = "https://github.com/takahashim/zipp-rb"
  spec.license = "MIT"
  spec.required_ruby_version = ">= 3.2.0"

  spec.files = Dir.glob(%w[lib/**/* ext/**/* README.md], base: __dir__)
                 .select { |f| File.file?(File.join(__dir__, f)) }
  spec.require_paths = ["lib"]
  spec.extensions = ["ext/zipp_rb/extconf.rb"]

  # rb_sys is needed to build the Rust extension.
  spec.add_dependency "rb_sys", "~> 0.9"
end
