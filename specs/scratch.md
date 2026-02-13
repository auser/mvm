Do we have 100% test coverage? What do we need to test next?

---

Do we have to run `sudo firecracker`?

auser@lima-mvm:/Users/auser/work/personal/microvm/kv/mvm$ firecracker
2026-02-12T17:18:27.185606081 [anonymous-instance:main] Running Firecracker v1.14.1
2026-02-12T17:18:27.185710038 [anonymous-instance:main] RunWithApiError error: Failed to bind and run the HTTP server: IO error: Permission denied (os error 13)
Error: RunWithApi(FailedToBindAndRunHttpServer(IOError(Os { code: 13, kind: PermissionDenied, message: "Permission denied" })))
2026-02-12T17:18:27.185731205 [anonymous-instance:main] Firecracker exiting with error. exit_code=1
