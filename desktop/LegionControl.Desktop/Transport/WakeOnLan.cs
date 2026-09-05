using System.Net;
using System.Net.Sockets;
using LegionControl.Desktop.Config;

namespace LegionControl.Desktop.Transport;

/// What came of sending a magic packet.
///
/// Sending one is not waking a machine. The packet is fire and forget: nothing acknowledges it,
/// nothing reports it, and a switch that drops broadcast traffic swallows it in silence. So this
/// says exactly what was sent and to where, and whether the machine came back is a separate
/// question answered by a probe rather than by hope.
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

    /// Sends to every configured broadcast address on every configured port.
    public static WakeAttempt Send(WakeConfig wake) => Send(wake, wake.Broadcast, wake.Ports);

    /// The same, with the addresses worked out elsewhere: a machine at a site inherits that site's
    /// broadcast when it names none of its own.
    public static WakeAttempt Send(WakeConfig wake, IReadOnlyList<string> broadcasts, IReadOnlyList<int> ports)
    {
        var mac = wake.MacBytes;
        if (mac is null)
        {
            return new WakeAttempt(0, Array.Empty<string>(),
                new[] { $"\"{wake.Mac}\" is not a six byte hardware address." });
        }

        var packet = MagicPacket(mac);
        var sent = 0;
        var addresses = new List<string>();
        var errors = new List<string>();

        foreach (var address in broadcasts)
        {
            if (!IPAddress.TryParse(address, out var parsed))
            {
                errors.Add($"\"{address}\" is not an address.");
                continue;
            }
            foreach (var port in ports)
            {
                try
                {
                    using var client = new UdpClient();
                    client.EnableBroadcast = true;
                    client.Send(packet, packet.Length, new IPEndPoint(parsed, port));
                    sent += 1;
                    addresses.Add($"{address}:{port}");
                }
                catch (SocketException error)
                {
                    errors.Add($"{address}:{port} refused the packet: {error.Message}");
                }
            }
        }
        return new WakeAttempt(sent, addresses, errors);
    }

    /// Whether something is listening yet.
    ///
    /// Probing is deliberately slow: no more often than every three seconds per address, because a
    /// machine that is booting is doing something more useful than answering this, and a probe
    /// storm on a tailnet address is indistinguishable from a scan.
    public static async Task<bool> ProbeAsync(string host, int port, TimeSpan timeout, CancellationToken cancellationToken = default)
    {
        try
        {
            using var client = new TcpClient();
            using var deadline = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
            deadline.CancelAfter(timeout);
            await client.ConnectAsync(host, port, deadline.Token);
            return client.Connected;
        }
        catch (Exception)
        {
            return false;
        }
    }
}
