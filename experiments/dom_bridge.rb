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

    globalThis.document = __wrapHost(0);
    globalThis.HTMLElement = function () {};
    globalThis.MutationObserver = function () { return { observe() {}, disconnect() {} }; };
  JS

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
      @document_id = 0
      @nodes[0] = Node.new(0, "#document")
    end

    def node(id) = @nodes[id]

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
