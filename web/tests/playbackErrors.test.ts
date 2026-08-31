import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { loadFailureAction } from '../src/lib/playbackErrors.ts';

describe('loadFailureAction', () => {
	it('stops on 404, which is the session being gone', () => {
		assert.equal(loadFailureAction(404), 'session-gone');
	});

	it('stops on a credential failure rather than retrying it', () => {
		// OPEN-DEFECTS entry 16: 401 fell through to the retry arm and produced
		// ~100 requests in 25 s with nothing shown.
		assert.equal(loadFailureAction(401), 'unauthorized');
		assert.equal(loadFailureAction(403), 'unauthorized');
	});

	it('retries everything a later request could answer differently', () => {
		for (const code of [500, 502, 503, 504, 0, undefined]) {
			assert.equal(loadFailureAction(code), 'retry', `status ${code}`);
		}
	});
});
