# frozen_string_literal: true

require "test_helper"
require_relative "../experiments/dom_bridge"

# Protects the host environment + DOM object bridge mechanism (see
# experiments/dom_bridge_poc.rb): JS mutating a Ruby-backed host object, Ruby
# reading it back, object identity, and event delivery via reentry.
class HostBridgeTest < Minitest::Test
  def setup
    @vm = ZippRb::VM.new
    @dom = DomBridge::MiniDom.new
    @dom.vm = @vm
    @vm.define_function("__host") { |op, id, key, value| @dom.handle(op, id, key, value) }
    @vm.eval(DomBridge::PREAMBLE)
  end

  def teardown
    @vm.dispose
  end

  def test_js_mutates_host_and_ruby_sees_it
    @vm.eval('const d = document.createElement("div"); d.className = "app"; d.textContent = "hi"; document.appendChild(d);')
    d = @dom.node(@dom.document_id).children.first
    assert_equal "div", d.tag
    assert_equal "app", d.attrs["class"]
    assert_equal "hi", d.text
  end

  def test_ruby_dispatches_event_and_js_listener_runs
    @vm.eval(<<~JS)
      const b = document.createElement("button");
      document.appendChild(b);
      b.addEventListener("click", (e) => { e.target.textContent = "hit"; });
    JS
    b = @dom.node(@dom.document_id).children.first
    assert_equal "button", b.tag
    @dom.dispatch(b.id, "click")
    assert_equal "hit", b.text
  end

  def test_identity_and_children_across_the_boundary
    @vm.eval(<<~JS)
      const r = document.createElement("section");
      const c = document.createElement("span");
      r.appendChild(c);
      document.appendChild(r);
    JS
    r = @dom.node(@dom.document_id).children.first
    assert_equal "section", r.tag
    assert_equal "span", r.children.first.tag
    # JS reads the same structure back through the bridge.
    assert_equal "SPAN", @vm.eval("document.children[0].children[0].tagName")
  end
end
