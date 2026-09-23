# Viewing and updating VM documents

The CLI prints YAML by default and accepts either YAML or JSON files:

```sh
machineemu show instance analysis01 > instance.yaml
machineemu update instance analysis01 --file instance.yaml

machineemu show profile malware-analysis-x64 > profile.yaml
machineemu update profile malware-analysis-x64 --file profile.yaml

machineemu show image win11-dev > image.yaml
machineemu update image win11-dev --file image.yaml
```

Add `--json` to `show` for JSON output. The API uses JSON by default, returns
YAML when `Accept: application/yaml` is sent, and accepts YAML on `PUT` with
`Content-Type: application/yaml` (or `text/yaml`). The routes are:

| Document | Read and replace |
| --- | --- |
| Instance | `GET` / `PUT /api/v2/instances/{id}/config` |
| Shared profile | `GET` / `PUT /api/v2/profiles/{id}` |
| Image manifest | `GET` / `PUT /api/v2/images/{id}` |

The instance document contains the immutable instance, image, and profile IDs,
the `auto_remove` policy, a `revision`, the instance's editable `profile`, and its saved
`launch_plan`. Stop the VM before updating it. The API rejects changes to the
IDs, policy, and disk/NVRAM/TPM preparation because those fields identify
existing writable state. Changes to the saved launch plan take effect on the
next start. SQLite commits the profile, launch plan and revision together.
`instances/{id}/profile.json` is a derived cache for helpers and inspection;
edit through the document API or CLI. Existing files are imported on the first
workspace open after upgrading. Explicit replanning reads the saved profile
from the daemon. A stale `revision` is rejected so another edit cannot be silently
overwritten. An instance created by an older client without a saved plan cannot
use this endpoint until it is configured with a launch plan.

Shared profile updates write a workspace override at
`profiles/{id}.json`; a catalog profile remains unchanged. Existing instances
keep their saved settings and launch plans. Image updates replace the editable
`images/{id}/manifest.json` after validating its ID and digest syntax. The
existing instance overlay keeps its original backing file; updated image
metadata applies when a new instance is created. The API stores canonical JSON
files while allowing YAML at the interface.
