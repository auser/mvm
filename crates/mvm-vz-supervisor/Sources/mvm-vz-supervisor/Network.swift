import Foundation
import Virtualization
import Darwin

// Plan 97 Phase A — gvproxy file-handle network attachment.
//
// On macOS, virtio-net is bridged to the host's NAT via gvproxy
// (ADR-055 §"Cross-platform backends" — passt is Linux-only). The
// supervisor opens a SOCK_DGRAM unix socket connected to gvproxy's
// `--listen-vfkit <path>` listener and hands the descriptor to Vz via
// `VZFileHandleNetworkDeviceAttachment(fileHandle:)`. From there Vz
// frames vfkit-protocol packets onto the virtio-net device the guest
// sees as eth0.
//
// No new frame parser lands here: parsing happens inside gvproxy (Go).
// The supervisor is a wire between Vz's device emulation and the
// gvproxy unix-datagram endpoint. ADR-055's threat model already
// covers gvproxy's input surface.
//
// Builder VMs that need outbound-only access (Nix substituter fetches)
// reuse the same gvproxy path; nat policy is decided host-side before
// the supervisor is invoked.

enum Network {
    static func makeAttachment(for config: NetworkConfig) throws -> VZNetworkDeviceConfiguration {
        switch config {
        case .gvproxy(let socketPath, let mac, let eventsIngestSocketPath):
            // Plan 102 W6.A.5 — when the Rust side wired up the
            // claim-10 audit substrate, `eventsIngestSocketPath` is
            // set. The bridged device interposes a SOCK_DGRAM
            // socketpair between Vz and gvproxy and emits FlowOpened
            // / FlowClosed NDJSON entries to the supervisor's ingest
            // socket. When `nil`, the legacy direct-attach path runs
            // (no audit fan-out — dev mode / pre-W6.A.5 callers).
            if let ingest = eventsIngestSocketPath {
                return try makeBridgedGvproxyDevice(
                    socketPath: socketPath,
                    mac: mac,
                    eventsIngestSocketPath: ingest
                )
            } else {
                return try makeGvproxyDevice(socketPath: socketPath, mac: mac)
            }
        }
    }

    private static func makeGvproxyDevice(
        socketPath: String,
        mac: MacAddress
    ) throws -> VZVirtioNetworkDeviceConfiguration {
        let fd = Darwin.socket(AF_UNIX, SOCK_DGRAM, 0)
        if fd < 0 {
            throw SupervisorError.ioError(
                "socket(AF_UNIX, SOCK_DGRAM) for gvproxy",
                underlying: POSIXErrno.current()
            )
        }

        // Bump send/receive buffers above the default (4 KiB) so a
        // single guest packet (up to MTU + framing overhead) survives
        // a backlog. 1 MiB matches what cloud-hypervisor and Apple's
        // own sample wire up.
        var bufSize: Int32 = 1024 * 1024
        let bufLen = socklen_t(MemoryLayout<Int32>.size)
        _ = withUnsafePointer(to: &bufSize) { ptr in
            Darwin.setsockopt(fd, SOL_SOCKET, SO_SNDBUF, ptr, bufLen)
        }
        _ = withUnsafePointer(to: &bufSize) { ptr in
            Darwin.setsockopt(fd, SOL_SOCKET, SO_RCVBUF, ptr, bufLen)
        }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = socketPath.utf8CString
        let maxPath = MemoryLayout.size(ofValue: addr.sun_path)
        if pathBytes.count > maxPath {
            Darwin.close(fd)
            throw SupervisorError.ioError(
                "gvproxy socket path too long: \(socketPath)",
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

        let connectResult = withUnsafePointer(to: &addr) { addrPtr -> Int32 in
            addrPtr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockPtr in
                Darwin.connect(fd, sockPtr, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        if connectResult != 0 {
            let err = POSIXErrno.current()
            Darwin.close(fd)
            throw SupervisorError.ioError(
                "connect() gvproxy at \(socketPath)",
                underlying: err
            )
        }

        let fileHandle = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
        let attachment = VZFileHandleNetworkDeviceAttachment(fileHandle: fileHandle)

        let device = VZVirtioNetworkDeviceConfiguration()
        device.attachment = attachment

        let macString = mac.asArray
            .map { String(format: "%02x", $0) }
            .joined(separator: ":")
        guard let vzMac = VZMACAddress(string: macString) else {
            throw SupervisorError.configValidation(
                "invalid MAC address for network attachment: \(macString)"
            )
        }
        device.macAddress = vzMac

        return device
    }

    // Plan 102 W6.A.5 — bridged gvproxy attachment that interposes
    // a SOCK_DGRAM socketpair between Vz and gvproxy, connects to
    // the Rust supervisor's events-ingest socket, sends the
    // `MVM_VZ_BRIDGE_V1\n` handshake, and emits FlowOpened on
    // first-packet-per-direction + FlowClosed("shutdown") on tear-
    // down. The bridge runs as background DispatchSourceRead tasks
    // pinned to the supervisor process lifetime; the supervisor's
    // exit() reaps them.
    private static func makeBridgedGvproxyDevice(
        socketPath: String,
        mac: MacAddress,
        eventsIngestSocketPath: String
    ) throws -> VZVirtioNetworkDeviceConfiguration {
        // 1. Build the inner SOCK_DGRAM socketpair. `vzFd` is handed
        //    to Vz via the file-handle attachment; `supFd` stays
        //    here for the bridge to recv/send on.
        var pair: [Int32] = [-1, -1]
        let pairResult = pair.withUnsafeMutableBufferPointer { ptr in
            Darwin.socketpair(AF_UNIX, SOCK_DGRAM, 0, ptr.baseAddress)
        }
        if pairResult != 0 {
            throw SupervisorError.ioError(
                "socketpair(AF_UNIX, SOCK_DGRAM) for bridged gvproxy",
                underlying: POSIXErrno.current()
            )
        }
        let vzFd = pair[0]
        let supFd = pair[1]

        // 2. Connect a second SOCK_DGRAM to gvproxy.
        let gvFd = Darwin.socket(AF_UNIX, SOCK_DGRAM, 0)
        if gvFd < 0 {
            Darwin.close(vzFd)
            Darwin.close(supFd)
            throw SupervisorError.ioError(
                "socket(AF_UNIX, SOCK_DGRAM) for gvproxy bridge leg",
                underlying: POSIXErrno.current()
            )
        }
        try bumpSocketBuffers(fd: vzFd)
        try bumpSocketBuffers(fd: supFd)
        try bumpSocketBuffers(fd: gvFd)
        try connectUnix(fd: gvFd, path: socketPath, contextLabel: "gvproxy")

        // 3. Connect the ingest stream and send the handshake.
        let ingestFd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        if ingestFd < 0 {
            Darwin.close(vzFd)
            Darwin.close(supFd)
            Darwin.close(gvFd)
            throw SupervisorError.ioError(
                "socket(AF_UNIX, SOCK_STREAM) for events-ingest",
                underlying: POSIXErrno.current()
            )
        }
        try connectUnix(fd: ingestFd, path: eventsIngestSocketPath, contextLabel: "events-ingest")
        let ingestHandle = FileHandle(fileDescriptor: ingestFd, closeOnDealloc: true)
        try ingestHandle.write(contentsOf: Data(VZ_BRIDGE_HANDSHAKE.utf8))

        // 4. Spawn the bridge worker. It owns supFd, gvFd, ingest.
        BridgeWorker.start(
            supFd: supFd,
            gvFd: gvFd,
            ingest: ingestHandle
        )

        // 5. Hand vzFd to Vz via the file-handle attachment.
        let fileHandle = FileHandle(fileDescriptor: vzFd, closeOnDealloc: true)
        let attachment = VZFileHandleNetworkDeviceAttachment(fileHandle: fileHandle)
        let device = VZVirtioNetworkDeviceConfiguration()
        device.attachment = attachment

        let macString = mac.asArray
            .map { String(format: "%02x", $0) }
            .joined(separator: ":")
        guard let vzMac = VZMACAddress(string: macString) else {
            throw SupervisorError.configValidation(
                "invalid MAC address for bridged network attachment: \(macString)"
            )
        }
        device.macAddress = vzMac

        return device
    }
}

/// Plan 102 W6.A.5 — Swift side of the
/// `mvm-supervisor::gateway_bridge::VZ_BRIDGE_HANDSHAKE` contract.
/// The Rust ingest task reads exactly these bytes before parsing
/// any NDJSON; a mismatch causes the connection to be rejected.
let VZ_BRIDGE_HANDSHAKE = "MVM_VZ_BRIDGE_V1\n"

/// Background bridge that:
///   - Reads datagrams from Vz (`supFd`), forwards to gvproxy (`gvFd`).
///   - Reads datagrams from gvproxy (`gvFd`), forwards to Vz (`supFd`).
///   - On first byte per direction, writes a FlowOpened NDJSON line
///     to `ingest`.
///   - On any I/O error / EOF, writes paired FlowClosed lines and
///     stops.
///
/// State is held in a single instance pinned by a module-level
/// reference so it outlives `makeBridgedGvproxyDevice`'s return.
/// Vz's supervisor process exit() reaps the dispatch sources.
final class BridgeWorker {
    /// Held in the module-level table so the worker outlives the
    /// `makeBridgedGvproxyDevice` stack frame. Vz holds the vzFd
    /// half; the worker holds supFd + gvFd + ingest and runs until
    /// the process exits.
    private static var live: [BridgeWorker] = []
    private static let liveLock = NSLock()

    private let supFd: Int32
    private let gvFd: Int32
    private let ingest: FileHandle
    private let ingestLock = NSLock()
    private var egressOpened = false
    private var ingressOpened = false
    private var egressClosed = false
    private var ingressClosed = false
    private let stateLock = NSLock()
    private let flowIdEgress: String
    private let flowIdIngress: String
    private var supSource: DispatchSourceRead?
    private var gvSource: DispatchSourceRead?

    private init(supFd: Int32, gvFd: Int32, ingest: FileHandle) {
        self.supFd = supFd
        self.gvFd = gvFd
        self.ingest = ingest
        // Stable per-direction flow IDs. Real flows would be 5-tuple
        // derived; here the bridge is one per VM so a single
        // "egress" + "ingress" stream is the modeled granularity.
        // UUIDs avoid collisions across concurrent VMs that share an
        // audit chain file.
        let uuid = UUID().uuidString
        self.flowIdEgress = "vz-egress-\(uuid)"
        self.flowIdIngress = "vz-ingress-\(uuid)"
    }

    static func start(supFd: Int32, gvFd: Int32, ingest: FileHandle) {
        let worker = BridgeWorker(supFd: supFd, gvFd: gvFd, ingest: ingest)
        liveLock.lock()
        live.append(worker)
        liveLock.unlock()
        worker.run()
    }

    /// Plan 102 W6.A.5 — test seam. Returns the worker so the test
    /// can `cancelForTesting` it explicitly; production callers use
    /// `start` and let the supervisor process's exit() reap the
    /// dispatch sources.
    static func startForTesting(supFd: Int32, gvFd: Int32, ingest: FileHandle) -> BridgeWorker {
        let worker = BridgeWorker(supFd: supFd, gvFd: gvFd, ingest: ingest)
        liveLock.lock()
        live.append(worker)
        liveLock.unlock()
        worker.run()
        return worker
    }

    /// Plan 102 W6.A.5 — test seam. Cancels both dispatch sources,
    /// which fires the cancel handler and emits paired flow_closed
    /// lines for any direction that was opened. Idempotent (cancel
    /// is a no-op on an already-cancelled source).
    func cancelForTesting() {
        supSource?.cancel()
        gvSource?.cancel()
    }

    private func run() {
        let queue = DispatchQueue(label: "mvm-vz-bridge", qos: .userInitiated)
        let supSource = DispatchSource.makeReadSource(fileDescriptor: supFd, queue: queue)
        let gvSource = DispatchSource.makeReadSource(fileDescriptor: gvFd, queue: queue)
        self.supSource = supSource
        self.gvSource = gvSource

        supSource.setEventHandler { [weak self] in
            self?.shuffle(from: self!.supFd, to: self!.gvFd, direction: .egress)
        }
        gvSource.setEventHandler { [weak self] in
            self?.shuffle(from: self!.gvFd, to: self!.supFd, direction: .ingress)
        }
        supSource.setCancelHandler { [weak self] in
            self?.markClosed(direction: .egress, reason: "bridge_error")
        }
        gvSource.setCancelHandler { [weak self] in
            self?.markClosed(direction: .ingress, reason: "bridge_error")
        }
        supSource.resume()
        gvSource.resume()
    }

    private enum Direction: String {
        case egress
        case ingress
    }

    private func shuffle(from src: Int32, to dst: Int32, direction: Direction) {
        var buf = [UInt8](repeating: 0, count: 65_536)
        let n = buf.withUnsafeMutableBufferPointer { ptr in
            Darwin.recv(src, ptr.baseAddress, ptr.count, 0)
        }
        if n <= 0 {
            // EOF or error — mark this direction closed.
            markClosed(direction: direction, reason: n == 0 ? "eof" : "bridge_error")
            return
        }
        // First-packet-per-direction → emit FlowOpened.
        markOpenedIfFirst(direction: direction)
        // Forward.
        _ = buf.withUnsafeBufferPointer { ptr in
            Darwin.send(dst, ptr.baseAddress, Int(n), 0)
        }
    }

    private func markOpenedIfFirst(direction: Direction) {
        stateLock.lock()
        let alreadyOpened: Bool
        switch direction {
        case .egress:
            alreadyOpened = egressOpened
            egressOpened = true
        case .ingress:
            alreadyOpened = ingressOpened
            ingressOpened = true
        }
        stateLock.unlock()
        if alreadyOpened { return }
        emitFlowOpened(direction: direction)
    }

    private func markClosed(direction: Direction, reason: String) {
        stateLock.lock()
        let alreadyClosed: Bool
        switch direction {
        case .egress:
            alreadyClosed = egressClosed
            egressClosed = true
        case .ingress:
            alreadyClosed = ingressClosed
            ingressClosed = true
        }
        stateLock.unlock()
        if alreadyClosed { return }
        emitFlowClosed(direction: direction, reason: reason)
    }

    private func emitFlowOpened(direction: Direction) {
        let flowId = direction == .egress ? flowIdEgress : flowIdIngress
        ingestWrite(formatFlowOpenedLine(flowId: flowId, direction: direction.rawValue))
    }

    private func emitFlowClosed(direction: Direction, reason: String) {
        let flowId = direction == .egress ? flowIdEgress : flowIdIngress
        ingestWrite(formatFlowClosedLine(
            flowId: flowId, direction: direction.rawValue, reason: reason
        ))
    }

    private func ingestWrite(_ line: String) {
        ingestLock.lock()
        defer { ingestLock.unlock() }
        // Best-effort write — if the Rust supervisor disconnects we
        // can't recover, but we shouldn't take down the data plane.
        try? ingest.write(contentsOf: Data(line.utf8))
    }
}

/// Bump SO_SNDBUF + SO_RCVBUF to 1 MiB so MTU-sized datagrams
/// survive a queueing burst. Mirrors the bump in `makeGvproxyDevice`.
private func bumpSocketBuffers(fd: Int32) throws {
    var bufSize: Int32 = 1024 * 1024
    let bufLen = socklen_t(MemoryLayout<Int32>.size)
    _ = withUnsafePointer(to: &bufSize) { ptr in
        Darwin.setsockopt(fd, SOL_SOCKET, SO_SNDBUF, ptr, bufLen)
    }
    _ = withUnsafePointer(to: &bufSize) { ptr in
        Darwin.setsockopt(fd, SOL_SOCKET, SO_RCVBUF, ptr, bufLen)
    }
}

/// Plan 102 W6.A.5 — JSON line formatters used by `BridgeWorker`
/// to write FlowEventWire entries to the ingest stream. Free
/// functions so XCTests can pin the exact wire shape without
/// reaching into BridgeWorker's private state.
func formatFlowOpenedLine(flowId: String, direction: String) -> String {
    #"{"kind":"flow_opened","flow_id":"\#(flowId)","direction":"\#(direction)"}"# + "\n"
}

func formatFlowClosedLine(flowId: String, direction: String, reason: String) -> String {
    #"{"kind":"flow_closed","flow_id":"\#(flowId)","direction":"\#(direction)","reason":"\#(reason)"}"# + "\n"
}

/// `connect()` an AF_UNIX socket to `path`. Closes `fd` and throws
/// on failure so the caller doesn't leak the fd on the error path.
/// `contextLabel` appears in the error message for diagnostics.
private func connectUnix(fd: Int32, path: String, contextLabel: String) throws {
    var addr = sockaddr_un()
    addr.sun_family = sa_family_t(AF_UNIX)
    let pathBytes = path.utf8CString
    let maxPath = MemoryLayout.size(ofValue: addr.sun_path)
    if pathBytes.count > maxPath {
        Darwin.close(fd)
        throw SupervisorError.ioError(
            "\(contextLabel) socket path too long: \(path)",
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
    let result = withUnsafePointer(to: &addr) { addrPtr -> Int32 in
        addrPtr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockPtr in
            Darwin.connect(fd, sockPtr, socklen_t(MemoryLayout<sockaddr_un>.size))
        }
    }
    if result != 0 {
        let err = POSIXErrno.current()
        Darwin.close(fd)
        throw SupervisorError.ioError(
            "connect() \(contextLabel) at \(path)",
            underlying: err
        )
    }
}
