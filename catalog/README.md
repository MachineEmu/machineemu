# MachineEmu catalog

Profiles live in `profiles/` and contain redistributable launch metadata only.
They refer to engine tracks, logical asset identifiers, resources, devices, and
policy. Firmware, prepared disks, credentials, and host-specific paths remain
external and must be imported through the asset boundary with content hashes.

A profile may be written as JSON (`.json`) or YAML (`.yaml`, `.yml`); the file
name is the profile ID, and the same ID must not appear in two formats. Reading
YAML needs the `yaml` extra.

`devices.nic` names the NIC model and `devices.mac` may pin its address. A
profile that pins one is asserting an identity every instance launched from it
will share, so leave it out unless that is the intent: without it each instance
gets a stable address derived from its own name, and `machineemu run --mac`
overrides both for one run. QEMU's own default address is the same
52:54:00:12:34:56 for every guest, which two instances on one bridge cannot
both keep.

The guest's hostname is not the planner's to set: it comes from the cloud-init
seed, whose meta-data carries `local-hostname` and `instance-id`. A seed reused
across instances names them all the same, so build one per instance --
`vm-seed -H lab01 -i iid-lab01 seeds/lab01.iso` from vmmanager-sh -- and pass
it with `--seed`. The instance-id has to change too, or cloud-init treats the
boot as the same instance and skips the per-instance modules that apply the
hostname and keys.

Prepared images use the portable bundle format described in
[`docs/image-store.md`](../docs/image-store.md). Copy the complete bundle
directory, including `manifest.json` and its `components/` directory; do not
copy digest-named files from the internal workspace store by hand.

`udm-pro-lab.json` extends the UDM Pro boot wiring with WAN on `br0`,
SFP+ LAN on `br10`, an emulated LCD, and the H4 Bluetooth UART. Import it with
`--profile udm-pro-lab` to include the guest Bluetooth attachment hook.
The host simulator is started separately; see the boot instructions below.

`udm-pro.json` is the minimal Rust boot profile. See [UDM Pro boot](../docs/udm-pro.md)
for importing its prepared firmware and starting an offline instance.
