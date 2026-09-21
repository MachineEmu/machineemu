# MachineEmu catalog

Profiles live in `profiles/` and contain redistributable launch metadata only.
They refer to engine tracks, logical asset identifiers, resources, devices, and
policy. Firmware, prepared disks, credentials, and host-specific paths remain
external and must be imported through the asset boundary with content hashes.

A profile may be written as JSON (`.json`) or YAML (`.yaml`, `.yml`); the file
name is the profile ID, and the same ID must not appear in two formats. Reading
YAML needs the `yaml` extra.

`udm-pro-lab.json` is the first migrated device-research profile. It preserves
the current UDM Pro lab's required LCD, Bluetooth, bridge-network, and prepared
firmware inputs while leaving their operator-specific locations outside Git.
