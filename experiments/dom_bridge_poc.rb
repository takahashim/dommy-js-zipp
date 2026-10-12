# frozen_string_literal: true

# PoC driver: host environment + DOM object bridge (see experiments/dom_bridge.rb).
#
#   mise exec ruby -- ruby -Ilib experiments/dom_bridge_poc.rb

require "zipp_rb"
require_relative "dom_bridge"

PASS = []

def assert(label, cond, detail = nil)
  if cond
    puts "  ok   #{label}"
    PASS << true
  else
    puts "  FAIL #{label}#{detail ? ": #{detail.inspect}" : ""}"
    PASS << false
  end
end

APP = <<~JS
  globalThis.__ready = false;
  const root = document.createElement("div");
  root.className = "app";
  root.id = "root";
  root.textContent = "hello";
  document.appendChild(root);

  const btn = document.createElement("button");
  btn.textContent = "click me";
  root.appendChild(btn);

  btn.addEventListener("click", (e) => { e.target.textContent = "clicked"; });
  globalThis.__ready = true;
JS

puts "== host environment + DOM bridge PoC"

vm = ZippRb::VM.new
dom = DomBridge::MiniDom.new
dom.vm = vm
vm.define_function("__host") { |op, id, key, value| dom.handle(op, id, key, value) }

vm.eval(DomBridge::PREAMBLE)
vm.eval(APP)

assert("browser globals present", vm.eval("typeof window") == "object" && vm.eval("typeof document") == "object")
assert("app ran through the bridge", vm.eval("globalThis.__ready") == true)

doc = dom.node(dom.document_id)
root = doc.children.first
btn = root.children.first
assert("Ruby sees the div JS created", root.tag == "div" && root.attrs["class"] == "app")
assert("Ruby sees text JS set", root.text == "hello")
assert("Ruby sees the button", btn.tag == "button" && btn.text == "click me")

assert("JS reads tagName back", vm.eval("document.children[0].children[0].tagName") == "BUTTON")
assert("JS reads children length", vm.eval("document.children[0].children.length") == 1)

dom.dispatch(btn.id, "click")
assert("event dispatched from Ruby ran the JS listener", btn.text == "clicked")
assert("JS sees the updated state afterwards", vm.eval("document.children[0].children[0].textContent") == "clicked")

puts "\n== real bundle on top of the environment"
vm.eval(File.read("/Users/maki/git/dommy-js-quickjs/test/fixtures/react.umd.js"))
assert("react.umd.js defines React", vm.eval("typeof React") == "object")
assert("React.createElement works", vm.eval("typeof React.createElement") == "function")

vm.dispose
puts "\nsummary: #{PASS.count(true)} passed, #{PASS.count(false)} failed"
exit(PASS.count(false).zero? ? 0 : 1)
