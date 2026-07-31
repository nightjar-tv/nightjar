/**
 * Absolute cue restore after hls.js sticky baseline (ADR-0013).
 *
 *   npm test
 */
import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import {
	applyAbsoluteCueTimesFromVtt,
	parseSubtitleTrackIdsFromMaster
} from '../src/lib/nativeHlsSubs.ts';

describe('parseSubtitleTrackIdsFromMaster', () => {
	it('reads path-absolute session subtitle URIs (ADR-0008)', () => {
		const master = `#EXTM3U
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI="/api/v0/sessions/s1/subs/e2.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=5000000,SUBTITLES="subs"
/api/v0/sessions/s1/runs/0/index.m3u8
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
