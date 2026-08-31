import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import {
	mediaSecondsFromTitle,
	mediaTimeInProducedWindow,
	scrubRangeMs,
	titleSecondsFromMedia
} from '../src/lib/hlsTimeline.ts';

describe('hlsTimeline', () => {
	it('scrubRangeMs prefers usable extent when damaged', () => {
		assert.equal(scrubRangeMs(1_354_496, 383_461), 383_461);
		assert.equal(scrubRangeMs(5_768_768, null), 5_768_768);
		assert.equal(scrubRangeMs(null, undefined), 0);
	});

	it('title/media transform around a per-run listing origin', () => {
		// ADR-0020's per-run playlist begins at the land, so the origin is the
		// land and element time is window-relative.
		const origin = 600_000;
		assert.equal(titleSecondsFromMedia(2, origin), 602);
		assert.equal(mediaSecondsFromTitle(602, origin), 2);
		assert.equal(mediaSecondsFromTitle(600, origin), 0);
		assert.equal(mediaSecondsFromTitle(120, origin), 0); // before origin clamps
		assert.equal(titleSecondsFromMedia(0, 0), 0);
	});

	it('a full-title listing has origin 0 however far in the session landed', () => {
		// ADR-0054: the playlist starts at 0 and says the land in EXT-X-START,
		// so element time is already title-absolute. Reading the land as the
		// origin here is the defect: a session landed at 600 s showed 20:02 on
		// a 15-minute title (OPEN-DEFECTS entry 12).
		const origin = 0;
		assert.equal(titleSecondsFromMedia(602.833, origin), 602.833);
		assert.equal(titleSecondsFromMedia(620.578, origin), 620.578);
		assert.equal(mediaSecondsFromTitle(640, origin), 640);
		// And the scrub does not undershoot by the land.
		assert.equal(mediaSecondsFromTitle(620, origin), 620);
	});

	it('produced window uses seekable end', () => {
		const seekable = {
			length: 1,
			start: () => 0,
			end: () => 40
		} as unknown as TimeRanges;
		const empty = { length: 0 } as unknown as TimeRanges;
		assert.equal(mediaTimeInProducedWindow(10, seekable, empty, NaN), true);
		assert.equal(mediaTimeInProducedWindow(41, seekable, empty, NaN), false);
		assert.equal(mediaTimeInProducedWindow(0.1, empty, empty, NaN), true);
		assert.equal(mediaTimeInProducedWindow(2, empty, empty, NaN), false);
	});
});
