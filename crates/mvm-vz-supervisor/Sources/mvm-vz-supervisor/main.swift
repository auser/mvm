import Foundation

// Plan 97 Phase A — `mvm-vz-supervisor` entry point.
//
// Single-purpose binary: read a SupervisorConfig JSON document from
// stdin, run one Linux guest via Virtualization.framework until it
// exits, exit with a status that reflects the guest outcome. One
// supervisor process per VM (no in-process VM registry); matches the
// `mvm-libkrun-supervisor` contract for `mvmctl`'s lifecycle code.
//
// Exit codes:
//   0   guest stopped cleanly
//   1   guest stopped with an error (the supervisor logs the error
//       string to stderr before exiting)
//   2   configuration could not be parsed
//   3   supervisor failed before the guest entered the running state
//
// Note: this file is named `main.swift` so its top-level code is the
// implicit entry point. `@main` and `main.swift` are mutually
// exclusive in Swift.

let json: Data
do {
    json = try FileHandle.standardInput.readToEnd() ?? Data()
} catch {
    FileHandle.standardError.write(
        Data("mvm-vz-supervisor: read stdin: \(error)\n".utf8)
    )
    exit(2)
}
if json.isEmpty {
    FileHandle.standardError.write(
        Data("mvm-vz-supervisor: empty stdin (expected SupervisorConfig JSON)\n".utf8)
    )
    exit(2)
}

let config: SupervisorConfig
do {
    let decoder = JSONDecoder()
    config = try decoder.decode(SupervisorConfig.self, from: json)
} catch {
    FileHandle.standardError.write(
        Data("mvm-vz-supervisor: parse SupervisorConfig: \(error)\n".utf8)
    )
    exit(2)
}

let supervisor = Supervisor(config: config)
do {
    let code = try supervisor.run()
    exit(code)
} catch {
    FileHandle.standardError.write(
        Data("mvm-vz-supervisor: \(error)\n".utf8)
    )
    exit(3)
}
