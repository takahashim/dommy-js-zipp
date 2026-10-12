# frozen_string_literal: true

# A minimal host environment + DOM object bridge for ZippRb, used by the PoC
# and its test. Ruby objects are exposed to JS as ordinary-looking objects: a
# JS-side Proxy routes property get/set and method calls back into Ruby via the
# structured host channel, and event listeners are held in JS (Ruby keeps only
# the callback id) so Ruby can deliver events by re-entering the same VM.
#
# This is deliberately small — a proof that the primitives compose into a
# quickjs.rb-style HostBridge, not a DOM implementation.

module DomBridge
  # Browser-ish globals and the Proxy bridge. `document` is host id 0.
  PREAMBLE = <<~JS
    globalThis.self = globalThis;
    globalThis.window = globalThis;
    if (typeof globalThis.navigator === "undefined") globalThis.navigator = { userAgent: "zipp-rb" };
    if (typeof globalThis.location === "undefined") {
      globalThis.location = { href: "http://localhost/", protocol: "http:", host: "localhost", pathname: "/" };
    }

    globalThis.__callbacks = {};
    globalThis.__cbSeq = 1;

    const METHODS = new Set([
      "createElement", "appendChild", "removeChild", "insertBefore",
      "setAttribute", "getAttribute", "removeAttribute",
      "addEventListener", "removeEventListener", "querySelector", "querySelectorAll",
      "remove",
    ]);

    // Identity of a host proxy lives in a WeakMap, never behind the proxy's own
    // get trap (an identity probe must not route into the host).
    const __ids = new WeakMap();

    function __wrapResult(v) {
      if (Array.isArray(v)) return v.map(__wrapResult);
      if (v && typeof v === "object" && typeof v.__host === "number") return __wrapHost(v.__host);
      return v;
    }
    function __marshallArg(a) {
      const id = a && typeof a === "object" ? __ids.get(a) : undefined;
      return id === undefined ? a : { __host: id };
    }
    function __wrapHost(id) {
      const handler = {
        get(t, prop) {
          if (typeof prop !== "string") return undefined;
          if (!METHODS.has(prop)) return __wrapResult(__host("get", id, prop));
          if (prop === "addEventListener") {
            return (type, cb) => {
              const cbId = globalThis.__cbSeq++;
              globalThis.__callbacks[cbId] = cb;
              return __wrapResult(__host("call", id, "addEventListener", [type, cbId]));
            };
          }
          return (...args) => __wrapResult(__host("call", id, prop, args.map(__marshallArg)));
        },
        set(t, prop, value) {
          __host("set", id, String(prop), __marshallArg(value));
          return true;
        },
      };
      const proxy = new Proxy({}, handler);
      __ids.set(proxy, id);
      return proxy;
    }

    globalThis.__dispatch = function (cbId, eventData) {
      const cb = globalThis.__callbacks[cbId];
      if (!cb) return;
      const ev = Object.assign({ type: "event" }, eventData || {});
      if (ev.target && typeof ev.target.__host === "number") ev.target = __wrapHost(ev.target.__host);
      return cb(ev);
    };

    // Timers delegate to the Ruby-driven event loop (`__timer`).
    globalThis.__timerCallbacks = {};
    globalThis.__timerSeq = 1;
    // NOTE: function declarations, not `globalThis.setTimeout = ...`: the
    // engine installs a native `setTimeout` as a PROGRAM global, which a bare
    // identifier read prefers over a global-object property. A function
    // declaration rebinds that global.
    function setTimeout(fn, delay, ...args) {
      const id = globalThis.__timerSeq++;
      globalThis.__timerCallbacks[id] = () => fn(...args);
      __timer("set", id, Math.max(0, Number(delay) || 0));
      return id;
    }
    function clearTimeout(id) {
      delete globalThis.__timerCallbacks[id];
      __timer("clear", id, 0);
    }
    function setInterval(fn, delay, ...args) {
      const id = globalThis.__timerSeq++;
      globalThis.__timerCallbacks[id] = () => fn(...args);
      __timer("setInterval", id, Math.max(1, Number(delay) || 1));
      return id;
    }
    function clearInterval(id) {
      delete globalThis.__timerCallbacks[id];
      __timer("clear", id, 0);
    }
    // Fires WITHOUT deleting: Ruby decides one-shot (setTimeout) vs repeating
    // (setInterval); a cancelled timer's callback is removed by clear*.
    globalThis.__fireTimer = function (id) {
      const cb = globalThis.__timerCallbacks[id];
      if (cb) cb();
    };

    globalThis.document = __wrapHost(0);
    globalThis.HTMLElement = function () {};
    globalThis.MutationObserver = function () { return { observe() {}, disconnect() {} }; };
  JS

  # Wire the JS bridge to a Ruby host: `__host` (DOM ops), `__timer` (timer
  # registration), and the preamble. Returns the host.
  def self.install(vm, dom)
    dom.vm = vm
    vm.define_function("__host") { |op, id, key, value| dom.handle(op, id, key, value) }
    vm.define_function("__timer") { |op, id, delay| dom.timer(op, id, delay) }
    vm.eval(PREAMBLE)
    dom
  end

  class Node
    attr_reader :id, :tag
    attr_accessor :text, :attrs, :children, :parent
    def initialize(id, tag)
      @id = id
      @tag = tag
      @text = ""
      @attrs = {}
      @children = []
      @parent = nil
    end
  end

  # A tiny document / element model. `#handle` is what the JS bridge calls.
  class MiniDom
    attr_writer :vm
    attr_reader :document_id

    def initialize
      @seq = 0
      @nodes = {}
      @events = Hash.new { |h, k| h[k] = [] }
      @timers = {}
      @document_id = 0
      @nodes[0] = Node.new(0, "#document")
    end

    def node(id) = @nodes[id]

    # Register / clear a timer (called by the JS timer shims). A repeating timer
    # records its interval; `run_timers` reschedules it.
    def timer(op, id, delay)
      seconds = delay.to_f / 1000.0
      case op
      when "set" then @timers[id] = { at: monotonic + seconds, interval: nil }
      when "setInterval"
        interval = [seconds, 0.001].max
        @timers[id] = { at: monotonic + interval, interval: interval }
      when "clear" then @timers.delete(id)
      end
      nil
    end

    # Fire due timers in due-time order, re-entering JS for each, until none
    # remain. `max` bounds total firings (so an interval cannot loop forever).
    # Returns how many fired.
    def run_timers(max: 10_000)
      fired = 0
      while fired < max
        now = monotonic
        due = @timers.select { |_, t| t[:at] <= now }.keys
        if due.empty?
          next_at = @timers.values.map { |t| t[:at] }.min
          break if next_at.nil?

          wait = next_at - now
          sleep(wait) if wait.positive?
          next
        end
        due.sort.each do |id|
          break if fired >= max

          entry = @timers[id]
          next unless entry

          if entry[:interval]
            entry[:at] += entry[:interval]
          else
            @timers.delete(id)
          end
          @vm.call("__fireTimer", id)
          @vm.drain_jobs!
          fired += 1
        end
      end
      fired
    end

    def handle(op, id, key, value)
      case op
      when "get" then get(id, key)
      when "set" then set(id, key, value)
      when "call" then call_method(id, key, value)
      end
    end

    # Deliver an event to the JS listeners registered on `node_id` (Ruby -> JS
    # reentry through `__dispatch`).
    def dispatch(node_id, type, extra = {})
      @events.fetch([node_id, type], []).each do |cb_id|
        @vm.call("__dispatch", cb_id, { "type" => type, "target" => { "__host" => node_id } }.merge(extra))
      end
    end

    private

    def monotonic = Process.clock_gettime(Process::CLOCK_MONOTONIC)

    def new_node(tag)
      @seq += 1
      node = Node.new(@seq, tag)
      @nodes[node.id] = node
      node
    end

    def get(id, key)
      n = node(id)
      case key
      when "tagName" then n.tag == "#document" ? "#document" : n.tag.upcase
      when "textContent" then n.text
      when "className" then n.attrs["class"].to_s
      when "id" then n.attrs.key?("id") ? n.attrs["id"] : n.id
      when "children" then n.children.map { |c| { "__host" => c.id } }
      when "parentNode" then n.parent ? { "__host" => n.parent.id } : nil
      else n.attrs[key]
      end
    end

    def set(id, key, value)
      n = node(id)
      case key
      when "textContent" then n.text = value.to_s
      when "className" then n.attrs["class"] = value.to_s
      else n.attrs[key] = value
      end
    end

    def call_method(id, method, args)
      n = node(id)
      case method
      when "createElement" then { "__host" => new_node(args[0].to_s).id }
      when "appendChild"
        child = node(args[0]["__host"])
        child.parent = n
        n.children << child
        nil
      when "removeChild"
        n.children.delete(node(args[0]["__host"]))
        nil
      when "insertBefore"
        child = node(args[0]["__host"])
        child.parent = n
        n.children.unshift(child)
        nil
      when "setAttribute" then n.attrs[args[0].to_s] = args[1].to_s
      when "getAttribute" then n.attrs[args[0].to_s]
      when "addEventListener" then @events[[n.id, args[0].to_s]] << args[1]
      when "querySelector" then nil
      when "querySelectorAll" then []
      end
    end
  end
end
