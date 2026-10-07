import ExpoModulesCore
import MetricKit
import OSLog
import UIKit

public final class StDiagnosticsAppDelegateSubscriber: ExpoAppDelegateSubscriber {
  private static let logger = Logger(subsystem: "smalltalk.diagnostics", category: "native-storage")
  private var subscribed = false
  private let metrics = DiagnosticMetricKitSubscriber()

  public func subscriberDidRegister() {
    // Expo registers subscribers before AppDelegate initialization. Waiting only for
    // didFinishLaunching would miss JS startup failures in an app that starts RN first.
    startDiagnostics()
  }

  public func appDelegateWillBeginInitialization() {
    startDiagnostics()
  }

  public func application(_ application: UIApplication,
                          didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil) -> Bool {
    startDiagnostics()
    return true
  }

  public func applicationWillTerminate(_ application: UIApplication) {
    do {
      try DiagnosticStore.shared.terminateCleanly()
    } catch {
      Self.logger.error("The native clean-termination marker could not be persisted")
    }
    if subscribed {
      MXMetricManager.shared.remove(metrics)
      subscribed = false
    }
  }

  // Background and suspension are deliberately NOT clean termination. iOS can reclaim
  // the process (or the user can force quit) without ever delivering willTerminate.
  // Thus the next-launch breadcrumb is explicitly inferred and never a crash report.

  private func startDiagnostics() {
    do {
      // Synchronous atomic commit: launchContext() and JS see this very same UUID.
      try DiagnosticStore.shared.start()
    } catch {
      // Diagnostics must not prevent app launch. Bridge calls retry storage and reject
      // with a fixed error if protection/disk failures still prevent durable capture.
      Self.logger.error("The native launch marker could not be persisted")
    }
    guard !subscribed else { return }
    subscribed = true
    MXMetricManager.shared.add(metrics)
    // Retrieving, sanitizing and persisting historical reports all happen off the
    // launch thread. Repeated launch callbacks must not wait for this replay.
    metrics.replayHistoricalReports()
  }
}

