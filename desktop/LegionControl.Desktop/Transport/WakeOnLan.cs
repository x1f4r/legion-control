using System.Net;
using System.Net.Sockets;
using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Transport;

/// What came of sending a magic packet.
///
/// Sending one is not waking a machine. The packet is fire and forget: nothing acknowledges it,
/// nothing reports it, and a switch that drops broadcast traffic swallows it in silence. So this
/// says exactly what was sent and to where, and whether the machine came back is a separate
/// question answered by its own authenticated status rather than by hope.
public sealed record WakeAttempt(int PacketsSent, IReadOnlyList<string> Addresses, IReadOnlyList<string> Errors)
{
    public bool AnythingSent => PacketsSent > 0;

    public string Sentence => AnythingSent
        ? $"Sent {PacketsSent} magic {(PacketsSent == 1 ? "packet" : "packets")} to {string.Join(", ", Addresses)}. Nothing acknowledges one, so whether the machine is coming up is the next question."
        : $"No packet went out. {string.Join(" ", Errors)}";
}

public static class WakeOnLan
{
    /// The magic packet: six 0xFF bytes, then the hardware address sixteen times.
    public static byte[] MagicPacket(byte[] mac)
    {
        if (mac.Length != 6) throw new ArgumentException("A hardware address is six bytes.", nameof(mac));
        var packet = new byte[6 + 16 * 6];
        for (var index = 0; index < 6; index += 1) packet[index] = 0xFF;
        for (var repeat = 0; repeat < 16; repeat += 1)
        {
            Buffer.BlockCopy(mac, 0, packet, 6 + repeat * 6, 6);
        }
        return packet;
    }

    /// Magic packets per broadcast address and port (contract section 10.2). A packet on a busy
    /// link is easily lost and nothing ever acknowledges one, so each is sent three times.
    public const int Repeats = 3;

    /// Sends [repeats] packets for [mac] to every broadcast address on every port, from a socket
    /// that may broadcast. The addresses are dotted IPv4 ones the wake plan already chose; nothing
    /// here resolves a name. [send] stands in for the socket in a test.
    public static WakeAttempt Send(
        byte[] mac, IReadOnlyList<string> broadcasts, IReadOnlyList<int> ports, int repeats = Repeats,
        Action<byte[], IPEndPoint>? send = null)
    {
        send ??= Broadcast;
        var packet = MagicPacket(mac);
        var sent = 0;
        var addresses = new List<string>();
        var errors = new List<string>();

        foreach (var address in broadcasts)
        {
            if (LanPrefix.ParseIPv4(address) is not { } parsed)
            {
                errors.Add($"\"{address}\" is not a dotted IPv4 address.");
                continue;
            }
            var endpoint = new IPAddress(new[] { (byte)(parsed >> 24), (byte)(parsed >> 16), (byte)(parsed >> 8), (byte)parsed });
            foreach (var port in ports)
            {
                var before = sent;
                for (var repeat = 0; repeat < repeats; repeat += 1)
                {
                    try
                    {
                        send(packet, new IPEndPoint(endpoint, port));
                        sent += 1;
                    }
                    catch (SocketException error)
                    {
                        errors.Add($"{address}:{port} refused the packet: {error.Message}");
                        break;
                    }
                }
                if (sent > before) addresses.Add($"{address}:{port}");
            }
        }
        return new WakeAttempt(sent, addresses, errors);
    }

    private static void Broadcast(byte[] packet, IPEndPoint endpoint)
    {
        using var client = new UdpClient();
        client.EnableBroadcast = true;
        client.Send(packet, packet.Length, endpoint);
    }
}
