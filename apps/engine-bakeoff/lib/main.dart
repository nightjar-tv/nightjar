import 'dart:convert';
import 'dart:io';

import 'package:engine_bakeoff/measure.dart';
import 'package:flutter/material.dart';
import 'package:media_kit/media_kit.dart';
import 'package:media_kit_video/media_kit_video.dart';
import 'package:path/path.dart' as p;

Future<void> main(List<String> args) async {
  WidgetsFlutterBinding.ensureInitialized();
  MediaKit.ensureInitialized();

  final repoRoot = _findRepoRoot();
  final base = Platform.environment['NIGHTJAR_BASE'] ?? 'http://127.0.0.1:18097';
  final outDir = Platform.environment['BAKEOFF_OUT'] ??
      p.join(repoRoot, 'notes', 'client-arch', 'bakeoff-runs');
  final samplePath = p.join(repoRoot, 'notes', 'client-arch', 'bakeoff-sample.json');
  final auto = args.contains('--auto') || Platform.environment['BAKEOFF_AUTO'] == '1';
  final bindingOnly = Platform.environment['BAKEOFF_BINDING'] == '1';

  final config = BakeoffConfig(baseUrl: base, samplePath: samplePath, outDir: outDir);

  if (auto) {
    await _runAuto(config, bindingOnly: bindingOnly);
    exit(0);
  }

  runApp(BakeoffApp(config: config));
}

String _findRepoRoot() {
  var dir = Directory.current;
  for (var i = 0; i < 8; i++) {
    if (File(p.join(dir.path, 'ENGINEERING_RULES.md')).existsSync()) {
      return dir.path;
    }
    dir = dir.parent;
  }
  // When launched from .app, cwd may be /; walk from executable.
  dir = File(Platform.resolvedExecutable).parent;
  for (var i = 0; i < 12; i++) {
    if (File(p.join(dir.path, 'ENGINEERING_RULES.md')).existsSync()) {
      return dir.path;
    }
    dir = dir.parent;
  }
  return Directory.current.path;
}

Future<void> _runAuto(BakeoffConfig config, {required bool bindingOnly}) async {
  final runner = BakeoffRunner(config);
  final report = <String, Object?>{
    'baseUrl': config.baseUrl,
    'client': 'flutter_bindings',
    'url_resolution_note':
        'Part A uses dp_byte_serve /items/{id}/stream; Nightjar /stream is BROWSER_V0-gated',
    'abr_signals': await runner.auditAbrSignals(),
  };

  // Engine A: media_kit / libmpv through Flutter binding
  report['part_a_mediakit'] = await runner.runPartALatency('mpv');
  if (bindingOnly) {
    report['t4_mediakit'] = await runner.runT4('mpv', limit: 40);
  } else {
    report['part_a_vlc'] = await runner.runPartALatency('vlc');
    report['t4_mediakit'] = await runner.runT4('mpv');
    report['t4_vlc'] = await runner.runT4('vlc');
    report['part_b_mpv'] = await runner.runPartB('mpv');
  }

  final outName = bindingOnly ? 'bakeoff-report-binding.json' : 'bakeoff-report.json';
  final dir = Directory(config.outDir);
  await dir.create(recursive: true);
  await File(p.join(config.outDir, outName))
      .writeAsString(const JsonEncoder.withIndent('  ').convert(report));
  await runner.writeOutputs(report);
  stdout.writeln(jsonEncode({'ok': true, 'out': config.outDir, 'bindingOnly': bindingOnly}));
}

class BakeoffApp extends StatefulWidget {
  const BakeoffApp({super.key, required this.config});

  final BakeoffConfig config;

  @override
  State<BakeoffApp> createState() => _BakeoffAppState();
}

class _BakeoffAppState extends State<BakeoffApp> {
  late final Player _player = Player();
  late final VideoController _controller = VideoController(_player);
  String _status = 'idle';
  String _engine = 'mpv';

  @override
  void dispose() {
    _player.dispose();
    super.dispose();
  }

  Future<void> _openDemo() async {
    final sample = jsonDecode(await File(widget.config.samplePath).readAsString())
        as Map<String, dynamic>;
    final id = (sample['latency_item_ids'] as List).first as int;
    final url = widget.config.streamUrl(id);
    setState(() => _status = 'opening $url');
    await _player.open(Media(url));
    setState(() => _status = 'playing item $id via $_engine (media_kit)');
  }

  Future<void> _runSuite() async {
    setState(() => _status = 'running binding suite…');
    await _runAuto(widget.config, bindingOnly: true);
    setState(() => _status = 'suite done → ${widget.config.outDir}');
  }

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Nightjar engine bake-off',
      home: Scaffold(
        appBar: AppBar(title: const Text('Engine bake-off (measurement only)')),
        body: Column(
          children: [
            Expanded(child: Video(controller: _controller)),
            Padding(
              padding: const EdgeInsets.all(12),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  Text(_status),
                  const SizedBox(height: 8),
                  Wrap(
                    spacing: 8,
                    children: [
                      ElevatedButton(onPressed: _openDemo, child: const Text('Open DP demo')),
                      ElevatedButton(onPressed: _runSuite, child: const Text('Run binding suite')),
                      DropdownButton<String>(
                        value: _engine,
                        items: const [
                          DropdownMenuItem(value: 'mpv', child: Text('media_kit / libmpv')),
                          DropdownMenuItem(value: 'vlc', child: Text('libvlc FFI')),
                        ],
                        onChanged: (v) => setState(() => _engine = v ?? 'mpv'),
                      ),
                    ],
                  ),
                ],
              ),
            ),
          ],
        ),
      ),
    );
  }
}
