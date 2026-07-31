/// Thin libVLC FFI for bake-off measurement only.
///
/// Links `/Applications/VLC.app/Contents/MacOS/lib/libvlc.dylib`.
/// No Flutter texture — open / seek / playing / stall signals only.
/// That missing surface path is itself a T3 finding.
library;

export 'src/libvlc_player.dart';
