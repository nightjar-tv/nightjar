// swift-tools-version: 5.9
import PackageDescription

let package = Package(
  name: "AetherBakeoffProbe",
  platforms: [.macOS(.v14)],
  dependencies: [
    .package(url: "https://github.com/superuser404notfound/AetherEngine", from: "5.20.0"),
  ],
  targets: [
    .executableTarget(
      name: "AetherBakeoffProbe",
      dependencies: [
        .product(name: "AetherEngine", package: "AetherEngine"),
      ],
      path: "Sources"
    ),
  ]
)
