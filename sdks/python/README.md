# mvm — Python SDK

Declarative microVM workloads for Python. Decorate a function or describe a
workload, and the `mvmforge` host CLI emits a Nix flake plus a launch plan
that [`mvm`](https://gomicrovm.com/) can boot.

```sh
pip install mvm
```

The host CLI (`mvmforge`, written in Rust) is distributed separately. The
Python SDK ships only the authoring + runtime surfaces that the host
subprocesses to.

## Quick start

```python
# app.py
import mvm as mv

@mv.func(
    name="adder",
    image=mv.nix_packages(["python312"]),
    resources=mv.resources(cpu_cores=1, memory_mb=256, rootfs_size_mb=512),
)
def add(a: int, b: int) -> int:
    return a + b
```

```sh
mvmforge emit app.py        # canonical IR
mvmforge compile app.py     # flake.nix + launch.json
mvmforge up app.py          # boot under mvm (dev only)
```

## Three surfaces

| Surface | Purpose |
| --- | --- |
| **Authoring** | `@mv.app`, `@mv.func`, `mv.workload(...)`, factories for image / network / resources / deps. |
| **Runtime** | `f.remote(...)` and `mv.session(...)` — host-side calls into a function-entrypoint VM. **Dev-only by design.** |
| **Sandbox** | `mv.Sandbox`, `Process`, `FileEntry` — typed lifecycle handles over local mvm sandbox primitives. **Dev-only.** |

The runtime SDK exists to assist build-time emission and dev-time
introspection. Production microVMs are observed via `mvmctl logs` and output
streams; no host-side `.remote()` calls.

## Optional extras

```sh
pip install 'mvm[schema]'   # pydantic-based schema auto-derivation
```

## Documentation

Full documentation: https://mvmforge-docs.pages.dev/sdks/python/

## License

Apache-2.0
