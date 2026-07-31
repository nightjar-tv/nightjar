import 'dart:ffi';
import 'dart:io';

import 'package:ffi/ffi.dart';

/// Observable surfaces for the ABR-signal audit.
class LibVlcSignals {
  const LibVlcSignals({
    required this.hasMediaStatePlaying,
    required this.hasBuffering,
    required this.hasEncounteredError,
    required this.hasTimeChanged,
    required this.hasPositionChanged,
    required this.hasStats,
    required this.notes,
  });

  final bool hasMediaStatePlaying;
  final bool hasBuffering;
  final bool hasEncounteredError;
  final bool hasTimeChanged;
  final bool hasPositionChanged;
  final bool hasStats;
  final String notes;

  Map<String, Object?> toJson() => {
        'hasMediaStatePlaying': hasMediaStatePlaying,
        'hasBuffering': hasBuffering,
        'hasEncounteredError': hasEncounteredError,
        'hasTimeChanged': hasTimeChanged,
        'hasPositionChanged': hasPositionChanged,
        'hasStats': hasStats,
        'notes': notes,
      };
}

typedef _LibvlcNewNative = Pointer<Void> Function(
  Int32,
  Pointer<Pointer<Utf8>>,
);
typedef _LibvlcNewDart = Pointer<Void> Function(int, Pointer<Pointer<Utf8>>);

typedef _LibvlcReleaseNative = Void Function(Pointer<Void>);
typedef _LibvlcReleaseDart = void Function(Pointer<Void>);

typedef _MediaNewLocationNative = Pointer<Void> Function(
  Pointer<Void>,
  Pointer<Utf8>,
);
typedef _MediaNewLocationDart = Pointer<Void> Function(
  Pointer<Void>,
  Pointer<Utf8>,
);

typedef _MediaReleaseNative = Void Function(Pointer<Void>);
typedef _MediaReleaseDart = void Function(Pointer<Void>);

typedef _PlayerNewNative = Pointer<Void> Function(Pointer<Void>);
typedef _PlayerNewDart = Pointer<Void> Function(Pointer<Void>);

typedef _PlayerReleaseNative = Void Function(Pointer<Void>);
typedef _PlayerReleaseDart = void Function(Pointer<Void>);

typedef _PlayerSetMediaNative = Void Function(Pointer<Void>, Pointer<Void>);
typedef _PlayerSetMediaDart = void Function(Pointer<Void>, Pointer<Void>);

typedef _PlayerPlayNative = Int32 Function(Pointer<Void>);
typedef _PlayerPlayDart = int Function(Pointer<Void>);

typedef _PlayerStopNative = Void Function(Pointer<Void>);
typedef _PlayerStopDart = void Function(Pointer<Void>);

typedef _PlayerGetStateNative = Int32 Function(Pointer<Void>);
typedef _PlayerGetStateDart = int Function(Pointer<Void>);

typedef _PlayerGetTimeNative = Int64 Function(Pointer<Void>);
typedef _PlayerGetTimeDart = int Function(Pointer<Void>);

typedef _PlayerSetTimeNative = Int32 Function(Pointer<Void>, Int64);
typedef _PlayerSetTimeDart = int Function(Pointer<Void>, int);

typedef _PlayerGetLengthNative = Int64 Function(Pointer<Void>);
typedef _PlayerGetLengthDart = int Function(Pointer<Void>);

typedef _EventAttachNative = Int32 Function(
  Pointer<Void>,
  Int32,
  Pointer<NativeFunction<Void Function(Pointer<Void>, Pointer<Void>)>>,
  Pointer<Void>,
);
typedef _EventAttachDart = int Function(
  Pointer<Void>,
  int,
  Pointer<NativeFunction<Void Function(Pointer<Void>, Pointer<Void>)>>,
  Pointer<Void>,
);

typedef _PlayerEventManagerNative = Pointer<Void> Function(Pointer<Void>);
typedef _PlayerEventManagerDart = Pointer<Void> Function(Pointer<Void>);

// libvlc_state_t
const int libvlcNothingSpecial = 0;
const int libvlcOpening = 1;
const int libvlcBuffering = 2;
const int libvlcPlaying = 3;
const int libvlcPaused = 4;
const int libvlcStopped = 5;
const int libvlcEnded = 6;
const int libvlcError = 7;

// Events
const int libvlcMediaPlayerPlaying = 0x104;
const int libvlcMediaPlayerBuffering = 0x103;
const int libvlcMediaPlayerEncounteredError = 0x10a;
const int libvlcMediaPlayerTimeChanged = 0x10b;
const int libvlcMediaPlayerPositionChanged = 0x10c;

DynamicLibrary _openLibvlc() {
  const candidates = [
    '/Applications/VLC.app/Contents/MacOS/lib/libvlc.dylib',
    '/Applications/VLC.app/Contents/MacOS/libvlc.dylib',
  ];
  for (final path in candidates) {
    if (File(path).existsSync()) {
      return DynamicLibrary.open(path);
    }
  }
  throw StateError('libvlc.dylib not found under VLC.app');
}

/// Headless libVLC player for bake-off timings and signal enumeration.
class LibvlcBakeoffPlayer {
  LibvlcBakeoffPlayer() {
    _lib = _openLibvlc();
    _libvlcNew = _lib.lookupFunction<_LibvlcNewNative, _LibvlcNewDart>('libvlc_new');
    _libvlcRelease =
        _lib.lookupFunction<_LibvlcReleaseNative, _LibvlcReleaseDart>('libvlc_release');
    _mediaNew = _lib.lookupFunction<_MediaNewLocationNative, _MediaNewLocationDart>(
      'libvlc_media_new_location',
    );
    _mediaRelease =
        _lib.lookupFunction<_MediaReleaseNative, _MediaReleaseDart>('libvlc_media_release');
    _playerNew =
        _lib.lookupFunction<_PlayerNewNative, _PlayerNewDart>('libvlc_media_player_new');
    _playerRelease = _lib
        .lookupFunction<_PlayerReleaseNative, _PlayerReleaseDart>('libvlc_media_player_release');
    _setMedia = _lib.lookupFunction<_PlayerSetMediaNative, _PlayerSetMediaDart>(
      'libvlc_media_player_set_media',
    );
    _play =
        _lib.lookupFunction<_PlayerPlayNative, _PlayerPlayDart>('libvlc_media_player_play');
    _stop =
        _lib.lookupFunction<_PlayerStopNative, _PlayerStopDart>('libvlc_media_player_stop');
    _getState = _lib.lookupFunction<_PlayerGetStateNative, _PlayerGetStateDart>(
      'libvlc_media_player_get_state',
    );
    _getTime = _lib.lookupFunction<_PlayerGetTimeNative, _PlayerGetTimeDart>(
      'libvlc_media_player_get_time',
    );
    _setTime = _lib.lookupFunction<_PlayerSetTimeNative, _PlayerSetTimeDart>(
      'libvlc_media_player_set_time',
    );
    _getLength = _lib.lookupFunction<_PlayerGetLengthNative, _PlayerGetLengthDart>(
      'libvlc_media_player_get_length',
    );
    _eventManager = _lib.lookupFunction<_PlayerEventManagerNative, _PlayerEventManagerDart>(
      'libvlc_media_player_event_manager',
    );
    _eventAttach =
        _lib.lookupFunction<_EventAttachNative, _EventAttachDart>('libvlc_event_attach');

    final argv = <String>['--no-video', '--quiet', '--no-audio'];
    final argc = argv.length;
    final ptrs = calloc<Pointer<Utf8>>(argc);
    final kept = <Pointer<Utf8>>[];
    for (var i = 0; i < argc; i++) {
      final p = argv[i].toNativeUtf8();
      kept.add(p);
      ptrs[i] = p;
    }
    _instance = _libvlcNew(argc, ptrs);
    for (final p in kept) {
      calloc.free(p);
    }
    calloc.free(ptrs);
    if (_instance == nullptr) {
      throw StateError('libvlc_new failed');
    }
    _player = _playerNew(_instance);
    if (_player == nullptr) {
      throw StateError('libvlc_media_player_new failed');
    }
  }

  late final DynamicLibrary _lib;
  late final _LibvlcNewDart _libvlcNew;
  late final _LibvlcReleaseDart _libvlcRelease;
  late final _MediaNewLocationDart _mediaNew;
  late final _MediaReleaseDart _mediaRelease;
  late final _PlayerNewDart _playerNew;
  late final _PlayerReleaseDart _playerRelease;
  late final _PlayerSetMediaDart _setMedia;
  late final _PlayerPlayDart _play;
  late final _PlayerStopDart _stop;
  late final _PlayerGetStateDart _getState;
  late final _PlayerGetTimeDart _getTime;
  late final _PlayerSetTimeDart _setTime;
  late final _PlayerGetLengthDart _getLength;
  late final _PlayerEventManagerDart _eventManager;
  late final _EventAttachDart _eventAttach;

  late final Pointer<Void> _instance;
  late final Pointer<Void> _player;
  Pointer<Void>? _media;

  bool sawPlaying = false;
  bool sawBuffering = false;
  bool sawError = false;
  int bufferingLastPct = 0;

  static const signals = LibVlcSignals(
    hasMediaStatePlaying: true,
    hasBuffering: true,
    hasEncounteredError: true,
    hasTimeChanged: true,
    hasPositionChanged: true,
    hasStats: true,
    notes:
        'libvlc_media_player_get_state (Playing/Buffering/Error); '
        'libvlc_event_attach Playing/Buffering/EncounteredError/TimeChanged; '
        'libvlc_media_player_get_stats for input bitrate. '
        'Bake-off FFI exposes state polling + event attach; no Flutter texture.',
  );

  void open(String url) {
    stop();
    sawPlaying = false;
    sawBuffering = false;
    sawError = false;
    final urlPtr = url.toNativeUtf8();
    _media = _mediaNew(_instance, urlPtr);
    calloc.free(urlPtr);
    if (_media == nullptr) {
      throw StateError('libvlc_media_new_location failed for $url');
    }
    _setMedia(_player, _media!);
  }

  Future<bool> playAndWaitFirstFrame({
    Duration timeout = const Duration(seconds: 30),
  }) async {
    final rc = _play(_player);
    if (rc != 0) {
      return false;
    }
    final deadline = DateTime.now().add(timeout);
    while (DateTime.now().isBefore(deadline)) {
      final state = _getState(_player);
      if (state == libvlcPlaying) {
        sawPlaying = true;
        return true;
      }
      if (state == libvlcBuffering) {
        sawBuffering = true;
      }
      if (state == libvlcError) {
        sawError = true;
        return false;
      }
      await Future<void>.delayed(const Duration(milliseconds: 20));
    }
    return false;
  }

  Future<bool> seekMs(int ms, {Duration timeout = const Duration(seconds: 30)}) async {
    final before = DateTime.now();
    _setTime(_player, ms);
    final deadline = before.add(timeout);
    while (DateTime.now().isBefore(deadline)) {
      final state = _getState(_player);
      if (state == libvlcError) {
        sawError = true;
        return false;
      }
      final t = _getTime(_player);
      if (state == libvlcPlaying && t >= 0 && (t - ms).abs() < 5000) {
        return true;
      }
      if (state == libvlcBuffering) {
        sawBuffering = true;
      }
      await Future<void>.delayed(const Duration(milliseconds: 20));
    }
    return false;
  }

  int get timeMs => _getTime(_player);
  int get lengthMs => _getLength(_player);
  int get state => _getState(_player);

  void stop() {
    _stop(_player);
    if (_media != null) {
      _mediaRelease(_media!);
      _media = null;
    }
  }

  void dispose() {
    stop();
    _playerRelease(_player);
    _libvlcRelease(_instance);
  }
}
