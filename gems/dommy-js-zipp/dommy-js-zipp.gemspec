# frozen_string_literal: true

require_relative "lib/dommy/js/zipp/version"

Gem::Specification.new do |spec|
  spec.name = "dommy-js-zipp"
  spec.version = Dommy::Js::Zipp::VERSION
  spec.authors = ["takahashim"]
  spec.email = ["takahashimm@gmail.com"]

  spec.summary = "Zipp backend for running JavaScript against a Dommy DOM."
  spec.description = <<~DESC
    dommy-js-zipp lets JavaScript drive a Dommy DOM on the Zipp engine (through
    zipp-rb), plugging into dommy's engine-agnostic HostBridge the way
    dommy-js-quickjs does for QuickJS.
  DESC
  spec.homepage = "https://github.com/takahashim/dommy-js-zipp/tree/main/gems/dommy-js-zipp"
  spec.metadata["source_code_uri"] = spec.homepage
  spec.license = "MIT"
  spec.required_ruby_version = ">= 3.2.0"

  spec.files = Dir.glob(%w[lib/**/* README.md LICENSE.txt], base: __dir__)
                  .select { |f| File.file?(File.join(__dir__, f)) }
  spec.require_paths = ["lib"]

  spec.add_dependency "zipp-rb", "~> 0.1"
  spec.add_dependency "dommy", ">= 0.16.0", "< 0.17"
end
