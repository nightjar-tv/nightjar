import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import {
	waitForSessionReady,
	type SessionWaitDeps,
	type SessionWaitOutcome
} from '../src/lib/sessionWait.ts';

const URL = 'http://nightjar.test/items/7/sessions/playlist.m3u8';

/** Minimal Response for the fetch mock; the wait only reads `ok`/`status`. */
function response(status: number): Response {
	return { ok: status >= 200 && status < 300, status } as Response;
}

describe('waitForSessionReady', () => {
	it('adopts the session as soon as the playlist answers ok', async () => {
		let releases = 0;
		const outcome = await waitForSessionReady(URL, {
			alive: () => true,
			release: () => releases++,
			fetchImpl: async () => response(200)
		});
		assert.equal(outcome.state, 'ready');
		assert.equal(releases, 0);
	});

	it('releases the session when the playlist never becomes servable', async () => {
		// Negative control for the session-leak fix: deleting the module's
		// release() call makes this test go red.
		let releases = 0;
		let polls = 0;
		const outcome = await waitForSessionReady(
			URL,
			{
				alive: () => true,
				release: () => releases++,
				fetchImpl: async () => {
					polls++;
					return response(503);
				}
			},
			3,
			0
		);
		assert.equal(outcome.state, 'released');
		assert.equal(releases, 1);
		assert.equal(polls, 3);
		if (outcome.state === 'released') assert.equal(outcome.reason, 'gave-up');
	});

	it('does not wait out the budget on a 404: it releases immediately', async () => {
		let releases = 0;
		let polls = 0;
		const outcome = await waitForSessionReady(
			URL,
			{
				alive: () => true,
				release: () => releases++,
				fetchImpl: async () => {
					polls++;
					return response(404);
				}
			},
			10,
			0
		);
		assert.equal(outcome.state, 'released');
		assert.equal(releases, 1);
		assert.equal(polls, 1);
		if (outcome.state === 'released') assert.equal(outcome.reason, 'gone');
	});

	it('keeps polling past a transient 503 until the playlist is servable', async () => {
		let releases = 0;
		let polls = 0;
		const outcome = await waitForSessionReady(
			URL,
			{
				alive: () => true,
				release: () => releases++,
				fetchImpl: async () => {
					polls++;
					return response(polls < 3 ? 503 : 200);
				}
			},
			10,
			0
		);
		assert.equal(outcome.state, 'ready');
		assert.equal(releases, 0);
		assert.equal(polls, 3);
	});

	it('a rejected fetch ends as a network verdict instead of throwing', async () => {
		// The unhandled-rejection bug: a fetch that throws must not reject out
		// of the wait, or the caller stays on its "starting…" state forever.
		let releases = 0;
		let outcome: SessionWaitOutcome | undefined;
		await assert.doesNotReject(async () => {
			outcome = await waitForSessionReady(
				URL,
				{
					alive: () => true,
					release: () => releases++,
					fetchImpl: async () => {
						throw new TypeError('Failed to fetch');
					}
				},
				3,
				0
			);
		});
		assert.deepEqual(outcome, { state: 'released', reason: 'network' });
		assert.equal(releases, 1);
	});

	it('a network error that clears mid-wait still ends ready', async () => {
		let releases = 0;
		let polls = 0;
		const outcome = await waitForSessionReady(
			URL,
			{
				alive: () => true,
				release: () => releases++,
				fetchImpl: async () => {
					polls++;
					if (polls === 1) throw new TypeError('Failed to fetch');
					return response(200);
				}
			},
			5,
			0
		);
		assert.equal(outcome.state, 'ready');
		assert.equal(releases, 0);
	});

	it('stops polling and releases when the owning page dies', async () => {
		let releases = 0;
		let polls = 0;
		let alive = true;
		const deps: SessionWaitDeps = {
			alive: () => alive,
			release: () => releases++,
			fetchImpl: async () => {
				polls++;
				return response(503);
			}
		};
		// intervalMs 20 paces the loop so the page can die between polls, as
		// an unmount does while the real 200 ms cadence runs.
		const pending = waitForSessionReady(URL, deps, 5, 20);
		await new Promise((r) => setTimeout(r, 5));
		alive = false;
		const outcome = await pending;
		assert.equal(outcome.state, 'released');
		assert.equal(releases, 1);
		assert.equal(polls, 1);
	});
});
