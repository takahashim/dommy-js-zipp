# frozen_string_literal: true

require "mkmf"
require "rb_sys/mkmf"

# Build the Rust cdylib and install it as lib/zipp_rb/zipp_rb.<ext>.
create_rust_makefile("zipp_rb/zipp_rb")
