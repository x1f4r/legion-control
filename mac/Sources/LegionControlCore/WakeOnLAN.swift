import Darwin
import Foundation

/// Wake-on-LAN: one magic packet, sent from this app. Done with sockets directly so there is no
/// helper script to install, and every address it uses comes from the machine's own `wake` block in
/// the config. Whether the machine came back is decided elsewhere, by asking its agent.
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

    // Waiting for the machine to come back deliberately does NOT live here any more.
    //
    // It used to be a bare TCP connection to port 22, made once a second until something answered.
    // OpenSSH has had per-source penalties on by default since 9.8, and repeated connections that
    // never complete authentication are exactly what those penalties are for: the wake probe could
    // earn a temporary refusal from the machine it was trying to reach. Readiness is now decided by
    // an ordinary authenticated agent call with backoff, in MachineModel, which has the additional
    // merit of proving the thing the user actually cares about — that the machine can be driven —
    // rather than that something is listening on a port.

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
