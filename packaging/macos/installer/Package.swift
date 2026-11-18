// swift-tools-version: 6.0
import PackageDescription

let package = Package(
  name: "KanadeInstaller",
  platforms: [.macOS(.v14)],
  products: [.executable(name: "KanadeInstaller", targets: ["KanadeInstaller"])],
  targets: [
    .executableTarget(name: "KanadeInstaller"),
    .testTarget(name: "KanadeInstallerTests", dependencies: ["KanadeInstaller"]),
  ]
)
