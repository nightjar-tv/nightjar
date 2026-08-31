import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { beginPlayback } from '../src/lib/profileScope.ts';

describe('beginPlayback ordering', () => {
	it('does not mark started while the narrow is still in flight', async () => {
		// The race control. Direct play mounts <video src> the moment `started`
		// is true, and /items/{id}/stream 403s an account-scope session, so a
		// markStarted that runs alongside the narrow is the defect.
		let started = false;
		let release!: () => void;
		const gate = new Promise<void>((r) => (release = r));

		const run = beginPlayback({
			ensureScope: () => gate,
			markStarted: () => (started = true)
		});

		await Promise.resolve();
		assert.equal(started, false, 'started before the narrow resolved');

		release();
		await run;
		assert.equal(started, true, 'never started after the narrow resolved');
	});

	it('does not mark started when the narrow fails', async () => {
		let started = false;
		await assert.rejects(
			beginPlayback({
				ensureScope: () => Promise.reject(new Error('account has no profile')),
				markStarted: () => (started = true)
			}),
			/no profile/
		);
		assert.equal(started, false);
	});
});
