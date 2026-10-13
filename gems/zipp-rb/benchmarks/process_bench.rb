# frozen_string_literal: true

# Separate-process benchmark: each measurement runs in a FRESH Ruby process
# (so the two engines do not share caches/GC/allocator state), alternating the
# order per repetition, and reports the median. It reports both the workload
# time and the whole-process wall time (which includes Ruby + extension load).
#
#   mise exec ruby -- ruby benchmarks/process_bench.rb
#
# Still lightweight (a few reps, one machine); a public claim would need more.

require "json"
require "rbconfig"

ITERS = Integer(ENV.fetch("N", "300"))
REPS = Integer(ENV.fetch("REPS", "5"))
ENGINES = %w[zipp quickjs].freeze
RUBY = RbConfig.ruby
ROOT = File.expand_path("..", __dir__)
CHILD = File.join(__dir__, "process_child.rb")

def median(xs)
  sorted = xs.sort
  mid = sorted.length / 2
  sorted.length.odd? ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2.0
end

def run_child(engine)
  start = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  out = IO.popen([RUBY, "-I#{File.join(ROOT, "lib")}", CHILD, engine, ITERS.to_s], &:read)
  wall_ms = (Process.clock_gettime(Process::CLOCK_MONOTONIC) - start) * 1000
  line = out.lines.find { |l| l.start_with?("RESULT ") } or
    abort("no RESULT from #{engine}:\n#{out}")
  JSON.parse(line.delete_prefix("RESULT "), symbolize_names: true).merge(wall_ms: wall_ms)
end

results = Hash.new { |h, k| h[k] = [] }
REPS.times do |rep|
  ENGINES.rotate(rep % 2).each { |engine| results[engine] << run_child(engine) }
end

def med(runs, key) = median(runs.map { |r| r[key] })

outputs = results.transform_values { |runs| runs.map { |r| r[:output] }.uniq }
puts "N = #{ITERS}  reps = #{REPS}  (median; each rep is a fresh process)"
puts "identical output across engines: #{outputs.values.flatten.uniq.size == 1}"
puts format("%-12s %10s %10s %12s", "", "load(ms)", "work(ms)", "process(ms)")
med = {}
results.each do |engine, runs|
  med[engine] = { load: med(runs, :load_ms), work: med(runs, :work_ms), wall: med(runs, :wall_ms) }
  printf("%-12s %10.2f %10.2f %12.2f\n", engine, med[engine][:load], med[engine][:work], med[engine][:wall])
end
puts
printf("zipp/quickjs  load %.2fx   work %.2fx   process %.2fx\n",
       med["zipp"][:load] / med["quickjs"][:load],
       med["zipp"][:work] / med["quickjs"][:work],
       med["zipp"][:wall] / med["quickjs"][:wall])
