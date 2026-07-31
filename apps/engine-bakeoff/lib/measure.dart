import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:math';

import 'package:http/http.dart' as http;
import 'package:libvlc_bakeoff/libvlc_bakeoff.dart';
import 'package:media_kit/media_kit.dart';
import 'package:path/path.dart' as p;

/// Nightjar engine bake-off measurement core (Part A/B + ABR signal audit).
class BakeoffConfig {
  BakeoffConfig({
    required this.baseUrl,
    required this.samplePath,
    required this.outDir,
    this.firstFrameTimeout = const Duration(seconds: 25),
  });

  final String baseUrl;
  final String samplePath;
  final String outDir;
  final Duration firstFrameTimeout;

  String streamUrl(int id) => '$baseUrl/items/$id/stream';
  String get sessionBase =>
      Platform.environment['NIGHTJAR_SESSION_BASE'] ?? 'http://127.0.0.1:8096';
}

class LatencyStats {
  LatencyStats(this.samples);

  final List<int> samples;

  int get n => samples.length;
  int get min => samples.reduce((a, b) => a < b ? a : b);
  int get max => samples.reduce((a, b) => a > b ? a : b);
  int get p50 => _pct(0.50);
  int get p90 => _pct(0.90);

  int _pct(double q) {
    final s = [...samples]..sort();
    final i = (q * (s.length - 1)).round().clamp(0, s.length - 1);
    return s[i];
  }

  Map<String, Object> toJson() => {
        'n': n,
        'min_ms': min,
        'p50_ms': p50,
        'p90_ms': p90,
        'max_ms': max,
        'samples_ms': samples,
      };
}

class BakeoffRunner {
  BakeoffRunner(this.config);

  final BakeoffConfig config;
  final List<Map<String, Object?>> events = [];

  Player? _mpv;
  LibvlcBakeoffPlayer? _vlc;
  String? _openEngine;
  String? _openUrl;

  void log(String type, Map<String, Object?> fields) {
    final row = {
      't': DateTime.now().toUtc().toIso8601String(),
      'type': type,
      ...fields,
    };
    events.add(row);
    stderr.writeln(jsonEncode(row));
  }

  Future<Map<String, dynamic>> loadSample() async {
    final text = await File(config.samplePath).readAsString();
    return jsonDecode(text) as Map<String, dynamic>;
  }

  Future<Map<String, Object?>> auditAbrSignals() async {
    final mediaKit = {
      'package': 'media_kit 1.2.x',
      'streams': {
        'playing': true,
        'buffering': true,
        'bufferingPercentage': true,
        'buffer': true,
        'duration': true,
        'position': true,
        'error': true,
        'audioBitrate': true,
        'videoParams': true,
        'audioParams': true,
      },
      'missing_for_server_abr': [
        'download_rate_bytes_per_sec',
        'hls_rendition / level index',
        'rebuffer_event distinct from buffering bool',
      ],
      'usable_trigger':
          'buffering / bufferingPercentage / buffer Duration detect starve; '
              'no download-rate for proactive rung selection',
      'notes':
          'Player.stream.buffering and bufferingPercentage are the practical stall signals. '
              'audioBitrate is media bitrate, not link throughput.',
    };

    return {
      'media_kit': mediaKit,
      'libvlc_bakeoff': LibvlcBakeoffPlayer.signals.toJson(),
      'verdict':
          'Both expose stall/buffer signals usable as a trigger to ask the server '
              'for a lower rung. Neither exposes a clean download-rate for proactive ABR. '
              'Server-side ABR is not blocked — react to sustained buffering.',
      'stop_gate_neither_signal': false,
    };
  }

  Future<Map<String, Object?>> runPartALatency(String engine) async {
    final sample = await loadSample();
    final ids = (sample['latency_item_ids'] as List).cast<int>();
    final byId = {
      for (final t in (sample['t4_sample'] as List).cast<Map<String, dynamic>>())
        t['id'] as int: t,
    };

    final coldStartup = <int>[];
    final warmStartup = <int>[];
    final warmNear = <int>[];
    final warmFar = <int>[];
    final coldFar = <int>[];

    for (final id in ids) {
      final meta = byId[id];
      if (meta == null) continue;
      final durationMs = meta['duration_ms'] as int? ?? 0;
      if (durationMs < 60_000) continue;
      final url = config.streamUrl(id);

      final cold = await _timedFirstFrame(engine, url, label: 'cold_startup', itemId: id);
      if (cold != null) coldStartup.add(cold);

      final warm = await _timedFirstFrame(engine, url, label: 'warm_startup', itemId: id);
      if (warm != null) warmStartup.add(warm);
      if (warm == null) continue;

      final nearTarget = (durationMs * 0.1).round() + 30_000;
      final farTarget = (durationMs * 0.75).round();

      final near = await _timedSeek(engine, url, nearTarget, label: 'warm_near', itemId: id);
      if (near != null) warmNear.add(near);

      final farW = await _timedSeek(engine, url, farTarget, label: 'warm_far', itemId: id);
      if (farW != null) warmFar.add(farW);

      await _disposeEngine('all');
      final farC =
          await _timedSeek(engine, url, farTarget, label: 'cold_far', itemId: id, reopen: true);
      if (farC != null) coldFar.add(farC);
    }

    await _disposeEngine('all');

    Map<String, Object?> pack(String name, List<int> xs) =>
        xs.isNotEmpty ? {name: LatencyStats(xs).toJson()} : {name: {'n': 0}};

    return {
      'engine': engine,
      'url_resolution':
          'constructed /api/v0/items/{id}/stream from MPV_V0 DP sample; not playback-info',
      ...pack('cold_startup', coldStartup),
      ...pack('warm_startup', warmStartup),
      ...pack('warm_near_seek', warmNear),
      ...pack('warm_far_seek', warmFar),
      ...pack('cold_far_seek', coldFar),
    };
  }

  Future<void> _disposeEngine(String engine) async {
    if (engine == 'mpv' || engine == 'all') {
      await _mpv?.dispose();
      _mpv = null;
    }
    if (engine == 'vlc' || engine == 'all') {
      _vlc?.dispose();
      _vlc = null;
    }
    _openEngine = null;
    _openUrl = null;
  }

  Future<int?> _timedFirstFrame(
    String engine,
    String url, {
    required String label,
    required int itemId,
  }) async {
    await _disposeEngine('all');
    final sw = Stopwatch()..start();
    try {
      if (engine == 'mpv') {
        MediaKit.ensureInitialized();
        _mpv = Player();
        final done = Completer<void>();
        // Prefer decoded clock advance; fall back to playing after buffer settles
        // so a stalled HTTP open still times out via firstFrameTimeout.
        var playing = false;
        final subPos = _mpv!.stream.position.listen((pos) {
          if (pos.inMilliseconds >= 80 && !done.isCompleted) done.complete();
        });
        final subPlay = _mpv!.stream.playing.listen((p) {
          playing = p;
        });
        final subBuf = _mpv!.stream.buffering.listen((b) {
          if (!b && playing && _mpv!.state.position.inMilliseconds >= 40 && !done.isCompleted) {
            done.complete();
          }
        });
        await _mpv!.open(Media(url), play: true);
        await done.future.timeout(config.firstFrameTimeout);
        await subPos.cancel();
        await subPlay.cancel();
        await subBuf.cancel();
        _openEngine = 'mpv';
        _openUrl = url;
      } else {
        _vlc = LibvlcBakeoffPlayer();
        _vlc!.open(url);
        final ok = await _vlc!.playAndWaitFirstFrame(timeout: config.firstFrameTimeout);
        if (!ok) {
          log('first_frame_fail', {'engine': engine, 'itemId': itemId, 'label': label});
          return null;
        }
        _openEngine = 'vlc';
        _openUrl = url;
      }
      sw.stop();
      log('first_frame', {
        'engine': engine,
        'itemId': itemId,
        'label': label,
        'ms': sw.elapsedMilliseconds,
      });
      return sw.elapsedMilliseconds;
    } catch (e) {
      log('first_frame_error', {
        'engine': engine,
        'itemId': itemId,
        'label': label,
        'error': e.toString(),
      });
      await _disposeEngine('all');
      return null;
    }
  }

  Future<int?> _timedSeek(
    String engine,
    String url,
    int targetMs, {
    required String label,
    required int itemId,
    bool reopen = false,
  }) async {
    try {
      if (reopen || _openEngine != engine || _openUrl != url) {
        final ff =
            await _timedFirstFrame(engine, url, label: '${label}_reopen', itemId: itemId);
        if (ff == null) return null;
      }
      final beforeMs = engine == 'mpv'
          ? _mpv!.state.position.inMilliseconds
          : (_vlc?.timeMs ?? 0);
      final sw = Stopwatch()..start();
      if (engine == 'mpv') {
        var sawBuffer = false;
        final sub = _mpv!.stream.buffering.listen((b) {
          if (b) sawBuffer = true;
        });
        final done = Completer<void>();
        // Land = wall clock until position is near target after a real move.
        // Ignore the first 100ms so a stale position sample cannot "land".
        final posSub = _mpv!.stream.position.listen((pos) {
          if (sw.elapsedMilliseconds < 100) return;
          final near = (pos.inMilliseconds - targetMs).abs() < 2500;
          final moved = (pos.inMilliseconds - beforeMs).abs() > 1500;
          if (near && moved && !done.isCompleted) done.complete();
        });
        await _mpv!.seek(Duration(milliseconds: targetMs));
        await done.future.timeout(config.firstFrameTimeout);
        await sub.cancel();
        await posSub.cancel();
        sw.stop();
        log('seek_land', {
          'engine': engine,
          'itemId': itemId,
          'label': label,
          'target_ms': targetMs,
          'ms': sw.elapsedMilliseconds,
          'saw_buffering': sawBuffer,
          'before_ms': beforeMs,
          'after_ms': _mpv!.state.position.inMilliseconds,
        });
      } else {
        final ok = await _vlc!.seekMs(targetMs, timeout: config.firstFrameTimeout);
        sw.stop();
        if (!ok) {
          log('seek_fail', {'engine': engine, 'itemId': itemId, 'label': label});
          return null;
        }
        log('seek_land', {
          'engine': engine,
          'itemId': itemId,
          'label': label,
          'target_ms': targetMs,
          'ms': sw.elapsedMilliseconds,
          'saw_buffering': _vlc!.sawBuffering,
        });
      }
      return sw.elapsedMilliseconds;
    } catch (e) {
      log('seek_error', {
        'engine': engine,
        'itemId': itemId,
        'label': label,
        'error': e.toString(),
      });
      return null;
    }
  }

  Future<Map<String, Object?>> runT4(String engine, {int? limit}) async {
    final sample = await loadSample();
    final titles = (sample['t4_sample'] as List).cast<Map<String, dynamic>>();
    final take = limit == null ? titles : titles.take(limit).toList();
    var ok = 0;
    var fail = 0;
    final failures = <Map<String, Object?>>[];
    final behaviourNotes = <Map<String, Object?>>[];

    for (final t in take) {
      final id = t['id'] as int;
      final stratum = t['stratum'] as String? ?? '';
      final url = config.streamUrl(id);
      final ms = await _timedFirstFrame(engine, url, label: 't4', itemId: id);
      await _disposeEngine('all');
      if (ms != null) {
        ok++;
      } else if (stratum == 'damaged_class') {
        behaviourNotes
            .add({'id': id, 'stratum': stratum, 'note': 'damaged_class no first frame'});
      } else {
        fail++;
        failures.add({
          'id': id,
          'stratum': stratum,
          'video_codec': t['video_codec'],
          'audio_codec': t['audio_codec'],
          'path': t['path'],
        });
      }
    }

    final scored = ok + fail;
    final rate = scored == 0 ? 0.0 : fail / scored;
    return {
      'engine': engine,
      'attempted': take.length,
      'ok': ok,
      'fail': fail,
      'failure_rate': rate,
      'threshold': 0.02,
      'disqualified': rate > 0.02,
      'failures': failures,
      'behaviour_notes': behaviourNotes,
      'sampling': sample['method'],
      'stratum_counts': sample['stratum_counts'],
    };
  }

  Future<Map<String, Object?>> runPartB(String engine) async {
    final sample = await loadSample();
    final candidates = (sample['part_b_candidates'] as List).cast<Map<String, dynamic>>();
    final results = <Map<String, Object?>>[];

    for (final t in candidates.take(5)) {
      final id = t['id'] as int;
      final bitrate = t['bitrate_bps_est'] as int? ?? 4_000_000;
      final configuredBps = max(12500, bitrate ~/ 2);
      final infoResp =
          await http.get(Uri.parse('${config.sessionBase}/api/v0/items/$id/playback-info'));
      if (infoResp.statusCode != 200) {
        results.add({'id': id, 'error': 'playback-info ${infoResp.statusCode}'});
        continue;
      }
      final info = jsonDecode(infoResp.body) as Map<String, dynamic>;
      final method = info['playbackMethod'] as String?;
      if (method != 'transcode') {
        results.add({
          'id': id,
          'skipped': true,
          'reason': 'playbackMethod=$method (want compatibility transcode)',
          'playbackMethod': method,
        });
        continue;
      }
      final sessionsUrl = info['sessionsUrl'] as String?;
      if (sessionsUrl == null) {
        results.add({'id': id, 'error': 'no sessionsUrl'});
        continue;
      }
      final start = await http.post(Uri.parse('${config.sessionBase}$sessionsUrl'));
      if (start.statusCode != 202) {
        results.add({'id': id, 'error': 'session start ${start.statusCode} ${start.body}'});
        continue;
      }
      final session = jsonDecode(start.body) as Map<String, dynamic>;
      final playlistUrl = '${config.sessionBase}${session['playlistUrl']}';
      final sessionId = session['sessionId'];

      final startupMs =
          await _timedFirstFrame(engine, playlistUrl, label: 'partb_startup', itemId: id);
      final durationMs = t['duration_ms'] as int? ?? 600_000;
      final farTarget = (durationMs * 0.75).round();
      final farMs =
          await _timedSeek(engine, playlistUrl, farTarget, label: 'partb_far', itemId: id);

      results.add({
        'id': id,
        'playbackMethod': method,
        'encoderKind': session['encoderKind'],
        'videoEncoder': session['videoEncoder'],
        'bitrate_bps_est': bitrate,
        'throttle_configured_bps': configuredBps,
        'throttle_note':
            'Configure proxy at 50% of bitrate_bps_est; record achieved from proxy counters',
        'startup_ms': startupMs,
        'far_seek_ms': farMs,
        'sessionId': sessionId,
        'playlist_shape_today':
            'full-title VOD listing with forced-IDR cuts; cold segments 503 until cook',
        'playlist_shape_1c':
            'honest full-title already publishable for transcode; seek land denser than copy',
      });

      await http.delete(Uri.parse('${config.sessionBase}/api/v0/sessions/$sessionId'));
      await _disposeEngine('all');
    }

    return {
      'engine': engine,
      'force_method': 'compatibility-transcode via BROWSER_V0 playback-info',
      'caveat':
          'Encoder input is not bitrate-capped; delivery is SessionMode::Transcode + forced IDR. '
              'HEVC/DTS sample skews high vs library p50 3.86 / p90 10 Mbps.',
      'runs': results,
    };
  }

  Future<void> writeOutputs(Map<String, Object?> report) async {
    final dir = Directory(config.outDir);
    await dir.create(recursive: true);
    final reportPath = p.join(config.outDir, 'bakeoff-report.json');
    await File(reportPath).writeAsString(const JsonEncoder.withIndent('  ').convert(report));
    final eventsPath = p.join(config.outDir, 'bakeoff-events.jsonl');
    final sink = File(eventsPath).openWrite();
    for (final e in events) {
      sink.writeln(jsonEncode(e));
    }
    await sink.close();
    stderr.writeln('wrote $reportPath and $eventsPath');
  }
}
