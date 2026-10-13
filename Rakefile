# frozen_string_literal: true

# Aggregate tasks for the monorepo. Each gem keeps its own Rakefile (and
# default task); these fan out to them.
GEMS = %w[zipp-rb dommy-js-zipp].freeze

GEMS.each do |name|
  desc "Compile and test #{name}"
  task "test:#{name}" do
    Dir.chdir(File.join(__dir__, "gems", name)) { sh "bundle exec rake" }
  end
end

desc "Compile and test every gem"
task test: GEMS.map { |name| "test:#{name}" }

desc "Run the Zipp engine's Rust unit tests"
task "engine:test" do
  Dir.chdir(File.join(__dir__, "gems", "zipp-rb")) { sh "bundle exec rake engine:test" }
end

task default: :test
