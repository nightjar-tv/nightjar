import AetherEngine
import Foundation

/// Minimal bake-off probe: stock AetherEngine API only (no Moonfin wrappers).
@main
struct AetherBakeoffProbe {
  static func main() async {
    let args = CommandLine.arguments
    guard args.count >= 2 else {
      fputs("usage: AetherBakeoffProbe <url> [seekSeconds]\n", stderr)
      exit(2)
    }
    let urlString = args[1]
    let seekSeconds = args.count >= 3 ? Double(args[2]) : nil
    guard let url = URL(string: urlString) else {
      fputs("bad url\n", stderr)
      exit(2)
    }

    do {
      let engine = try AetherEngine()
      let t0 = Date()
      try await engine.load(url: url)
      engine.play()
      let deadline = Date().addingTimeInterval(40)
      var firstMs: Double?
      while Date() < deadline {
        switch engine.state {
        case .playing, .paused:
          firstMs = Date().timeIntervalSince(t0) * 1000
        case .error:
          break
        default:
          try await Task.sleep(nanoseconds: 20_000_000)
          continue
        }
        break
      }

      var seekMs: Double?
      if let seekSeconds, firstMs != nil {
        let s0 = Date()
        var sawSeeking = false
        await engine.seek(to: seekSeconds)
        let seekDeadline = Date().addingTimeInterval(40)
        while Date() < seekDeadline {
          if engine.isSeeking { sawSeeking = true }
          let near = abs(engine.currentTime - seekSeconds) < 2.5
          let elapsed = Date().timeIntervalSince(s0)
          if elapsed >= 0.15 && near && (sawSeeking || !engine.isSeeking) && sawSeeking {
            seekMs = elapsed * 1000
            break
          }
          // Fallback: landed near target and seeking cleared after 150ms
          if elapsed >= 0.15 && near && !engine.isSeeking && sawSeeking {
            seekMs = elapsed * 1000
            break
          }
          try await Task.sleep(nanoseconds: 20_000_000)
        }
        if seekMs == nil, abs(engine.currentTime - seekSeconds) < 2.5 {
          seekMs = Date().timeIntervalSince(s0) * 1000
        }
      }

      var out: [String: Any] = [
        "engine": "AetherEngine",
        "url": urlString,
        "state": String(describing: engine.state),
        "builds_outside_moonfin": true,
      ]
      if let firstMs { out["first_frame_ms"] = firstMs }
      if let seekSeconds { out["seek_to_s"] = seekSeconds }
      if let seekMs { out["seek_land_ms"] = seekMs }
      let data = try JSONSerialization.data(withJSONObject: out, options: [.prettyPrinted, .sortedKeys])
      FileHandle.standardOutput.write(data)
      FileHandle.standardOutput.write(Data("\n".utf8))
      engine.stop()
      exit(firstMs == nil ? 1 : 0)
    } catch {
      fputs("AetherEngine failed: \(error)\n", stderr)
      let out: [String: Any] = [
        "engine": "AetherEngine",
        "builds_outside_moonfin": false,
        "error": String(describing: error),
      ]
      if let data = try? JSONSerialization.data(withJSONObject: out, options: [.prettyPrinted]) {
        FileHandle.standardOutput.write(data)
        FileHandle.standardOutput.write(Data("\n".utf8))
      }
      exit(1)
    }
  }
}
