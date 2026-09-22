# Migration records

The [Rust migration plan](rust.md) defines the next implementation around device
models, images, instances and runs, with no API backward-compatibility requirement.
Start implementation with the [first usable milestone](rust-first-milestone.md):
first directly port the existing YAML/JSON-to-QEMU launch planner, then deliver
one working Rust machine, safe state operations, a small browser flow and one
verified real-machine import. That plan narrows initial scope and takes precedence
for delivery order; the full design remains the destination architecture.
It is separate from the historical source-repository migration ledger below.

The migration ledger records every capability moved from `unifi-qemu`, its
target owner, disposition, evidence, and milestone. A source commit is not a
complete baseline while relevant work remains only in a dirty working tree.

The detailed ledger is [unifi-qemu.yaml](unifi-qemu.yaml). It is an initial
scope ledger captured from a dirty source checkout, not a claim that every row
has been ported. Each port commit should update the row status and link the
tests or other evidence that establish parity. Restricted assets, credentials,
build trees, and runtime directories remain outside this repository.
