// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "FrankenRemoteApp",
    platforms: [
        .iOS(.v16),
        .macOS(.v13)
    ],
    products: [
        .library(
            name: "FrankenRemoteApp",
            targets: ["FrankenRemoteApp"]
        ),
    ],
    dependencies: [
        .package(path: "../FrankenRemoteKit")
    ],
    targets: [
        .target(
            name: "FrankenRemoteApp",
            dependencies: [
                .product(name: "FrankenRemoteKit", package: "FrankenRemoteKit")
            ],
            path: "Sources"
        ),
        .testTarget(
            name: "FrankenRemoteAppTests",
            dependencies: ["FrankenRemoteApp"],
            path: "Tests"
        ),
    ]
)
