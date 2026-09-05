// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "LegionControl",
    platforms: [
        .macOS("26.0")
    ],
    targets: [
        // Everything the app is, as a library, so the parts that can be wrong on their own can be
        // tested on their own. The executable below is the entry point and nothing else.
        .target(
            name: "LegionControlCore",
            path: "Sources/LegionControlCore",
            swiftSettings: [
                .swiftLanguageMode(.v6)
            ]
        ),
        .executableTarget(
            name: "LegionControl",
            dependencies: ["LegionControlCore"],
            path: "Sources/LegionControlApp",
            swiftSettings: [
                .swiftLanguageMode(.v6)
            ]
        ),
        .testTarget(
            name: "LegionControlCoreTests",
            dependencies: ["LegionControlCore"],
            path: "Tests/LegionControlCoreTests",
            swiftSettings: [
                .swiftLanguageMode(.v6)
            ]
        )
    ]
)
