# frozen_string_literal: true

require "test_helper"

# Lifecycle and isolation: thread ownership, dispose during execution, and
# reclamation of the state the binding retains for a VM.
class LifecycleTest < Minitest::Test
  def test_vm_ids_are_unique_across_threads
    worker_vm = nil
    Thread.new { worker_vm = ZippRb::VM.new }.join
    main_vm = ZippRb::VM.new
    main_id = main_vm.instance_variable_get(:@id)
    worker_id = worker_vm.instance_variable_get(:@id)
    refute_equal main_id, worker_id
    main_vm.dispose
  end

  def test_using_a_vm_from_another_thread_is_refused
    vm = ZippRb::VM.new
    vm.eval("globalThis.tag = 'main'")
    error = nil
    Thread.new do
      vm.eval("globalThis.tag")
    rescue StandardError => e
      error = e
    end.join
    assert_instance_of ThreadError, error
    # The main-thread VM is untouched and still usable from its owner.
    assert_equal "main", vm.eval("globalThis.tag")
    vm.dispose
  end

  def test_two_threads_do_not_touch_each_others_vms
    # The reviewer's repro: a main VM reading "main" must not end up reading a
    # worker VM's "worker" when called cross-thread.
    main = ZippRb::VM.new
    main.eval("globalThis.tag = 'main'")
    seen = nil
    Thread.new do
      worker = ZippRb::VM.new
      worker.eval("globalThis.tag = 'worker'")
      seen = begin
        main.eval("globalThis.tag")
      rescue StandardError => e
        e.class
      end
      worker.dispose
    end.join
    assert_equal ThreadError, seen
    assert_equal "main", main.eval("globalThis.tag")
    main.dispose
  end

  def test_dispose_during_a_callback_is_refused
    vm = ZippRb::VM.new
    vm.define_function("stop") { vm.dispose }
    assert_raises(ThreadError) { vm.eval("stop()") }
    refute vm.disposed?, "the VM must survive a refused dispose"
    assert_equal 42, vm.eval("40 + 2"), "the VM stays usable afterwards"
    vm.dispose
  end

  # While a callback runs, the VM is out of the registry (its entry holds it),
  # but it is alive: a host that checks `disposed?` before re-entering must
  # not be told otherwise.
  def test_an_executing_vm_is_not_disposed
    vm = ZippRb::VM.new
    vm.define_function("probe") { vm.disposed? }
    assert_equal false, vm.eval("probe()")
    vm.dispose
  end

  def test_dispose_releases_registered_callbacks
    before = ZippRb::CALLBACKS.size
    3.times do |i|
      vm = ZippRb::VM.new
      vm.define_function("f#{i}") { i }
      vm.dispose
    end
    assert_equal before, ZippRb::CALLBACKS.size
  end

  def test_drop_releases_retained_state
    vm = ZippRb::VM.new
    vm.define_function("h") { 1 }
    vm.on_log { |_s, _m| nil }
    vm.set_module_provider("/app", { "/app/m.mjs" => "export const x = 1;" })
    id = vm.instance_variable_get(:@id)

    assert(ZippRb::CALLBACKS.keys.any? { |k| k.start_with?("rb:#{id}:") })
    assert(ZippRb::LISTENERS.key?("log:#{id}"))
    assert(ZippRb::MODULES.key?(id))

    ZippRb._drop(id)

    refute(ZippRb::CALLBACKS.keys.any? { |k| k.start_with?("rb:#{id}:") })
    refute(ZippRb::LISTENERS.key?("log:#{id}"))
    refute(ZippRb::MODULES.key?(id))
    assert vm.disposed?
  end

  def test_gc_reclaims_an_unreferenced_vm
    ids = []
    # Held until they are counted: a GC while the loop allocates would
    # otherwise reclaim the first ones before the check.
    held = []
    -> { 10.times { held << ZippRb::VM.new } }.call
    ids.concat(held.map { |vm| vm.instance_variable_get(:@id) })
    assert_equal 0, ids.count { |i| ZippRb._disposed?(i) }
    held.clear
    10.times do
      GC.start(full_mark: true, immediate_sweep: true)
      break if ids.all? { |i| ZippRb._disposed?(i) }
    end
    assert(ids.all? { |i| ZippRb._disposed?(i) }, "unreferenced VMs should be dropped by the finalizer")
  end

  def test_callback_arguments_survive_gc_stress
    vm = ZippRb::VM.new
    vm.define_function("sum") { |a, b, c| a + b + c }
    GC.stress = true
    assert_equal 6, vm.eval("sum(1, 2, 3)")
  ensure
    GC.stress = false
    vm.dispose
  end
end
