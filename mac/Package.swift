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
        // Points LEGION_CONTROL_HOME at a directory of the test run's own before the first test
        // starts, so no test can write into the real app state, whatever it forgets to pass in.
        .target(
            name: "TestHome",
            path: "Tests/TestHome"
        ),
        .testTarget(
            name: "LegionControlCoreTests",
            dependencies: ["LegionControlCore", "TestHome"],
            path: "Tests/LegionControlCoreTests",
            swiftSettings: [
                .swiftLanguageMode(.v6)
            ]
        )
    ]
)
