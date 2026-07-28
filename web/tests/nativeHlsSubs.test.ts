/**
 * Absolute cue restore after hls.js sticky baseline (ADR-0013).
 *
 *   npm test
 */
import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { applyAbsoluteCueTimesFromVtt } from '../src/lib/nativeHlsSubs.ts';

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
});
