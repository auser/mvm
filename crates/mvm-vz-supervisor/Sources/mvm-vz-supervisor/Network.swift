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
        case .gvproxy(let socketPath, let mac):
            return try makeGvproxyDevice(socketPath: socketPath, mac: mac)
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
}
