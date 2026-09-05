/**
 * Absolute cue restore after hls.js sticky baseline (ADR-0013).
 *
 *   npm test
 */
import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import {
	applyAbsoluteCueTimesFromVtt,
	mediaPlaylistUrlFromMaster,
	parseSubtitleTrackIdsFromMaster
} from '../src/lib/nativeHlsSubs.ts';

describe('parseSubtitleTrackIdsFromMaster', () => {
	it('reads path-absolute session subtitle URIs (ADR-0008)', () => {
		const master = `#EXTM3U
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI="/api/v0/sessions/s1/subs/e2.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=5000000,SUBTITLES="subs"
/api/v0/sessions/s1/index.m3u8
`;
		assert.deepEqual(parseSubtitleTrackIdsFromMaster(master), ['e2']);
	});

	it('still accepts legacy relative ../../subs/ URIs', () => {
		const master = `#EXTM3U
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI="../../subs/e2.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=5000000,SUBTITLES="subs"
index.m3u8
`;
		assert.deepEqual(parseSubtitleTrackIdsFromMaster(master), ['e2']);
	});
});

describe('applyAbsoluteCueTimesFromVtt', () => {
	it('rewrites doubled cue times back to wire absolute by id', () => {
		const body =
			'WEBVTT\n\n640764\n00:10:40.764 --> 00:10:44.000\nHello\n';
		const cues: Array<{
			id: string;
			startTime: number;
			endTime: number;
		}> = [];
		const track = {
			cues: {
				getCueById(id: string) {
					return cues.find((c) => c.id === id) ?? null;
				}
			}
		} as unknown as TextTrack;
		// hls.js sticky baseline 636 on title-absolute wire.
		cues.push({ id: '640764', startTime: 1276.792, endTime: 1280.028 });
		const fixed = applyAbsoluteCueTimesFromVtt(track, body);
		assert.equal(fixed, 1);
		assert.ok(Math.abs(cues[0]!.startTime - 640.764) < 0.001);
		assert.ok(Math.abs(cues[0]!.endTime - 644.0) < 0.001);
	});

	it('no-ops when times already match the wire', () => {
		const body =
			'WEBVTT\n\n1000\n00:00:01.000 --> 00:00:02.000\nHi\n';
		const cue = { id: '1000', startTime: 1, endTime: 2 };
		const track = {
			cues: {
				getCueById(id: string) {
					return id === cue.id ? cue : null;
				}
			}
		} as unknown as TextTrack;
		assert.equal(applyAbsoluteCueTimesFromVtt(track, body), 0);
		assert.equal(cue.startTime, 1);
	});

	it('shifts wire times into the media window using live landedMs', () => {
		const body =
			'WEBVTT\n\n600500\n00:10:00.500 --> 00:10:02.000\nMid\n';
		const cue = { id: '600500', startTime: 600.5, endTime: 602 };
		const track = {
			cues: {
				getCueById(id: string) {
					return id === cue.id ? cue : null;
				}
			}
		} as unknown as TextTrack;
		const fixed = applyAbsoluteCueTimesFromVtt(track, body, 600_000);
		assert.equal(fixed, 1);
		assert.ok(Math.abs(cue.startTime - 0.5) < 0.001);
		assert.ok(Math.abs(cue.endTime - 2) < 0.001);
	});
});

describe('mediaPlaylistUrlFromMaster', () => {
	// Wire shape emitted by the server's `build_master` (hls_master.rs): one
	// #EXT-X-STREAM-INF per rendition, followed by the rendition's media
	// playlist URI, path-absolute under /api/v0/sessions/{id}/v/{rung}/
	// (ADR-0008, ADR-0051 amendment 1).
	it('returns the rung-scoped URI the master advertises', () => {
		const master = `#EXTM3U
#EXT-X-VERSION:7
#EXT-X-STREAM-INF:BANDWIDTH=5000000
/api/v0/sessions/s1/v/single/index.m3u8
`;
		assert.equal(
			mediaPlaylistUrlFromMaster(master),
			'/api/v0/sessions/s1/v/single/index.m3u8'
		);
	});

	it('skips the SUBTITLES group line and names the video rendition', () => {
		const master = `#EXTM3U
#EXT-X-VERSION:7
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI="/api/v0/sessions/s1/subs/e2.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=5000000,SUBTITLES="subs"
/api/v0/sessions/s1/v/single/index.m3u8
`;
		assert.equal(
			mediaPlaylistUrlFromMaster(master),
			'/api/v0/sessions/s1/v/single/index.m3u8'
		);
	});

	it('is never the flat alias a master-filename rewrite names', () => {
		// The readiness probe must poll the URI the master advertises, not the
		// string surgery that rewrote `master.m3u8` to `index.m3u8` — that
		// flat form is a compatibility alias and stops matching the rendition
		// being resumed the moment a second one exists.
		const masterUrl = '/api/v0/sessions/s1/master.m3u8';
		const advertised = mediaPlaylistUrlFromMaster(`#EXTM3U
#EXT-X-VERSION:7
#EXT-X-STREAM-INF:BANDWIDTH=5000000
/api/v0/sessions/s1/v/single/index.m3u8
`);
		const flatAlias = masterUrl.replace(/master\.m3u8$/i, 'index.m3u8');
		assert.equal(flatAlias, '/api/v0/sessions/s1/index.m3u8');
		assert.notEqual(advertised, flatAlias);
	});

	it('returns null for a master that names no video rendition', () => {
		const subtitleOnly = `#EXTM3U
#EXT-X-VERSION:7
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI="/api/v0/sessions/s1/subs/e2.m3u8"
`;
		assert.equal(mediaPlaylistUrlFromMaster(subtitleOnly), null);
		assert.equal(mediaPlaylistUrlFromMaster(''), null);
	});

	it('returns null when the STREAM-INF line ends the master', () => {
		const truncated = `#EXTM3U
#EXT-X-VERSION:7
#EXT-X-STREAM-INF:BANDWIDTH=5000000
`;
		assert.equal(mediaPlaylistUrlFromMaster(truncated), null);
	});
});
