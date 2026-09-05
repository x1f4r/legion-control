import Foundation

enum CommandArguments {
    static func parse(_ text: String) -> [String]? {
        guard let values = try? JSONDecoder().decode([String].self, from: Data(text.utf8)),
              !values.isEmpty, values.allSatisfy({ !$0.isEmpty && !$0.contains("\0") }) else { return nil }
        return values
    }
    static func text(_ argv: [String]) -> String {
        String(decoding: (try? JSONEncoder().encode(argv)) ?? Data("[]".utf8), as: UTF8.self)
    }
}
