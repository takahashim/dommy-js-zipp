# frozen_string_literal: true

# Rough comparison of zipp-rb (this fork + binding) against quickjs.rb on a few
# representative operations. Not a rigorous benchmark: it exists to show the
# order of magnitude and to catch gross regressions, not to publish numbers.
#
#   mise exec ruby -- ruby -Ilib benchmarks/compare_to_quickjs.rb

require "quickjs"
require "zipp_rb"

N = Integer(ENV.fetch("N", "2000"))

def time_n(iterations)
  start = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  iterations.times { yield }
  Process.clock_gettime(Process::CLOCK_MONOTONIC) - start
end

def report(label, zipp, quickjs)
  ratio = zipp / quickjs
  printf("%-28s zipp %8.4fs   quickjs %8.4fs   zipp/quickjs %6.2fx\n", label, zipp, quickjs, ratio)
end

puts "N = #{N}"

# --- VM creation -------------------------------------------------------------
report(
  "VM.new + dispose",
  time_n(N) { ZippRb::VM.new.dispose },
  time_n(N) { Quickjs::VM.new.dispose! },
)

# --- short eval --------------------------------------------------------------
zipp_vm = ZippRb::VM.new
quickjs_vm = Quickjs::VM.new
report(
  "eval 1 + 2",
  time_n(N) { zipp_vm.eval("1 + 2") },
  time_n(N) { quickjs_vm.eval_code("1 + 2", async: false) },
)

# --- structured value conversion --------------------------------------------
report(
  "eval array/hash",
  time_n(N) { zipp_vm.eval("({ a: 1, b: [1, 2, 3], c: 'x' })") },
  time_n(N) { quickjs_vm.eval_code("({ a: 1, b: [1, 2, 3], c: 'x' })", async: false) },
)

# --- calling a JS function from Ruby ----------------------------------------
zipp_vm.eval("function inc(x) { return x + 1 }")
quickjs_vm.eval_code("function inc(x) { return x + 1 }", async: false)
report(
  "call JS function",
  time_n(N) { zipp_vm.call("inc", 41) },
  time_n(N) { quickjs_vm.call("inc", 41) },
)

# --- Ruby function called from JS -------------------------------------------
zipp_cb = ZippRb::VM.new
zipp_cb.define_function("rb_add") { |a, b| a + b }
quickjs_cb = Quickjs::VM.new
quickjs_cb.define_function("rb_add") { |a, b| a + b }
report(
  "JS->Ruby callback",
  time_n(N) { zipp_cb.eval("rb_add(2, 3)") },
  time_n(N) { quickjs_cb.eval_code("rb_add(2, 3)", async: false) },
)

zipp_vm.dispose
quickjs_vm.dispose!
zipp_cb.dispose
quickjs_cb.dispose!
