// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "LegionControl",
    platforms: [
        .macOS("26.0")
    ],
    targets: [
        .executableTarget(
            name: "LegionControl",
            path: "Sources/LegionControl",
            swiftSettings: [
                .swiftLanguageMode(.v6)
            ]
        )
    ]
)
