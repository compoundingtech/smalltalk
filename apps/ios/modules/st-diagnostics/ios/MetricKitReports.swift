import CoreFoundation
import Foundation
import MetricKit

// Only these allowlisted fields cross into the store/bridge. Raw MetricKit JSON, binary
// names/UUIDs, memory addresses, termination reasons and exception messages never persist.
enum MetricKitReports {
  static func reports(_ payloads: [MXDiagnosticPayload], capturedAt: UInt64) throws -> [DiagnosticReport] {
    var reports: [DiagnosticReport] = []
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys]
    for payload in payloads {
      let begin = diagnosticMilliseconds(payload.timeStampBegin)
      let end = diagnosticMilliseconds(payload.timeStampEnd)
      var ordinal: UInt32 = 0
      for diagnostic in payload.crashDiagnostics ?? [] {
        let summary = DiagnosticPayload.crash(
          exceptionType: diagnostic.exceptionType?.intValue,
          signal: diagnostic.signal?.intValue,
          frames: frames(diagnostic.callStackTree)
        )
        reports.append(try report(diagnostic, summary: summary, ordinal: ordinal, begin: begin, end: end,
                                  capturedAt: capturedAt, severity: "fatal", encoder: encoder))
        ordinal += 1
        if reports.count > 128 { reports.removeFirst() }
      }
      for diagnostic in payload.hangDiagnostics ?? [] {
        let duration = diagnostic.hangDuration.converted(to: .milliseconds).value
        guard duration.isFinite, duration >= 0 else { continue }
        let summary = DiagnosticPayload.hang(
          durationMs: UInt64(min(duration, 9_007_199_254_740_991)), frames: frames(diagnostic.callStackTree)
        )
        reports.append(try report(diagnostic, summary: summary, ordinal: ordinal, begin: begin, end: end,
                                  capturedAt: capturedAt, severity: "warning", encoder: encoder))
        ordinal += 1
        if reports.count > 128 { reports.removeFirst() }
      }
    }
    return reports
  }

  private static func report(_ diagnostic: MXDiagnostic, summary: DiagnosticPayload, ordinal: UInt32,
                             begin: UInt64, end: UInt64, capturedAt: UInt64, severity: String,
                             encoder: JSONEncoder) throws -> DiagnosticReport {
    let app = diagnosticVersion(diagnostic.applicationVersion)
    let build = diagnosticVersion(diagnostic.metaData.applicationBuildVersion)
    let intervalIdentity = "st-diagnostics-metric-interval-v1:\(begin):\(end):\(app):\(build)"
    let context = DiagnosticLaunchContext(
      launch_id: diagnosticStableID(Data(intervalIdentity.utf8)), started_at_unix_ms: begin,
      app_version: app, native_build: build, os_version: diagnosticOSVersion(diagnostic.metaData.osVersion)
    )
    var identity = Data("st-diagnostics-metric-event-v1:\(context.launch_id):\(ordinal):".utf8)
    identity.append(try encoder.encode(summary))
    return DiagnosticReport(
      eventID: diagnosticStableID(identity), context: context, sequence: ordinal, occurredAt: end,
      capturedAt: capturedAt, severity: severity, source: "metrickit", payload: summary,
      occurrenceBasis: "metric-interval-end", launchBasis: "metric-interval"
    )
  }

  private static func frames(_ tree: MXCallStackTree) -> [DiagnosticNativeFrame] {
    // MetricKit exposes no typed frame API. Decode transiently, with a size ceiling, then
    // walk only known keys. No raw dump is logged, queued, hashed or exposed to JavaScript.
    let data = tree.jsonRepresentation()
    guard data.count <= 1_048_576,
          let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
          let tree = root["callStackTree"] as? [String: Any],
          let stacks = tree["callStacks"] as? [[String: Any]] else { return [] }
    var output: [DiagnosticNativeFrame] = []
    var visited = 0
    // Put the attributed (crashing) thread first without retaining all OS stack frames.
    for attributed in [true, false] {
      for stack in stacks where (stack["threadAttributed"] as? Bool ?? false) == attributed {
        guard let roots = stack["callStackRootFrames"] as? [[String: Any]] else { continue }
        walk(roots, depth: 0, visited: &visited, output: &output)
        if output.count == 32 || visited >= 256 { return output }
      }
    }
    return output
  }

  private static func walk(_ nodes: [[String: Any]], depth: Int, visited: inout Int,
                           output: inout [DiagnosticNativeFrame]) {
    guard depth < 32 else { return }
    for node in nodes {
      guard output.count < 32, visited < 256 else { return }
      visited += 1
      if let offset = offset(node["offsetIntoBinaryTextSegment"]) {
        output.append(DiagnosticNativeFrame(binary: binary(node["binaryName"] as? String), offset: offset))
      }
      if let children = node["subFrames"] as? [[String: Any]] {
        walk(children, depth: depth + 1, visited: &visited, output: &output)
      }
    }
  }

  private static func offset(_ value: Any?) -> String? {
    if let number = value as? NSNumber,
       CFGetTypeID(number) != CFBooleanGetTypeID(), let integer = UInt64(number.stringValue) {
      return String(integer, radix: 16)
    }
    // Accommodate an OS-provided numeric string, never an arbitrary symbol/address/path.
    guard let string = value as? String, !string.isEmpty, string.utf8.count <= 32 else { return nil }
    if string.hasPrefix("0x"), let integer = UInt64(string.dropFirst(2), radix: 16) {
      return String(integer, radix: 16)
    }
    guard let integer = UInt64(string) else { return nil }
    return String(integer, radix: 16)
  }

  private static func binary(_ name: String?) -> String {
    guard let name else { return "unknown" }
    if name == Bundle.main.object(forInfoDictionaryKey: "CFBundleExecutable") as? String { return "app" }
    switch name {
    case "Foundation", "CoreFoundation", "UIKit", "UIKitCore", "QuartzCore", "CoreGraphics",
         "CoreText", "CFNetwork", "Security", "Metal", "SwiftUI", "libobjc.A.dylib",
         "libsystem_kernel.dylib", "libsystem_pthread.dylib", "libsystem_c.dylib",
         "libsystem_platform.dylib", "libdispatch.dylib", "libdyld.dylib", "dyld":
      return "system"
    default:
      return "unknown"
    }
  }
}
