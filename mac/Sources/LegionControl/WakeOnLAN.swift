import Darwin
import Foundation

/// Wake-on-LAN, plus a plain TCP probe to tell when the machine is back. Everything here is done
/// with sockets directly so the app does not depend on a helper script, and every address it uses
/// comes from the machine's own `wake` block in the config.
enum WakeOnLAN {
    /// How many copies of the packet go to each address and port. A magic packet is a datagram, so
    /// one that is dropped is simply gone.
    static let repeats = 3

    /// Sends the magic packet. Returns nil on success, or a sentence describing what went wrong.
    static func sendMagicPackets(_ wake: WakeConfig) async -> String? {
        await withCheckedContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                continuation.resume(returning: sendMagicPacketsBlocking(wake))
            }
        }
    }

    /// Polls the configured probe address until the box answers or the cap runs out. A machine with
    /// no probe cannot be waited for, only woken, so this says so at once rather than sitting there.
    static func waitForProbe(_ wake: WakeConfig, timeout: TimeInterval) async -> Bool {
        guard let probe = wake.probe else { return false }
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if await canConnect(host: probe.host, port: probe.probePort, timeout: 3) { return true }
            if Task.isCancelled { return false }
            try? await Task.sleep(for: .seconds(1))
        }
        return false
    }

    static func canConnect(host: String, port: UInt16, timeout: TimeInterval) async -> Bool {
        await withCheckedContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                continuation.resume(returning: canConnectBlocking(host: host, port: port, timeout: timeout))
            }
        }
    }

    // MARK: - Packet

    static func magicPacket(for bytes: [UInt8]) -> Data {
        var packet = Data(repeating: 0xFF, count: 6)
        for _ in 0..<16 { packet.append(contentsOf: bytes) }
        return packet
    }

    private static func sendMagicPacketsBlocking(_ wake: WakeConfig) -> String? {
        guard let bytes = wake.macBytes else {
            return "The configured hardware address is not valid."
        }
        let packet = magicPacket(for: bytes)

        let handle = socket(AF_INET, SOCK_DGRAM, 0)
        guard handle >= 0 else {
            return "Could not open a UDP socket: \(errorText())."
        }
        defer { close(handle) }

        var enabled: Int32 = 1
        guard setsockopt(handle, SOL_SOCKET, SO_BROADCAST, &enabled, socklen_t(MemoryLayout<Int32>.size)) == 0 else {
            return "Could not enable broadcast on the socket: \(errorText())."
        }

        var delivered = 0
        var lastError: String?
        for address in wake.broadcast {
            guard var destination = socketAddress(host: address, port: 0) else {
                lastError = "Could not parse the broadcast address \(address)."
                continue
            }
            for port in wake.ports {
                destination.sin_port = port.bigEndian
                for _ in 0..<repeats {
                    let sent = packet.withUnsafeBytes { buffer -> Int in
                        withUnsafePointer(to: &destination) { pointer in
                            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { generic in
                                sendto(handle, buffer.baseAddress, buffer.count, 0,
                                       generic, socklen_t(MemoryLayout<sockaddr_in>.size))
                            }
                        }
                    }
                    if sent < 0 {
                        lastError = "Sending to \(address) port \(port) failed: \(errorText())."
                    } else {
                        delivered += 1
                    }
                }
            }
        }

        if delivered == 0 {
            return lastError ?? "No magic packet could be sent."
        }
        return nil
    }

    // MARK: - Probe

    private static func canConnectBlocking(host: String, port: UInt16, timeout: TimeInterval) -> Bool {
        guard var destination = socketAddress(host: host, port: port) else { return false }

        let handle = socket(AF_INET, SOCK_STREAM, 0)
        guard handle >= 0 else { return false }
        defer { close(handle) }

        let flags = fcntl(handle, F_GETFL, 0)
        guard flags >= 0, fcntl(handle, F_SETFL, flags | O_NONBLOCK) >= 0 else { return false }

        let started = withUnsafePointer(to: &destination) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { generic in
                connect(handle, generic, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        if started == 0 { return true }
        guard errno == EINPROGRESS else { return false }

        var descriptor = pollfd(fd: handle, events: Int16(POLLOUT), revents: 0)
        guard poll(&descriptor, 1, Int32(timeout * 1000)) > 0 else { return false }

        var socketError: Int32 = 0
        var length = socklen_t(MemoryLayout<Int32>.size)
        guard getsockopt(handle, SOL_SOCKET, SO_ERROR, &socketError, &length) == 0 else { return false }
        return socketError == 0
    }

    private static func socketAddress(host: String, port: UInt16) -> sockaddr_in? {
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = port.bigEndian
        guard inet_pton(AF_INET, host, &address.sin_addr) == 1 else { return nil }
        return address
    }

    private static func errorText() -> String {
        String(cString: strerror(errno))
    }
}
