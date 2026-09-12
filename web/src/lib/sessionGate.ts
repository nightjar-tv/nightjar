/**
 * The sign-in gate's decision, pure so it can be checked without a browser.
 *
 * A failed setup or session check is not proof that the stored credential is
 * bad. A transport error, a 5xx, and a 403 (the server knows the caller and
 * refuses the action) all leave the token alone and offer a retry. Only a 401
 * is definitive: the server answers `no_credential`, `session_unknown`,
 * `session_expired`, or `session_revoked`, all 401, and that is the one case
 * that clears the token and sends the person back to the sign-in form.
 *
 * The gate reads `status` off the thrown error, never the sentence, so
 * rewording server copy cannot change whether a credential is discarded.
 */

/** The layout's gate once a check has answered. */
export type Gate = 'bootstrap' | 'login' | 'in' | 'unavailable';

export interface GateDeps {
	getSetupState: () => Promise<{ adminExists: boolean }>;
	getSession: () => Promise<unknown>;
	hasToken: () => boolean;
	clearToken: () => void;
}

/** A 401 is the server rejecting the credential itself. */
export function isInvalidCredential(e: unknown): boolean {
	return (e as { status?: unknown } | null | undefined)?.status === 401;
}

export async function resolveGate(deps: GateDeps): Promise<Gate> {
	let adminExists: boolean;
	try {
		({ adminExists } = await deps.getSetupState());
	} catch {
		// The server did not answer, so nothing is known about the credential.
		// Keep it and let the surface retry.
		return 'unavailable';
	}
	if (!adminExists) return 'bootstrap';
	if (!deps.hasToken()) return 'login';
	try {
		await deps.getSession();
		return 'in';
	} catch (e) {
		if (isInvalidCredential(e)) {
			deps.clearToken();
			return 'login';
		}
		return 'unavailable';
	}
}
