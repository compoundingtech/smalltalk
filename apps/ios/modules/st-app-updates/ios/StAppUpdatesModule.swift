import ExpoModulesCore
import EXUpdates

public final class StAppUpdatesModule: Module {
  public func definition() -> ModuleDefinition {
    Name("StAppUpdates")
    AsyncFunction("setPairedGateway") { (gateway: String) in
      guard AppController.isInitialized(),
        let controller = AppController.sharedInstance as? EnabledAppController else {
        throw NSError(domain: "StAppUpdates", code: 3)
      }
      try controller.setSmalltalkPairedGateway(gateway)
    }.runOnQueue(.main)
  }
}
