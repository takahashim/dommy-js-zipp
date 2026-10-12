# frozen_string_literal: true

# Child process for benchmarks/process_bench.rb: run ONE engine/workload and
# print a machine-readable result line. Not meant to be run directly, but:
#   mise exec ruby -- ruby -Ilib benchmarks/process_child.rb zipp 300

engine, iters = ARGV
iters = Integer(iters)

require "json"
require "zipp_rb" if engine == "zipp"
require "quickjs" if engine == "quickjs"

FIXTURES = "/Users/maki/git/dommy-js-quickjs/test/fixtures"
BOOT = "globalThis.self = globalThis; globalThis.process = { env: {} };"
REACT = File.read(File.join(FIXTURES, "react.umd.js"))
REACT_DOM_SERVER = File.read(File.join(FIXTURES, "react-dom-server.umd.js"))

RENDER_BODY = <<~JS
  ReactDOMServer.renderToString(
    React.createElement("div", { className: "app" },
      React.createElement("h1", null, "Hello"),
      React.createElement("ul", null,
        React.createElement("li", null, "one"),
        React.createElement("li", null, "two"),
        React.createElement("li", null, "three"))));
JS
DEFINE_RENDER = "globalThis.render = () => #{RENDER_BODY.strip};"

def now = Process.clock_gettime(Process::CLOCK_MONOTONIC)

start = now
if engine == "zipp"
  vm = ZippRb::VM.new
  vm.eval(BOOT)
  vm.eval(REACT)
  vm.eval(REACT_DOM_SERVER)
else
  vm = Quickjs::VM.new
  vm.eval_code(BOOT, async: false)
  vm.eval_code(REACT, async: false)
  vm.eval_code(REACT_DOM_SERVER, async: false)
end
loaded = now

if engine == "zipp"
  vm.eval(DEFINE_RENDER)
  output = vm.call("render")
  work_start = now
  iters.times { vm.call("render") }
else
  vm.eval_code(DEFINE_RENDER, async: false)
  output = vm.call("render")
  work_start = now
  iters.times { vm.call("render") }
end
work_end = now
vm.dispose if engine == "zipp"
vm.dispose! if engine == "quickjs"

puts "RESULT #{JSON.generate(
  engine: engine,
  iters: iters,
  load_ms: (loaded - start) * 1000,
  work_ms: (work_end - work_start) * 1000,
  output: output
)}"
