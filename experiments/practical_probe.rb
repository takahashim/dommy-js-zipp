# frozen_string_literal: true

# A practical-usage probe for ZippRb: run realistic JavaScript (real UMD
# bundles, a multi-module ESM graph, async, callbacks, modern syntax) and see
# what works and what does not. Not a test suite; an assessment.
#
#   mise exec ruby -- ruby -Ilib experiments/practical_probe.rb

require "zipp_rb"

FIXTURES = "/Users/maki/git/dommy-js-quickjs/test/fixtures"
$passed = 0
$failed = 0

def check(name)
  result = yield
  if result == true
    $passed += 1
    puts "  ok   #{name}"
  else
    $failed += 1
    puts "  FAIL #{name}: #{result.inspect}"
  end
rescue StandardError => e
  $failed += 1
  puts "  FAIL #{name}: #{e.class}: #{e.message.lines.first&.strip}"
end

def section(title)
  puts "\n== #{title}"
end

# ---------------------------------------------------------------------------
section "real UMD bundles (parse + run + global)"

def load_bundle(vm, file, global)
  src = File.read(File.join(FIXTURES, file))
  start = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  vm.eval(src)
  ms = (Process.clock_gettime(Process::CLOCK_MONOTONIC) - start) * 1000
  type = vm.eval("typeof #{global}")
  [ms, type]
end

{
  "react.umd.js" => "React",
  "htmx.umd.js" => "htmx",
  "alpine.umd.js" => "Alpine",
  "react-dom.umd.js" => "ReactDOM",
  "turbo.umd.js" => "Turbo",
  "stimulus.umd.js" => "Stimulus",
  "vue.global.js" => "Vue",
}.each do |file, global|
  vm = ZippRb::VM.new
  check("#{file} defines #{global}") do
    ms, type = load_bundle(vm, file, global)
    type == "object" || type == "function" ? true : "type=#{type} (#{ms.round(1)}ms)"
  end
  vm.dispose
end

# ---------------------------------------------------------------------------
section "multi-module ESM graph (re-export, relative imports)"

vm = ZippRb::VM.new
vm.set_module_provider("/app", {
  "/app/math.mjs" => "export const double = x => x * 2; export const triple = x => x * 3;",
  "/app/index.mjs" => "export { double } from './math.mjs'; export * as math from './math.mjs';",
  "/app/main.mjs" => "import { double, math } from './index.mjs'; export const r = double(21) + math.triple(2);",
})
check("re-export graph resolves") { vm.import("/app/main.mjs")["r"] == 48 }
vm.dispose

# ---------------------------------------------------------------------------
section "async / await / Promise.all / microtask draining"

vm = ZippRb::VM.new
check("await + Promise.all") do
  vm.eval(<<~JS)
    globalThis.__out = null;
    (async () => {
      const vals = await Promise.all([Promise.resolve(1), Promise.resolve(2), Promise.resolve(3)]);
      globalThis.__out = vals.reduce((a, b) => a + b, 0);
    })();
  JS
  vm.drain_jobs!
  vm.eval("globalThis.__out") == 6 ? true : vm.eval("globalThis.__out")
end
check("queueMicrotask order") do
  vm.eval("globalThis.__order = []; Promise.resolve().then(() => globalThis.__order.push(1)); globalThis.__order.push(0);")
  vm.drain_jobs!
  vm.eval("globalThis.__order") == [0, 1] ? true : vm.eval("globalThis.__order")
end
vm.dispose

# ---------------------------------------------------------------------------
section "modern syntax / builtins"

vm = ZippRb::VM.new
check("class + private field + static") do
  vm.eval(<<~JS) == 7 ? true : "got"
    class C { #x = 5; static make() { return new C(); } inc() { return ++this.#x; } }
    C.make().inc() + 1;
  JS
end
check("optional chaining / nullish") { vm.eval("({a:{b:null}})?.a?.b ?? 42") == 42 }
check("Map/Set/JSON/Math") do
  vm.eval("JSON.stringify({ m: Math.max(...[1,5,3]), s: [...new Set([1,1,2])] })") ==
    '{"m":5,"s":[1,2]}' ? true : vm.eval("JSON.stringify({ m: Math.max(...[1,5,3]), s: [...new Set([1,1,2])] })")
end
check("destructuring/spread/generators") { vm.eval("(() => { const [a,,b] = [1,2,3]; return a + b + [...'xy'].length; })()") == 6 }
vm.dispose

# ---------------------------------------------------------------------------
section "Ruby callbacks, reentry and handles (application bridge shape)"

vm = ZippRb::VM.new
state = { count: 0 }
vm.define_function("emit") { |event, payload| state[:count] += 1; "#{event}:#{payload}" }
check("JS calls Ruby with data, Ruby calls back into JS") do
  out = vm.eval(<<~JS)
    let seen = [];
    const push = (e) => { seen.push(emit(e, seen.length)); };
    push('a'); push('b');
    seen.join(',');
  JS
  out == "a:0,b:1" && state[:count] == 2 ? true : "#{out} / #{state.inspect}"
end
vm.eval("globalThis.listeners = []; globalThis.on = (fn) => listeners.push(fn);")
vm.eval("on((x) => x * 10);")
check("handle a JS callback and call it later") do
  h = vm.handle("__dummy") # not used; keep API exercise
  fn = vm.eval("(function (x) { return x * 10; })")
  # A function value is Opaque; to hold it we stash it on a global and handle that.
  vm.eval("globalThis.__fn = (x) => x * 10;")
  handle = vm.handle("__fn")
  handle.call(7) == 70 ? true : "got"
end
vm.dispose

# ---------------------------------------------------------------------------
section "gaps that matter for real applications"

vm = ZippRb::VM.new
check("setTimeout actually fires?") do
  vm.eval("globalThis.__t = 0; setTimeout(() => { globalThis.__t = 1; }, 0);")
  vm.drain_jobs!
  vm.eval("globalThis.__t") == 1 ? true : "present but never fires (no event loop / timer driver)"
end
check("fetch is present?") do
  vm.eval("typeof fetch") != "undefined" ? true : "undefined (no host IO)"
end
check("document is present?") do
  vm.eval("typeof document") != "undefined" ? true : "undefined (no DOM; host must provide)"
end
vm.dispose

puts "\nsummary: #{$passed} passed, #{$failed} failed"
