// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "St3Client",
    platforms: [.iOS(.v17), .macOS(.v14)],
    products: [.library(name: "St3Client", targets: ["St3Client"])],
    targets: [
        .target(name: "St3Client"),
        .testTarget(name: "St3ClientTests", dependencies: ["St3Client"]),
    ]
)
