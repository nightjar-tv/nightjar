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

	it('title/media transform around a mid-title land', () => {
		const land = 600_000;
		assert.equal(titleSecondsFromMedia(2, land), 602);
		assert.equal(mediaSecondsFromTitle(602, land), 2);
		assert.equal(mediaSecondsFromTitle(600, land), 0);
		assert.equal(mediaSecondsFromTitle(120, land), 0); // before land clamps
		assert.equal(titleSecondsFromMedia(0, 0), 0);
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
