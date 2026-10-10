import ExpoModulesCore

// A build made without ST3_FABRIC=1 does not link the carrier or require a Rust toolchain. The app
// then offers no fabric carrier.
public final class StFabricModule: Module {
  public func definition() -> ModuleDefinition {
    Name("StFabric")
    AsyncFunction("available") { () -> Bool in false }
    AsyncFunction("identity") { () -> String in
      throw Exception(name: "FabricDisabled", description: "Reinstall pods with ST3_FABRIC=1 to link the fabric carrier")
    }
    AsyncFunction("dial") { (_: String, _: String, _: String?, _: String) -> String in
      throw Exception(name: "FabricDisabled", description: "This build does not link the fabric carrier")
    }
    AsyncFunction("stats") { () -> String in
      throw Exception(name: "FabricDisabled", description: "This build does not link the fabric carrier")
    }
    AsyncFunction("stop") {}
  }
}
