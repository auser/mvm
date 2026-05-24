import Foundation
import Virtualization
import Darwin

// Plan 97 Phase E — control-socket IPC between the host and the
// running supervisor.
//
// The supervisor binds a `SOCK_STREAM` unix socket at
// `<vm_state_dir>/control.sock` mode 0700. Rust-side
// `mvm-backend::vz` connects, exchanges newline-framed text commands,
// and reads newline-framed responses on the same connection. Each
// command runs on the supervisor's main dispatch queue (where every
// VZ API call must originate), so commands serialize naturally.
//
// Protocol (one command per line, one response per line):
//
//   PAUSE              → "OK"     | "ERR <message>"
//   RESUME             → "OK"     | "ERR <message>"
//   STATUS             → "OK running" | "OK paused" | "OK stopped"
//   BALLOON <mib>      → "OK"     | "ERR <message>"      (target inflate)
//   SAVE <path>        → "OK"     | "ERR <message>"      (macOS 14+)
//   RESTORE <path>     → "ERR not yet implemented"       (deferred)
//
// Security: the socket file is chmod 0700 (W1.2 contract) so only the
// owning UID can dial. No authentication beyond filesystem perms —
// matches the host-trust boundary in ADR-002 §"Threat model".

final class ControlSocket {
    private let socketPath: String
    private let vm: VZVirtualMachine
    /// Memory the VM was configured with, in bytes. Stored separately
    /// because `VZVirtualMachine` doesn't expose `configuration` post-init;
    /// the supervisor holds the canonical value from the parsed config.
    private let memorySize: UInt64
    /// The VM's platform machine identifier, captured at config-build
    /// time. SAVE serializes its `dataRepresentation` into the
    /// sidecar `<snapshot_path>.machine-id` so RESTORE can re-apply
    /// the same identity to the new VM configuration (Plan 97
    /// Security §10).
    private let machineIdentifier: VZGenericMachineIdentifier?
    private let queue: DispatchQueue
    private var listenFd: Int32 = -1
    private var acceptSource: DispatchSourceRead?
    /// Accepted-client sources we hold so they're not deallocated mid-read.
    private var clientSources: [DispatchSourceRead] = []

    init(
        socketPath: String,
        vm: VZVirtualMachine,
        memorySize: UInt64,
        machineIdentifier: VZGenericMachineIdentifier?,
        queue: DispatchQueue
    ) {
        self.socketPath = socketPath
        self.vm = vm
        self.memorySize = memorySize
        self.machineIdentifier = machineIdentifier
        self.queue = queue
    }

    func start() throws {
        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        if fd < 0 {
            throw SupervisorError.ioError(
                "socket(AF_UNIX, SOCK_STREAM) for control",
                underlying: POSIXErrno.current()
            )
        }
        Darwin.unlink(socketPath)

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = socketPath.utf8CString
        let maxPath = MemoryLayout.size(ofValue: addr.sun_path)
        if pathBytes.count > maxPath {
            Darwin.close(fd)
            throw SupervisorError.ioError(
                "control socket path too long: \(socketPath)",
                underlying: nil
            )
        }
        withUnsafeMutablePointer(to: &addr.sun_path) { rawPtr in
            rawPtr.withMemoryRebound(to: CChar.self, capacity: maxPath) { cstr in
                for (i, b) in pathBytes.enumerated() {
                    cstr[i] = b
                }
            }
        }
        let bindResult = withUnsafePointer(to: &addr) { addrPtr -> Int32 in
            addrPtr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockPtr in
                Darwin.bind(fd, sockPtr, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        if bindResult != 0 {
            let err = POSIXErrno.current()
            Darwin.close(fd)
            throw SupervisorError.ioError(
                "bind() control socket \(socketPath)",
                underlying: err
            )
        }
        // mode 0700 on the socket file itself — only the owning UID can
        // dial. Matches the W1.2 / ADR-002 claim 1 contract.
        Darwin.chmod(socketPath, 0o700)

        if Darwin.listen(fd, 4) != 0 {
            let err = POSIXErrno.current()
            Darwin.close(fd)
            throw SupervisorError.ioError(
                "listen() control socket",
                underlying: err
            )
        }
        self.listenFd = fd

        let src = DispatchSource.makeReadSource(fileDescriptor: fd, queue: queue)
        src.setEventHandler { [weak self] in
            self?.handleAccept()
        }
        src.resume()
        self.acceptSource = src
    }

    func shutdown() {
        acceptSource?.cancel()
        acceptSource = nil
        if listenFd >= 0 {
            Darwin.close(listenFd)
            listenFd = -1
        }
        Darwin.unlink(socketPath)
        for cs in clientSources {
            cs.cancel()
        }
        clientSources.removeAll()
    }

    private func handleAccept() {
        let clientFd = Darwin.accept(listenFd, nil, nil)
        if clientFd < 0 {
            return
        }
        // One DispatchSource per client; reads on the main queue,
        // commands execute synchronously inline since they all need
        // the main queue anyway.
        let clientSrc = DispatchSource.makeReadSource(
            fileDescriptor: clientFd,
            queue: queue
        )
        var buffer = Data()
        clientSrc.setEventHandler { [weak self] in
            guard let self else {
                Darwin.close(clientFd)
                return
            }
            self.drainClient(fd: clientFd, buffer: &buffer)
        }
        clientSrc.setCancelHandler {
            Darwin.close(clientFd)
        }
        clientSrc.resume()
        clientSources.append(clientSrc)
    }

    private func drainClient(fd: Int32, buffer: inout Data) {
        var tmp = [UInt8](repeating: 0, count: 4096)
        let n = tmp.withUnsafeMutableBufferPointer { ptr -> Int in
            Darwin.read(fd, ptr.baseAddress, ptr.count)
        }
        if n <= 0 {
            // EOF or error → cancel the source via its cancel handler.
            // Find the source and cancel it.
            for src in clientSources where src.handle == UInt(bitPattern: Int(fd)) {
                src.cancel()
            }
            return
        }
        buffer.append(contentsOf: tmp.prefix(n))
        // Split on newlines; process complete lines.
        while let nlIndex = buffer.firstIndex(of: 0x0a) {
            let lineData = buffer.subdata(in: 0..<nlIndex)
            buffer.removeSubrange(0...nlIndex)
            let line = String(data: lineData, encoding: .utf8) ?? ""
            let response = handleCommand(line.trimmingCharacters(in: .whitespacesAndNewlines))
            let respBytes = (response + "\n").data(using: .utf8) ?? Data()
            // Best-effort write back — if the client hung up, no harm.
            _ = respBytes.withUnsafeBytes { ptr -> Int in
                if let base = ptr.baseAddress {
                    return Darwin.write(fd, base, ptr.count)
                }
                return 0
            }
        }
    }

    private func handleCommand(_ line: String) -> String {
        let parts = line.split(separator: " ", maxSplits: 1, omittingEmptySubsequences: false)
        let verb = parts.first.map(String.init) ?? ""
        let arg = parts.count > 1 ? String(parts[1]) : ""
        switch verb.uppercased() {
        case "PAUSE":
            return synchronousVZCall { sema, result in
                self.vm.pause { res in
                    if case .failure(let err) = res { result.error = "\(err)" }
                    sema.signal()
                }
            }
        case "RESUME":
            return synchronousVZCall { sema, result in
                self.vm.resume { res in
                    if case .failure(let err) = res { result.error = "\(err)" }
                    sema.signal()
                }
            }
        case "STATUS":
            switch vm.state {
            case .stopped: return "OK stopped"
            case .running: return "OK running"
            case .paused: return "OK paused"
            case .error: return "OK error"
            case .starting: return "OK starting"
            case .pausing: return "OK pausing"
            case .resuming: return "OK resuming"
            case .stopping: return "OK stopping"
            case .saving: return "OK saving"
            case .restoring: return "OK restoring"
            @unknown default: return "OK unknown"
            }
        case "BALLOON":
            guard let mib = UInt64(arg) else { return "ERR BALLOON expects a positive integer (MiB)" }
            guard let balloon = vm.memoryBalloonDevices.first
                as? VZVirtioTraditionalMemoryBalloonDevice
            else {
                return "ERR no traditional memory balloon device attached"
            }
            // `targetVirtualMachineMemorySize` is the bytes the guest
            // is allowed to use (i.e. `memorySize - inflated`). We
            // accept the inflate target as MiB and translate against
            // the configured total (stored at init since
            // VZVirtualMachine doesn't expose `configuration`).
            let totalBytes = self.memorySize
            let inflateBytes = mib * 1024 * 1024
            if inflateBytes > totalBytes {
                return "ERR BALLOON \(mib) MiB exceeds VM memory \(totalBytes / (1024 * 1024)) MiB"
            }
            balloon.targetVirtualMachineMemorySize = totalBytes - inflateBytes
            return "OK"
        case "SAVE":
            if #available(macOS 14.0, *) {
                if arg.isEmpty {
                    return "ERR SAVE requires a path argument"
                }
                let result = synchronousVZCall { sema, result in
                    let url = URL(fileURLWithPath: arg)
                    self.vm.saveMachineStateTo(url: url) { error in
                        if let error { result.error = "\(error)" }
                        sema.signal()
                    }
                }
                // Best-effort: write `<arg>.machine-id` alongside the
                // snapshot so RESTORE can re-apply the same machine
                // identifier. On a snapshot-save failure we skip the
                // sidecar (the snapshot blob may be partial). Sidecar
                // write failure does not fail the SAVE — RESTORE can
                // still proceed with a fresh identifier (machine-id
                // continuity is lost but the VM boots).
                if result == "OK", let id = self.machineIdentifier {
                    let sidecarPath = arg + ".machine-id"
                    do {
                        try id.dataRepresentation.write(
                            to: URL(fileURLWithPath: sidecarPath),
                            options: [.atomic]
                        )
                        // Tighten mode to 0600 — the identifier is
                        // small but binds to guest identity.
                        try? FileManager.default.setAttributes(
                            [.posixPermissions: NSNumber(value: 0o600)],
                            ofItemAtPath: sidecarPath
                        )
                    } catch {
                        FileHandle.standardError.write(
                            Data(
                                "mvm-vz-supervisor: SAVE machine-id sidecar write failed: \(error)\n"
                                    .utf8))
                    }
                }
                return result
            } else {
                return "ERR SAVE requires macOS 14+ (saveMachineStateTo)"
            }
        case "RESTORE":
            // RESTORE is a different supervisor startup mode — it
            // boots a fresh supervisor process with
            // `startup_mode: { kind: "restore", snapshot_path }` on
            // stdin instead of going through the control socket.
            // The control-socket RESTORE verb stays explicit so
            // existing clients that hit it get a clear redirection.
            return "ERR RESTORE is a supervisor startup mode, not a control-socket verb — "
                + "spawn a new supervisor with startup_mode={kind:restore,...} on stdin"
        case "":
            return "ERR empty command"
        default:
            return "ERR unknown verb: \(verb)"
        }
    }
}

/// Mutable holder so an inner closure can write back an error string
/// captured by the outer synchronous wrapper. Reference semantics keep
/// the write visible after the closure exits.
private final class CommandResult {
    var error: String?
}

/// Run an async VZ API call as if it were synchronous: block the
/// caller until the completion handler fires, then translate the
/// captured error (if any) into the line-protocol response.
///
/// The control queue is the same as the VM's main dispatch queue, so
/// calling this from inside a queue handler keeps the VZ API calls
/// on the queue Vz requires.
private func synchronousVZCall(
    _ body: (DispatchSemaphore, CommandResult) -> Void
) -> String {
    let sema = DispatchSemaphore(value: 0)
    let result = CommandResult()
    body(sema, result)
    sema.wait()
    if let err = result.error {
        return "ERR \(err)"
    }
    return "OK"
}
