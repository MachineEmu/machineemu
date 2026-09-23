# Rust migration

The [Rust migration plan](rust.md) defines the next implementation around device
models, images, instances and runs, with no API backward-compatibility requirement.
Start implementation with the [first usable milestone](rust-first-milestone.md):
first directly port the existing YAML/JSON-to-QEMU launch planner, then deliver
one working Rust machine, safe state operations, a small browser flow and one
verified real-machine import. That plan narrows initial scope and takes precedence
for delivery order; the full design remains the destination architecture.

The [current crate map](../../crates/README.md) documents the implemented Rust
boundaries, build commands and remaining migration work.
