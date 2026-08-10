// swift-tools-version: 6.2
import PackageDescription

// SwiftPM wrapper around the Rust `genai-agent-ffi` library.
// Regenerate GenAIAgent.xcframework and Sources/GenAIAgent/genai_agent_ffi.swift
// by running ../build_apple.sh — do not edit them by hand.
//
// Minimum OS versions match IPHONEOS_DEPLOYMENT_TARGET/MACOSX_DEPLOYMENT_TARGET
// in ../build_apple.sh — keep them in sync.
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
		// Rust static libraries + FFI headers (module `genai_agent_ffiFFI`).
		// The embedded modulemap auto-links Security/CoreFoundation/SystemConfiguration.
		.binaryTarget(name: "genai_agent_ffiFFI", path: "GenAIAgent.xcframework"),
		// UniFFI-generated bindings + hand-written conveniences; this is the
		// module apps import.
		.target(name: "GenAIAgent", dependencies: ["genai_agent_ffiFFI"]),
		.testTarget(name: "GenAIAgentTests", dependencies: ["GenAIAgent"]),
	]
)
