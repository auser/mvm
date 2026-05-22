import Foundation
import Virtualization

// Plan 97 Phase A — VZ machine config + lifecycle.
//
// Owns exactly one Linux guest. Constructs a
// `VZVirtualMachineConfiguration` from the parsed `SupervisorConfig`,
// validates it, starts the VM on a dedicated dispatch queue, writes the
// supervisor's PID under `vm_state_dir`, installs a SIGTERM handler that
// requests a graceful guest shutdown, and blocks the calling thread
// until the guest stops. Exit code mirrors the guest stop outcome.
//
// Pattern parallels `mvm-libkrun-supervisor`: one supervisor process
// per VM, no in-process registry. If the supervisor dies, the guest
// dies (Vz tears the VM down when its owning queue retains nothing).
//
// Out-of-band concerns wired by sibling files:
// - VsockProxy.swift — host-side unix sockets ↔ guest virtio-vsock
// - Network.swift — gvproxy file-handle attachment

enum SupervisorError: Error, CustomStringConvertible {
    case configValidation(String)
    case attachmentFailed(String, underlying: Error?)
    case startFailed(Error)
    case stopFailed(Error)
    case noVsockDevice
    case ioError(String, underlying: Error?)

    var description: String {
        switch self {
        case .configValidation(let msg):
            return "VZ configuration invalid: \(msg)"
        case .attachmentFailed(let what, let err):
            return "attachment failed for \(what)" + (err.map { ": \($0)" } ?? "")
        case .startFailed(let err):
            return "VM failed to start: \(err)"
        case .stopFailed(let err):
            return "VM stop returned error: \(err)"
        case .noVsockDevice:
            return "VM configured without a virtio-socket device"
        case .ioError(let what, let err):
            return "I/O failure (\(what))" + (err.map { ": \($0)" } ?? "")
        }
    }
}

final class Supervisor: NSObject, VZVirtualMachineDelegate {
    private let config: SupervisorConfig
    private let queue = DispatchQueue(label: "mvm.vz.supervisor", qos: .userInitiated)
    private let exitSignal = DispatchSemaphore(value: 0)
    private var vm: VZVirtualMachine?
    private var consoleFileHandle: FileHandle?
    private var vsockProxy: VsockProxy?

    // Set by VZ delegate callbacks; main thread reads after exitSignal.
    private var exitError: Error?

    init(config: SupervisorConfig) {
        self.config = config
        super.init()
    }

    /// Synchronous entry. Returns the process exit code: 0 on clean
    /// guest exit, 143 on SIGTERM-triggered stop, nonzero on error.
    func run() throws -> Int32 {
        try writePidFile()

        let vzConfig = try buildVZConfiguration()
        try vzConfig.validate()

        let vm = VZVirtualMachine(configuration: vzConfig, queue: queue)
        vm.delegate = self
        self.vm = vm

        // Bind vsock listeners *before* start so the guest agent can
        // dial them as soon as it boots. Plan 97 §"Guest communication
        // is still vsock" — the unix-socket convention under
        // `~/.mvm/run/<vm_id>/vsock/` is the contract `mvmctl` reads.
        try startVsockProxyIfNeeded(on: vm)

        installSignalHandler()

        let startResult = DispatchSemaphore(value: 0)
        var startError: Error?
        queue.async {
            vm.start { result in
                if case .failure(let err) = result {
                    startError = err
                }
                startResult.signal()
            }
        }
        startResult.wait()
        if let err = startError {
            try? removePidFile()
            throw SupervisorError.startFailed(err)
        }

        // Block until the VZVirtualMachineDelegate signals exit.
        exitSignal.wait()
        try? removePidFile()
        vsockProxy?.shutdown()

        if let err = exitError {
            FileHandle.standardError.write(
                Data("mvm-vz-supervisor: guest stopped with error: \(err)\n".utf8)
            )
            return 1
        }
        return 0
    }

    // MARK: - VZ configuration build

    private func buildVZConfiguration() throws -> VZVirtualMachineConfiguration {
        let vzConfig = VZVirtualMachineConfiguration()
        vzConfig.cpuCount = config.resources.cpuCount
        vzConfig.memorySize = config.resources.memoryBytes

        // Direct kernel boot. Plan 97 §"Boot loader locked to
        // VZLinuxBootLoader" — no EFI loader, smaller attack surface.
        let bootLoader = VZLinuxBootLoader(
            kernelURL: URL(fileURLWithPath: config.kernel.path)
        )
        bootLoader.commandLine = config.kernel.cmdline
        if let initrdPath = config.kernel.initrdPath {
            bootLoader.initialRamdiskURL = URL(fileURLWithPath: initrdPath)
        }
        vzConfig.bootLoader = bootLoader

        // Disks → virtio-blk in declared order: rootfs is `/dev/vda`,
        // overlay `/dev/vdc`, verity sidecar `/dev/vdd`, app-deps
        // volume next. Disk image format pinned to raw per Plan 97.
        var storageDevices: [VZStorageDeviceConfiguration] = []
        for disk in config.disks {
            let url = URL(fileURLWithPath: disk.path)
            let attachment: VZDiskImageStorageDeviceAttachment
            do {
                attachment = try VZDiskImageStorageDeviceAttachment(
                    url: url,
                    readOnly: disk.readOnly
                )
            } catch {
                throw SupervisorError.attachmentFailed(
                    "disk \(disk.id) (\(disk.path))",
                    underlying: error
                )
            }
            storageDevices.append(
                VZVirtioBlockDeviceConfiguration(attachment: attachment)
            )
        }
        vzConfig.storageDevices = storageDevices

        // virtio-fs shares (builder VM uses this for the Nix output).
        // Plan 97 §"Host-path mounts" — workload microVMs get zero
        // shares by default; only an admitted plan's named shares
        // appear here. Phase A doesn't gate on admission — that's
        // Phase B's job — but the supervisor faithfully relays
        // whatever the host JSON asks for.
        if !config.virtioFs.isEmpty {
            var directoryShares: [VZDirectorySharingDeviceConfiguration] = []
            for share in config.virtioFs {
                let single = VZSingleDirectoryShare(
                    directory: VZSharedDirectory(
                        url: URL(fileURLWithPath: share.hostPath),
                        readOnly: false
                    )
                )
                let device = VZVirtioFileSystemDeviceConfiguration(tag: share.tag)
                device.share = single
                directoryShares.append(device)
            }
            vzConfig.directorySharingDevices = directoryShares
        }

        // virtio-vsock. CID 3 is the Vz default for the first guest;
        // the host side will dial via the per-port unix sockets we
        // expose in VsockProxy.
        let vsockDevice = VZVirtioSocketDeviceConfiguration()
        vzConfig.socketDevices = [vsockDevice]

        // Console capture. Workload microVMs get capture-only: a file
        // for stdout, nothing on stdin. Interactive console
        // (PTY-over-vsock for dev mode) is on vsock ports 20000+,
        // handled by VsockProxy, not here. Plan 97 §"Console mode
        // lockdown".
        if let consolePath = config.consoleOutputPath {
            FileManager.default.createFile(atPath: consolePath, contents: nil)
            guard let fh = FileHandle(forWritingAtPath: consolePath) else {
                throw SupervisorError.ioError(
                    "open console log \(consolePath)",
                    underlying: nil
                )
            }
            self.consoleFileHandle = fh
            let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
            serial.attachment = VZFileHandleSerialPortAttachment(
                fileHandleForReading: nil,
                fileHandleForWriting: fh
            )
            vzConfig.serialPorts = [serial]
        }

        // Entropy. Plan 97 device table.
        vzConfig.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]

        // Network (optional). Phase A allows null for the simplest
        // boot; gvproxy wiring is filled in via Network.swift.
        if let network = config.network {
            let netConfig = try Network.makeAttachment(for: network)
            vzConfig.networkDevices = [netConfig]
        }

        // Balloon — Plan 97 §"Memory balloon floor".
        if let balloon = config.balloon, balloon.enabled {
            vzConfig.memoryBalloonDevices = [
                VZVirtioTraditionalMemoryBalloonDeviceConfiguration()
            ]
        }

        // Generic machine identifier — fresh per launch (ephemeral).
        // Plan 97 Security §10. Snapshot resume in Phase E will
        // persist this; Phase A does not.
        let platform = VZGenericPlatformConfiguration()
        platform.machineIdentifier = VZGenericMachineIdentifier()
        vzConfig.platform = platform

        return vzConfig
    }

    // MARK: - Vsock proxy

    private func startVsockProxyIfNeeded(on vm: VZVirtualMachine) throws {
        guard let socketDevice = vm.socketDevices.first as? VZVirtioSocketDevice else {
            throw SupervisorError.noVsockDevice
        }
        let proxy = VsockProxy(
            socketDevice: socketDevice,
            socketDir: config.vsock.socketDir,
            ports: config.vsock.ports,
            queue: queue
        )
        try proxy.start()
        self.vsockProxy = proxy
    }

    // MARK: - PID file

    private func writePidFile() throws {
        try ensureDirectory(config.vmStateDir)
        let pid = "\(ProcessInfo.processInfo.processIdentifier)\n"
        do {
            try pid.write(
                to: config.resolvedPidFile,
                atomically: true,
                encoding: .utf8
            )
        } catch {
            throw SupervisorError.ioError(
                "write pid file \(config.resolvedPidFile.path)",
                underlying: error
            )
        }
    }

    private func removePidFile() throws {
        try? FileManager.default.removeItem(at: config.resolvedPidFile)
    }

    private func ensureDirectory(_ path: String) throws {
        let url = URL(fileURLWithPath: path, isDirectory: true)
        try FileManager.default.createDirectory(
            at: url,
            withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700]
        )
    }

    // MARK: - SIGTERM forwarding

    private var sigtermSource: DispatchSourceSignal?

    private func installSignalHandler() {
        // Mask the default action so DispatchSourceSignal can fire.
        signal(SIGTERM, SIG_IGN)
        signal(SIGINT, SIG_IGN)
        let term = DispatchSource.makeSignalSource(signal: SIGTERM, queue: queue)
        term.setEventHandler { [weak self] in
            self?.requestGracefulStop()
        }
        term.resume()
        self.sigtermSource = term

        // SIGINT (Ctrl-C) treated the same when running interactively.
        let int_ = DispatchSource.makeSignalSource(signal: SIGINT, queue: queue)
        int_.setEventHandler { [weak self] in
            self?.requestGracefulStop()
        }
        int_.resume()
    }

    private func requestGracefulStop() {
        guard let vm else { return }
        // requestStop sends an ACPI power button press; the guest
        // gets a chance to shut down cleanly. If the guest ignores
        // it, the Vz framework still terminates the VM when the
        // process exits.
        do {
            try vm.requestStop()
        } catch {
            // Fall back to the harder stop API.
            vm.stop { _ in /* exitSignal is fired by delegate */ }
        }
    }

    // MARK: - VZVirtualMachineDelegate

    func guestDidStop(_ vm: VZVirtualMachine) {
        exitSignal.signal()
    }

    func virtualMachine(_ vm: VZVirtualMachine, didStopWithError error: Error) {
        self.exitError = error
        exitSignal.signal()
    }
}
