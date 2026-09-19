# API contracts

`openapi.json` is generated from the FastAPI application and is the source for
browser/client API types. Do not hand-edit it. Regenerate and verify it with:

```sh
PYTHONPATH=python python scripts/openapi.py contracts/openapi.json
PYTHONPATH=python python scripts/check_openapi.py
```

The contract describes the initial session lifecycle boundary and the
authenticated ticket used to open a profile-declared UART terminal. Display,
audio, and remote-device contracts will be added beside it as those
capabilities move.
