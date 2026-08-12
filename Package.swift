// swift-tools-version: 6.2
import PackageDescription

// GenAIAgent — DISTRIBUTION manifest (repo root, what the Swift Package Index and remote
// consumers see). The Rust XCFramework is fetched from this repo's GitHub Releases as a
// versioned zip; `url`/`checksum` below are rewritten by ffi/release_swift.sh on every
// release — do not edit them by hand.
//
// Local development uses ffi/swiftpm/Package.swift instead (path-based binary target against
// a locally built GenAIAgent.xcframework — run ffi/build_apple.sh first). Keep the
// `platforms:` floors of BOTH manifests in sync with the deployment targets in
// ffi/build_apple.sh.
//
// Version policy: the Swift package version tracks the `rust-genai-agent` crate version and
// is tagged as a bare semver tag (`0.2.0`) — crate releases use prefixed tags
// (`rust-genai-agent-v0.2.0`), which the Swift Package Index ignores.
let package = Package(
	name: "GenAIAgent",
	platforms: [
		.iOS(.v26),
		.macOS(.v26),
	],
	products: [
		.library(name: "GenAIAgent", targets: ["GenAIAgent"]),
	],
	targets: [
		// Rust static libraries + FFI headers (module `genai_agent_ffiFFI`), fetched from the
		// GitHub Release for this tag. The embedded modulemap auto-links
		// Security/CoreFoundation/SystemConfiguration.
		.binaryTarget(
			name: "genai_agent_ffiFFI",
			url: "https://github.com/agentprism/genai-agent-rs/releases/download/0.2.0/GenAIAgent.xcframework.zip",
			checksum: "fb82dd841978dce65d8f0fbd1bae368c96aed9ec8471001aed1b911355827abd"
		),
		// UniFFI-generated bindings (committed at release time) + hand-written conveniences;
		// this is the module apps import.
		.target(
			name: "GenAIAgent",
			dependencies: ["genai_agent_ffiFFI"],
			path: "ffi/swiftpm/Sources/GenAIAgent"
		),
		.testTarget(
			name: "GenAIAgentTests",
			dependencies: ["GenAIAgent"],
			path: "ffi/swiftpm/Tests/GenAIAgentTests"
		),
	]
)
