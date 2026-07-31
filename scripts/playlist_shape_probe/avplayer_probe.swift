#!/usr/bin/env swift
import Foundation
import AVFoundation
import CoreMedia

// AVPlayer playlist-shape probe (macOS). Pumps the run loop so HLS can load.
// Usage: swift avplayer_probe.swift <playlist_url> <seek_seconds>

let args = CommandLine.arguments
guard args.count >= 3, let seekTo = Double(args[2]), let url = URL(string: args[1]) else {
    fputs("usage: \(args[0]) <playlist_url> <seek_seconds>\n", stderr)
    exit(2)
}

func seekableJSON(_ item: AVPlayerItem) -> String {
    let ranges = item.seekableTimeRanges.compactMap { $0 as? CMTimeRange }
    let parts = ranges.map { r -> String in
        let s = CMTimeGetSeconds(r.start)
        let e = CMTimeGetSeconds(CMTimeRangeGetEnd(r))
        return String(format: "[%.3f,%.3f]", s, e)
    }
    let dur = CMTimeGetSeconds(item.duration)
    let durStr = dur.isFinite && !dur.isNaN ? String(format: "%.3f", dur) : "null"
    let cur = CMTimeGetSeconds(item.currentTime())
    return "{\"duration\":\(durStr),\"seekable\":[\(parts.joined(separator: ","))],\"current\":\(String(format: "%.3f", cur))}"
}

let item = AVPlayerItem(url: url)
let player = AVPlayer(playerItem: item)
var ready = false
var failed: String?

let statusObs = item.observe(\.status, options: [.new, .initial]) { item, _ in
    switch item.status {
    case .readyToPlay:
        ready = true
    case .failed:
        failed = String(describing: item.error)
    default:
        break
    }
}

player.play()

let deadline = Date().addingTimeInterval(20)
while Date() < deadline && !ready && failed == nil {
    RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.05))
}

if let failed {
    print("ready=false error=\(failed) before_seek=\(seekableJSON(item))")
    statusObs.invalidate()
    exit(1)
}

print("ready=\(ready) before_seek=\(seekableJSON(item))")

var seekDone = false
player.seek(to: CMTime(seconds: seekTo, preferredTimescale: 600), toleranceBefore: .zero, toleranceAfter: .zero) { finished in
    print("seek_finished=\(finished) after_seek=\(seekableJSON(item))")
    seekDone = true
}

let seekDeadline = Date().addingTimeInterval(15)
while Date() < seekDeadline && !seekDone {
    RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.05))
}

let playDeadline = Date().addingTimeInterval(2)
while Date() < playDeadline {
    RunLoop.current.run(mode: .default, before: Date().addingTimeInterval(0.05))
}
print("after_2s=\(seekableJSON(item)) rate=\(player.rate)")
statusObs.invalidate()
exit(ready ? 0 : 1)
