import AVFoundation
import Darwin
import Foundation
import Speech

// st-listen: the microphone into SpeechAnalyzer's transcriber, on this Mac, for stui's voice mode.
// It writes one JSON object per line to stdout:
//   {"event":"ready","device":"MacBook Pro Microphone"}  listening, and to what
//   {"event":"silent","device":"…"}                 nothing but silence for 3 seconds
//   {"event":"level","level":0.42}                 loudness 0-1, about 20 a second, for a waveform
//   {"event":"text","text":"so far","final":false} the words so far
//   {"event":"done","text":"all of it"}            after finish (or stdin closing)
//   {"event":"error","message":"in words"}         then it exits 1
// It reads lines from stdin: "finish" stops and reports everything heard; "cancel" stops at once.
//
// The microphone prompt belongs to the process macOS holds responsible, which for a program run
// from a terminal is the terminal app. So st-listen first starts itself again disclaimed, which
// makes it responsible for itself: the prompt then names Small Talk, from this bundle's
// Info.plist, whatever terminal stui runs in.

setvbuf(stdout, nil, _IOLBF, 0)

func emit(_ object: [String: Any]) {
  guard let data = try? JSONSerialization.data(withJSONObject: object),
    let line = String(data: data, encoding: .utf8)
  else { return }
  print(line)
}

func fail(_ message: String) -> Never {
  emit(["event": "error", "message": message])
  exit(1)
}

/// Start this program again, responsible for itself, and pass on how it ended.
func respawnDisclaimed() {
  let environment = ProcessInfo.processInfo.environment
  if environment["ST_LISTEN_DISCLAIMED"] != nil || CommandLine.arguments.contains("--no-disclaim") {
    return
  }
  typealias Disclaim = @convention(c) (UnsafeMutablePointer<posix_spawnattr_t?>, Int32) -> Int32
  guard let symbol = dlsym(UnsafeMutableRawPointer(bitPattern: -2), "responsibility_spawnattrs_setdisclaim")
  else { return }
  let disclaim = unsafeBitCast(symbol, to: Disclaim.self)
  var attributes: posix_spawnattr_t?
  posix_spawnattr_init(&attributes)
  defer { posix_spawnattr_destroy(&attributes) }
  guard disclaim(&attributes, 1) == 0 else { return }
  let path = Bundle.main.executablePath ?? CommandLine.arguments[0]
  let arguments = [path] + CommandLine.arguments.dropFirst()
  let variables = environment.map { "\($0.key)=\($0.value)" } + ["ST_LISTEN_DISCLAIMED=1"]
  var argv = arguments.map { strdup($0) } + [nil]
  var envp = variables.map { strdup($0) } + [nil]
  defer {
    argv.forEach { free($0) }
    envp.forEach { free($0) }
  }
  var child: pid_t = 0
  guard posix_spawn(&child, path, nil, &attributes, &argv, &envp) == 0 else { return }
  // The child shares this terminal's stdin and stdout; this process only waits for it.
  for signal in [SIGINT, SIGTERM, SIGHUP] {
    Darwin.signal(signal, SIG_IGN)
    let source = DispatchSource.makeSignalSource(signal: signal)
    source.setEventHandler { kill(child, signal) }
    source.resume()
  }
  var status: Int32 = 0
  while waitpid(child, &status, 0) == -1 && errno == EINTR {}
  let code = (status & 0x7f) == 0 ? (status >> 8) & 0xff : 128 + (status & 0x7f)
  exit(code)
}

@available(macOS 26.0, *)
final class Listener {
  private let engine = AVAudioEngine()
  private var analyzer: SpeechAnalyzer?
  private var input: AsyncStream<AnalyzerInput>.Continuation?
  private var results: Task<Void, Never>?
  private var settled = ""
  private var latest = ""
  private var lastLevel = Date.distantPast
  private var started = Date()
  private var heard = false
  private var device = AVCaptureDevice.default(for: .audio)?.localizedName ?? "the default input"

  func start(locale wanted: Locale) async throws {
    guard await AVCaptureDevice.requestAccess(for: .audio) else {
      fail("the microphone is off for Small Talk in System Settings › Privacy & Security › Microphone")
    }
    let locale = await SpeechTranscriber.supportedLocale(equivalentTo: wanted) ?? Locale(identifier: "en-US")
    let transcriber = SpeechTranscriber(
      locale: locale, transcriptionOptions: [], reportingOptions: [.volatileResults], attributeOptions: [])
    // The model for the language downloads once; later it is on the Mac already.
    if let install = try await AssetInventory.assetInstallationRequest(supporting: [transcriber]) {
      try await install.downloadAndInstall()
    }
    let analyzer = SpeechAnalyzer(modules: [transcriber])
    self.analyzer = analyzer
    guard let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [transcriber]) else {
      fail("no audio format suits the transcriber")
    }
    let (stream, continuation) = AsyncStream<AnalyzerInput>.makeStream()
    input = continuation
    results = Task { [weak self] in
      do {
        for try await result in transcriber.results {
          guard let self else { return }
          let text = String(result.text.characters)
          if result.isFinal {
            settled += text
            latest = settled
          } else {
            latest = settled + text
          }
          emit(["event": "text", "text": latest, "final": result.isFinal])
        }
      } catch {
        emit(["event": "error", "message": error.localizedDescription])
      }
    }
    try await analyzer.start(inputSequence: stream)

    let node = engine.inputNode
    let micFormat = node.outputFormat(forBus: 0)
    guard let converter = AVAudioConverter(from: micFormat, to: format) else {
      fail("the microphone's audio cannot be converted")
    }
    node.installTap(onBus: 0, bufferSize: 2048, format: micFormat) { [weak self] buffer, _ in
      guard let self else { return }
      let now = Date()
      if now.timeIntervalSince(lastLevel) >= 0.05 {
        lastLevel = now
        let level = (Listener.level(of: buffer) * 100).rounded() / 100
        // Two places exactly, not 0.77000000000000002.
        emit(["event": "level", "level": NSDecimalNumber(string: String(format: "%.2f", level))])
        // A muted, absent or not-connected input gives exact silence, not quiet.
        if level > 0 { heard = true }
        if !heard && now.timeIntervalSince(started) >= 3 {
          heard = true
          emit(["event": "silent", "device": device])
        }
      }
      let ratio = format.sampleRate / micFormat.sampleRate
      let capacity = AVAudioFrameCount(Double(buffer.frameLength) * ratio) + 1
      guard let converted = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: capacity) else { return }
      var supplied = false
      var error: NSError?
      converter.convert(to: converted, error: &error) { _, status in
        if supplied {
          status.pointee = .noDataNow
          return nil
        }
        supplied = true
        status.pointee = .haveData
        return buffer
      }
      if error == nil { input?.yield(AnalyzerInput(buffer: converted)) }
    }
    engine.prepare()
    try engine.start()
    started = Date()
    emit(["event": "ready", "device": device])
  }

  /// Stop listening and return everything heard.
  func finish() async -> String {
    engine.stop()
    engine.inputNode.removeTap(onBus: 0)
    input?.finish()
    try? await analyzer?.finalizeAndFinishThroughEndOfInput()
    await results?.value
    return latest
  }

  /// The buffer's loudness, 0 to 1.
  static func level(of buffer: AVAudioPCMBuffer) -> Double {
    guard let samples = buffer.floatChannelData?[0], buffer.frameLength > 0 else { return 0 }
    var sum: Float = 0
    for index in 0..<Int(buffer.frameLength) { sum += samples[index] * samples[index] }
    let rms = (sum / Float(buffer.frameLength)).squareRoot()
    return Double(min(1, max(0, (20 * log10(max(rms, 0.000_01)) + 50) / 50)))
  }
}

respawnDisclaimed()

guard #available(macOS 26.0, *) else { fail("voice needs macOS 26 or later") }

let listener = Listener()
Task {
  do {
    try await listener.start(locale: Locale.current)
  } catch {
    fail(error.localizedDescription)
  }
}

// Commands on stdin; its end is "finish".
Thread {
  var command = "finish"
  while let line = readLine() {
    let word = line.trimmingCharacters(in: .whitespaces)
    if word == "cancel" || word == "finish" {
      command = word
      break
    }
  }
  if command == "cancel" { exit(0) }
  Task {
    let text = await listener.finish()
    emit(["event": "done", "text": text])
    exit(0)
  }
}.start()

dispatchMain()
