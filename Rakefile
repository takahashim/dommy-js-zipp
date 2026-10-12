# frozen_string_literal: true

require "bundler/gem_tasks"
require "rake/testtask"
require "rb_sys/extensiontask"

gemspec = Gem::Specification.load("zipp-rb.gemspec")

Rake::ExtensionTask.new("zipp_rb", gemspec) do |ext|
  ext.lib_dir = "lib/zipp_rb"
end

Rake::TestTask.new(:test) do |t|
  t.libs << "test" << "lib"
  t.test_files = FileList["test/**/*_test.rb"]
end

task default: %i[compile test]
