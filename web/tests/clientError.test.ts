import { afterEach, describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

// client.ts imports '../session' with no extension, which Node will not
// resolve for an ESM module. Register the repo's bundler-style hook before
// the dynamic import below so the module under test loads unchanged.
register('./ts-specifier-hook.mjs', import.meta.url);

const { api } = await import('../src/lib/api/client.ts');

const realFetch = globalThis.fetch;

function stubFetch(status: number, init: { statusText?: string; body?: string }) {
	globalThis.fetch = async () =>
		new Response(init.body ?? '', {
			status,
			statusText: init.statusText ?? '',
			headers: { 'Content-Type': 'text/html' }
		});
}

afterEach(() => {
	globalThis.fetch = realFetch;
});

describe('api error message fallback', () => {
	it('names the HTTP status when the error body is not the JSON envelope', async () => {
		// A reverse-proxy 502 over HTTP/2 arrives with an HTML body and an
		// empty statusText. statusText alone would throw Error(''), which
		// renders as a blank error paragraph.
		stubFetch(502, { body: '<html>Bad gateway</html>' });
		const err = await api.listLibraries().then(
			() => assert.fail('expected a thrown error'),
			(e: Error & { status?: number }) => e
		);
		assert.equal(err.message, 'HTTP 502');
		assert.equal(err.status, 502);
	});

	it('keeps the server JSON envelope sentence and code over the fallback', async () => {
		stubFetch(422, {
			body: JSON.stringify({ error: 'name is required', code: 'bad_request' })
		});
		const err = await api.listLibraries().then(
			() => assert.fail('expected a thrown error'),
			(e: Error & { code?: string; status?: number }) => e
		);
		assert.equal(err.message, 'name is required');
		assert.equal(err.code, 'bad_request');
		assert.equal(err.status, 422);
	});

	it('exposes the status so the gate can classify a dead credential', async () => {
		// The sign-in gate clears the token only on a 401 (sessionGate.ts),
		// so the 401 and its machine code have to survive the throw.
		stubFetch(401, {
			body: JSON.stringify({
				error: 'session_expired: session has expired',
				code: 'unauthorized'
			})
		});
		const err = await api.listLibraries().then(
			() => assert.fail('expected a thrown error'),
			(e: Error & { code?: string; status?: number }) => e
		);
		assert.equal(err.status, 401);
		assert.equal(err.code, 'unauthorized');
		assert.equal(err.message, 'session_expired: session has expired');
	});

	it('keeps a non-empty HTTP/1 reason phrase for a non-JSON error body', async () => {
		stubFetch(503, { statusText: 'Service Unavailable', body: '<html>down</html>' });
		const err = await api.listLibraries().then(
			() => assert.fail('expected a thrown error'),
			(e: Error & { status?: number }) => e
		);
		assert.equal(err.message, 'Service Unavailable');
		assert.equal(err.status, 503);
	});
});
