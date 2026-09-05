import { readFileSync } from 'node:fs';
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import {
	ADMISSION_REFUSED_CODE,
	shouldRetrySessionStart
} from '../src/lib/sessionRetry.ts';

describe('session-create admission retry', () => {
	it('retries a 503 whose body carries the admission code', () => {
		// The message deliberately shares no words with the prose fallback,
		// so this passes only through the code. With the server sentence here
		// the test stayed green with the code match deleted, which made it
		// prove nothing about the field it exists to pin.
		const error = Object.assign(new Error('Service Unavailable'), {
			code: ADMISSION_REFUSED_CODE
		});
		assert.equal(shouldRetrySessionStart(error), true);
	});

	it('does not retry an error that carries a different code', () => {
		// Presence of a code is not the signal; this exact code is.
		const error = Object.assign(new Error('item 7 is not ready to play'), {
			code: 'unsupported_media_type'
		});
		assert.equal(shouldRetrySessionStart(error), false);
	});

	it('still retries on the old sentences for a server that predates the code', () => {
		assert.equal(
			shouldRetrySessionStart(
				new Error('playback capacity is temporarily unavailable; retry shortly')
			),
			true
		);
		assert.equal(shouldRetrySessionStart(new Error('session already in use')), true);
	});

	it('lets an unrelated failure surface as a hard error', () => {
		assert.equal(shouldRetrySessionStart(new Error('something else broke')), false);
	});

	it('the server constant still declares this code', () => {
		// Coupling pin: the watch page retries on ADMISSION_REFUSED_CODE, and
		// sessions.rs serialises that same constant into the 503 body.
		// Rewording the server sentence or changing one side alone must go red.
		const sourceUrl = new URL(
			'../../server/crates/api/src/routes/sessions.rs',
			import.meta.url
		);
		const source = readFileSync(sourceUrl, 'utf8');
		const declared = source.match(/pub const ADMISSION_REFUSED_CODE: &str = "([^"]+)"/);
		assert.ok(declared, 'sessions.rs no longer declares ADMISSION_REFUSED_CODE');
		assert.equal(declared[1], ADMISSION_REFUSED_CODE);
	});
});
