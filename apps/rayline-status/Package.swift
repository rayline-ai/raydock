// swift-tools-version: 5.9

import PackageDescription

let package = Package(
  name: "RaylineStatus",
  platforms: [
    .macOS(.v13)
  ],
  products: [
    .library(name: "RaylineStatusCore", targets: ["RaylineStatusCore"]),
    .executable(name: "RaylineStatusApp", targets: ["RaylineStatusApp"]),
  ],
  targets: [
    .target(name: "RaylineStatusCore"),
    .executableTarget(
      name: "RaylineStatusApp",
      dependencies: ["RaylineStatusCore"]
    ),
    .testTarget(
      name: "RaylineStatusCoreTests",
      dependencies: ["RaylineStatusCore"]
    ),
  ]
)
