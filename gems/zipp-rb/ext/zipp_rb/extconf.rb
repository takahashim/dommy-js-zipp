# frozen_string_literal: true

require "mkmf"
require "rb_sys/mkmf"

# The Rust extension path-depends on the in-tree engine, which both the
# checkout and a built gem carry. Fail with a clear message rather than a
# cargo error if it is missing (a broken package).
engine = File.expand_path("../../engine/crates/zipp-vm", __dir__)
abort "zipp-rb: the Zipp engine sources are missing (#{engine})." unless File.directory?(engine)

# Build the Rust cdylib and install it as lib/zipp_rb/zipp_rb.<ext>.
create_rust_makefile("zipp_rb/zipp_rb")
