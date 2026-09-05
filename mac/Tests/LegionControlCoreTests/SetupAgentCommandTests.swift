import Foundation
import Testing
@testable import LegionControlCore

struct SetupAgentCommandTests {
    @Test func installerCommandsUsePinnedWrappers() {
        #expect(CommandArguments.installedAgent(platform: .linux, user: "atlas") == CommandArguments.installedAgent(platform: .mac, user: "robert"))
        let windows = CommandArguments.installedAgent(platform: .windows, user: "First Last")
        #expect(windows.suffix(2) == ["-File", "C:\\Users\\First Last\\.legion-control\\bin\\legionctl.ps1"])
        #expect(!windows.contains("node"))
    }

    @Test func posixStarterCommandUsesRemoteHomeWithoutNodeOnPath() throws {
        let home = FileManager.default.temporaryDirectory.appending(path: "setup home ' \(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: home) }
        let bin = home.appending(path: ".legion-control/bin")
        try FileManager.default.createDirectory(at: bin, withIntermediateDirectories: true)
        let launcher = bin.appending(path: "legionctl")
        try "#!/bin/sh\nprintf '%s' \"$*\"\n".write(to: launcher, atomically: false, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: launcher.path)
        let argv = CommandArguments.installedAgent(platform: .linux, user: "different account")
        let command = RemoteShell.posix.serialize(argv + ["config"])
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = ["-c", command]
        process.environment = ["HOME": home.path, "PATH": "/nonexistent"]
        let output = Pipe()
        process.standardOutput = output
        try process.run()
        process.waitUntilExit()
        #expect(process.terminationStatus == 0)
        #expect(String(decoding: output.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self) == "config")
    }
}
