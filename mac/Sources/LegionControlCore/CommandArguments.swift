import Foundation

enum CommandArguments {
    static func installedAgent(platform: Platform, user: String) -> [String] {
        let account = user.trimmingCharacters(in: .whitespacesAndNewlines)
        let name = account.isEmpty ? "me" : account
        switch platform {
        case .linux, .mac:
            return ["/bin/sh", "-c", "exec \"$HOME/.legion-control/bin/legionctl\" \"$@\"", "legionctl"]
        case .windows:
            return ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                    "-File", "C:\\Users\\\(name)\\.legion-control\\bin\\legionctl.ps1"]
        }
    }

    static func parse(_ text: String) -> [String]? {
        guard let values = try? JSONDecoder().decode([String].self, from: Data(text.utf8)),
              !values.isEmpty, values.allSatisfy({ !$0.isEmpty && !$0.contains("\0") }) else { return nil }
        return values
    }
    static func text(_ argv: [String]) -> String {
        String(decoding: (try? JSONEncoder().encode(argv)) ?? Data("[]".utf8), as: UTF8.self)
    }
}
