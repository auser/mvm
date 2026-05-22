import Foundation
import Virtualization
import Darwin

// Plan 97 Phase A — host-side vsock bridge.
//
// For each declared port the host wants to dial into the guest, we
// open a `SOCK_STREAM` unix listener at
// `<socketDir>/vsock-<port>.sock` mode 0700. When a host client
// connects, we dial the guest's matching vsock port via
// `VZVirtioSocketDevice.connect(toPort:)` and bidirectionally splice
// the two file descriptors. Connection lifetime tracks whichever side
// closes first.
//
// Same path convention `mvmctl` already speaks for the libkrun
// backend (`crates/mvm-libkrun/`'s `vsock_socket_path`); the rest of
// the host-side code is hypervisor-agnostic.

final class VsockProxy {
    private let socketDevice: VZVirtioSocketDevice
    private let socketDir: String
    private let ports: [UInt32]
    private let queue: DispatchQueue
    private var listeners: [PortListener] = []

    init(
        socketDevice: VZVirtioSocketDevice,
        socketDir: String,
        ports: [UInt32],
        queue: DispatchQueue
    ) {
        self.socketDevice = socketDevice
        self.socketDir = socketDir
        self.ports = ports
        self.queue = queue
    }

    func start() throws {
        try ensureSocketDir()
        for port in ports {
            let listener = try PortListener(
                port: port,
                socketDir: socketDir,
                queue: queue,
                socketDevice: socketDevice
            )
            listener.start()
            listeners.append(listener)
        }
    }

    func shutdown() {
        for listener in listeners {
            listener.shutdown()
        }
        listeners.removeAll()
    }

    private func ensureSocketDir() throws {
        let url = URL(fileURLWithPath: socketDir, isDirectory: true)
        try FileManager.default.createDirectory(
            at: url,
            withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700]
        )
    }
}

// MARK: - Per-port listener

private final class PortListener {
    let port: UInt32
    let socketPath: String
    private let queue: DispatchQueue
    private let socketDevice: VZVirtioSocketDevice
    private var listenFd: Int32
    private var acceptSource: DispatchSourceRead?
    private var bridges: [Bridge] = []

    init(
        port: UInt32,
        socketDir: String,
        queue: DispatchQueue,
        socketDevice: VZVirtioSocketDevice
    ) throws {
        self.port = port
        self.queue = queue
        self.socketDevice = socketDevice
        self.socketPath = (socketDir as NSString)
            .appendingPathComponent("vsock-\(port).sock")
        self.listenFd = -1

        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        if fd < 0 {
            throw SupervisorError.ioError(
                "socket() for vsock \(port)",
                underlying: POSIXErrno.current()
            )
        }
        // Best-effort unlink; if it fails because the socket doesn't
        // exist (`ENOENT`) we ignore it.
        Darwin.unlink(socketPath)

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = socketPath.utf8CString
        let maxPath = MemoryLayout.size(ofValue: addr.sun_path)
        if pathBytes.count > maxPath {
            Darwin.close(fd)
            throw SupervisorError.ioError(
                "vsock socket path too long: \(socketPath)",
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
                "bind() vsock \(port) at \(socketPath)",
                underlying: err
            )
        }

        // mode 0700 on the socket file itself — matches the
        // libkrun-side W1.2 contract. setsockopt(SO_RCVTIMEO) etc.
        // intentionally omitted; the bridge handles read EOFs.
        Darwin.chmod(socketPath, 0o700)

        if Darwin.listen(fd, 8) != 0 {
            let err = POSIXErrno.current()
            Darwin.close(fd)
            throw SupervisorError.ioError(
                "listen() vsock \(port)",
                underlying: err
            )
        }

        self.listenFd = fd
    }

    func start() {
        let src = DispatchSource.makeReadSource(
            fileDescriptor: listenFd,
            queue: queue
        )
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
        for bridge in bridges {
            bridge.close()
        }
        bridges.removeAll()
    }

    private func handleAccept() {
        let clientFd = Darwin.accept(listenFd, nil, nil)
        if clientFd < 0 {
            return
        }
        socketDevice.connect(toPort: port) { [weak self] result in
            guard let self else {
                Darwin.close(clientFd)
                return
            }
            switch result {
            case .success(let connection):
                let bridge = Bridge(
                    hostFd: clientFd,
                    guestConnection: connection,
                    queue: self.queue
                )
                bridge.start()
                self.bridges.append(bridge)
            case .failure(let error):
                FileHandle.standardError.write(
                    Data("mvm-vz-supervisor: vsock connect to port \(self.port) failed: \(error)\n".utf8)
                )
                Darwin.close(clientFd)
            }
        }
    }
}

// MARK: - Bidirectional fd bridge

private final class Bridge {
    private let hostFd: Int32
    private let guestConnection: VZVirtioSocketConnection
    private let guestFd: Int32
    private let queue: DispatchQueue
    private var hostChannel: DispatchIO?
    private var guestChannel: DispatchIO?
    private var closed = false

    init(
        hostFd: Int32,
        guestConnection: VZVirtioSocketConnection,
        queue: DispatchQueue
    ) {
        self.hostFd = hostFd
        self.guestConnection = guestConnection
        // VZVirtioSocketConnection exposes a POSIX fd; dup it so
        // closing the channel doesn't poison the underlying
        // VZ-managed connection lifecycle. We close our dup'd fd
        // when the bridge tears down; VZ owns the original.
        self.guestFd = Darwin.dup(guestConnection.fileDescriptor)
        self.queue = queue
    }

    func start() {
        hostChannel = DispatchIO(
            type: .stream,
            fileDescriptor: hostFd,
            queue: queue
        ) { _ in
            Darwin.close(self.hostFd)
        }
        guestChannel = DispatchIO(
            type: .stream,
            fileDescriptor: guestFd,
            queue: queue
        ) { _ in
            Darwin.close(self.guestFd)
        }
        hostChannel?.setLimit(lowWater: 1)
        guestChannel?.setLimit(lowWater: 1)
        pump(from: hostChannel!, to: guestChannel!)
        pump(from: guestChannel!, to: hostChannel!)
    }

    func close() {
        guard !closed else { return }
        closed = true
        hostChannel?.close(flags: .stop)
        guestChannel?.close(flags: .stop)
    }

    private func pump(from src: DispatchIO, to dst: DispatchIO) {
        src.read(offset: 0, length: Int.max, queue: queue) { [weak self] done, data, error in
            guard let self else { return }
            if let data, !data.isEmpty {
                dst.write(
                    offset: 0,
                    data: data,
                    queue: self.queue
                ) { _, _, _ in /* ignore write errors; close on read EOF */ }
            }
            if done || error != 0 {
                self.close()
            }
        }
    }
}

// MARK: - Errno helper

struct POSIXErrno: Error, CustomStringConvertible {
    let code: Int32

    static func current() -> POSIXErrno { POSIXErrno(code: errno) }

    var description: String {
        let cstr = strerror(code)
        let msg = cstr.map { String(cString: $0) } ?? "errno \(code)"
        return "\(msg) (errno=\(code))"
    }
}
