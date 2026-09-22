// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "FrankenRemoteKit",
    platforms: [
        .iOS(.v16),
        .macOS(.v13)
    ],
    products: [
        .library(
            name: "FrankenRemoteKit",
            targets: ["FrankenRemoteKit"]
        ),
    ],
    targets: [
        .target(
            name: "CFrankenRemote",
            path: "Sources/CFrankenRemote",
            publicHeadersPath: "include"
        ),
        .target(
            name: "FrankenRemoteKit",
            dependencies: ["CFrankenRemote"],
            path: "Sources/FrankenRemoteKit"
        ),
        .testTarget(
            name: "FrankenRemoteKitTests",
            dependencies: ["FrankenRemoteKit"],
            path: "Tests/FrankenRemoteKitTests"
        ),
    ]
)
