import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { isInvalidCredential, resolveGate } from '../src/lib/sessionGate.ts';

function httpError(status: number, code?: string): Error & { status?: number; code?: string } {
	const error = new Error(`HTTP ${status}`) as Error & { status?: number; code?: string };
	error.status = status;
	if (code) error.code = code;
	return error;
}

/** A gate that keeps its token across checks, as the layout does. */
function gateHarness() {
	const state = { token: 'stored-token' as string | null, cleared: 0, sessions: 0 };
	return {
		state,
		deps: {
			getSetupState: async () => ({ adminExists: true }),
			getSession: async () => {
				state.sessions++;
				return {};
			},
			hasToken: () => state.token !== null,
			clearToken: () => {
				state.cleared++;
				state.token = null;
			}
		}
	};
}

describe('resolveGate', () => {
	it('sends a server with no account to bootstrap without touching the token', async () => {
		const h = gateHarness();
		h.deps.getSetupState = async () => ({ adminExists: false });
		assert.equal(await resolveGate(h.deps), 'bootstrap');
		assert.equal(h.state.cleared, 0);
		assert.equal(h.state.sessions, 0);
	});

	it('asks a signed-out visitor to log in', async () => {
		const h = gateHarness();
		h.state.token = null;
		assert.equal(await resolveGate(h.deps), 'login');
		assert.equal(h.state.sessions, 0);
	});

	it('accepts a valid session', async () => {
		const h = gateHarness();
		assert.equal(await resolveGate(h.deps), 'in');
		assert.equal(h.state.sessions, 1);
	});

	it('keeps the token when setup cannot reach the server', async () => {
		const h = gateHarness();
		h.deps.getSetupState = async () => {
			throw new TypeError('Failed to fetch');
		};
		assert.equal(await resolveGate(h.deps), 'unavailable');
		assert.equal(h.state.token, 'stored-token');
		assert.equal(h.state.cleared, 0);
	});

	it('keeps the token when setup answers 502 or 503', async () => {
		for (const status of [502, 503]) {
			const h = gateHarness();
			h.deps.getSetupState = async () => {
				throw httpError(status);
			};
			assert.equal(await resolveGate(h.deps), 'unavailable', `setup ${status}`);
			assert.equal(h.state.token, 'stored-token', `setup ${status}`);
			assert.equal(h.state.cleared, 0, `setup ${status}`);
		}
	});

	it('keeps the token on a session transport error', async () => {
		const h = gateHarness();
		h.deps.getSession = async () => {
			throw new TypeError('Failed to fetch');
		};
		assert.equal(await resolveGate(h.deps), 'unavailable');
		assert.equal(h.state.token, 'stored-token');
		assert.equal(h.state.cleared, 0);
	});

	it('keeps the token when session validation answers 502 or 503', async () => {
		for (const status of [502, 503]) {
			const h = gateHarness();
			h.deps.getSession = async () => {
				throw httpError(status);
			};
			assert.equal(await resolveGate(h.deps), 'unavailable', `session ${status}`);
			assert.equal(h.state.token, 'stored-token', `session ${status}`);
			assert.equal(h.state.cleared, 0, `session ${status}`);
		}
	});

	it('keeps the token when session validation answers 403', async () => {
		// Authenticated and refused is not a dead credential.
		const h = gateHarness();
		h.deps.getSession = async () => {
			throw httpError(403, 'insufficient_role');
		};
		assert.equal(await resolveGate(h.deps), 'unavailable');
		assert.equal(h.state.token, 'stored-token');
		assert.equal(h.state.cleared, 0);
	});

	it('clears the token only on a 401', async () => {
		for (const code of ['no_credential', 'session_unknown', 'session_expired', 'session_revoked']) {
			const h = gateHarness();
			h.deps.getSession = async () => {
				throw httpError(401, code);
			};
			assert.equal(await resolveGate(h.deps), 'login', code);
			assert.equal(h.state.token, null, code);
			assert.equal(h.state.cleared, 1, code);
		}
	});

	it('recovers on retry with the retained token, no password entry', async () => {
		// The point of the whole change: a 503 keeps the credential, and the
		// next check reuses it instead of forcing the form.
		const h = gateHarness();
		h.deps.getSession = async () => {
			throw httpError(503);
		};
		assert.equal(await resolveGate(h.deps), 'unavailable');
		assert.equal(h.state.token, 'stored-token');

		h.deps.getSession = async () => {
			h.state.sessions++;
			return {};
		};
		assert.equal(await resolveGate(h.deps), 'in');
		assert.equal(h.state.token, 'stored-token');
		assert.equal(h.state.cleared, 0);
	});
});

describe('isInvalidCredential', () => {
	it('is true only for a 401', () => {
		assert.equal(isInvalidCredential(httpError(401)), true);
		for (const status of [403, 500, 502, 503]) {
			assert.equal(isInvalidCredential(httpError(status)), false, `status ${status}`);
		}
		assert.equal(isInvalidCredential(new TypeError('Failed to fetch')), false);
		assert.equal(isInvalidCredential(undefined), false);
	});
});
