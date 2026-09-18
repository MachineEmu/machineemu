# Migration records

The migration ledger records every capability moved from `unifi-qemu`, its
target owner, disposition, evidence, and milestone. A source commit is not a
complete baseline while relevant work remains only in a dirty working tree.

The detailed ledger is [unifi-qemu.yaml](unifi-qemu.yaml). It is an initial
scope ledger captured from a dirty source checkout, not a claim that every row
has been ported. Each port commit should update the row status and link the
tests or other evidence that establish parity. Restricted assets, credentials,
build trees, and runtime directories remain outside this repository.
