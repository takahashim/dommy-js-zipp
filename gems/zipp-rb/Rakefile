# frozen_string_literal: true

require "bundler/gem_tasks"
require "rake/testtask"
require "rb_sys/extensiontask"

gemspec = Gem::Specification.load("zipp-rb.gemspec")

Rake::ExtensionTask.new("zipp_rb", gemspec) do |ext|
  ext.lib_dir = "lib/zipp_rb"
end

# Materialise the vendored engine (vendor/engine) from the vendor/zipp submodule
# before compiling or packaging, so the extension's path dependency exists and a
# built .gem is self-contained.
task :vendor_engine do
  sh "scripts/vendor-engine.sh"
end
Rake::Task[:compile].enhance([:vendor_engine]) if Rake::Task.task_defined?(:compile)
Rake::Task[:build].enhance([:vendor_engine]) if Rake::Task.task_defined?(:build)
task test: :vendor_engine

Rake::TestTask.new(:test) do |t|
  t.libs << "test" << "lib"
  t.test_files = FileList["test/**/*_test.rb"]
end

task default: %i[compile test]
