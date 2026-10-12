# frozen_string_literal: true

# A representative, identical-workload comparison of zipp-rb against quickjs.rb:
# load React + ReactDOMServer and server-render a small tree many times. Both
# engines run the SAME bundle bytes and produce the same output.
#
#   mise exec ruby -- ruby -Ilib benchmarks/react_ssr_bench.rb
#
# Not a rigorous benchmark (single process, single run); it prices the glue on a
# realistic React workload.

require "quickjs"
require "zipp_rb"

FIXTURES = "/Users/maki/git/dommy-js-quickjs/test/fixtures"
REACT = File.read(File.join(FIXTURES, "react.umd.js"))
REACT_DOM_SERVER = File.read(File.join(FIXTURES, "react-dom-server.umd.js"))
BOOT = "globalThis.self = globalThis; globalThis.process = { env: {} };"

N = Integer(ENV.fetch("N", "500"))

def time
  start = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  yield
  Process.clock_gettime(Process::CLOCK_MONOTONIC) - start
end

RENDER = <<~JS
  ReactDOMServer.renderToString(
    React.createElement("div", { className: "app" },
      React.createElement("h1", null, "Hello"),
      React.createElement("ul", null,
        React.createElement("li", null, "one"),
        React.createElement("li", null, "two"),
        React.createElement("li", null, "three"))));
JS

# Define the render once, so a call measures execution (not a re-parse).
DEFINE_RENDER = "globalThis.render = () => #{RENDER.strip};"

report = []

# --- zipp-rb ---
zipp_load = nil
zipp_render = nil
zipp_vm = ZippRb::VM.new
zipp_load = time do
  zipp_vm.eval(BOOT)
  zipp_vm.eval(REACT)
  zipp_vm.eval(REACT_DOM_SERVER)
end
zipp_out = zipp_vm.eval(RENDER)
zipp_render = time { N.times { zipp_vm.eval(RENDER) } }
zipp_vm.eval(DEFINE_RENDER)
zipp_call = time { N.times { zipp_vm.call("render") } }
zipp_vm.dispose

# --- quickjs.rb ---
quickjs_vm = Quickjs::VM.new
quickjs_load = time do
  quickjs_vm.eval_code(BOOT, async: false)
  quickjs_vm.eval_code(REACT, async: false)
  quickjs_vm.eval_code(REACT_DOM_SERVER, async: false)
end
quickjs_out = quickjs_vm.eval_code(RENDER, async: false)
quickjs_render = time { N.times { quickjs_vm.eval_code(RENDER, async: false) } }
quickjs_vm.eval_code(DEFINE_RENDER, async: false)
quickjs_call = time { N.times { quickjs_vm.call("render") } }
quickjs_vm.dispose!

$stdout.sync = true
puts "N = #{N}"
puts "identical output: #{zipp_out == quickjs_out} (#{zipp_out.bytesize} bytes)"
puts format("%-26s zipp %8.4fs   quickjs %8.4fs   zipp/quickjs %6.2fx",
            "load React+ReactDOMServer", zipp_load, quickjs_load, zipp_load / quickjs_load)
puts format("%-26s zipp %8.4fs   quickjs %8.4fs   zipp/quickjs %6.2fx",
            "renderToString eval x#{N}", zipp_render, quickjs_render, zipp_render / quickjs_render)
puts format("%-26s zipp %8.4fs   quickjs %8.4fs   zipp/quickjs %6.2fx",
            "render() call x#{N}", zipp_call, quickjs_call, zipp_call / quickjs_call)
