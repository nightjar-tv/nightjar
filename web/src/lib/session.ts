/** The demo client's credential, kept only so the dogfood UI still works.
 *
 * Throwaway on purpose. B2-2 made every endpoint need a session and the real
 * login lives in Block 3; this is the smallest thing that keeps the one UI we
 * look at real data through alive until then. No design intent, no persistence
 * beyond `localStorage`, no refresh handling — ADR-0034 item 5 has no refresh
 * token, so an expired session means logging in again and that is correct.
 *
 * The token also arrives as an `HttpOnly` cookie scoped to `/api/v0`, which is
 * what makes `<img>` and `<video>` work: those cannot send an `Authorization`
 * header, and the eight routes ADR-0034 item 9 enumerates accept the cookie
 * instead. Script never reads that cookie; this copy is the bearer one.
 */

const TOKEN_KEY = 'nj_token';

export function storedToken(): string | null {
	if (typeof localStorage === 'undefined') return null;
	return localStorage.getItem(TOKEN_KEY);
}

export function storeToken(token: string): void {
	localStorage.setItem(TOKEN_KEY, token);
}

export function clearToken(): void {
	localStorage.removeItem(TOKEN_KEY);
}

export function authHeaders(): Record<string, string> {
	const token = storedToken();
	return token ? { Authorization: `Bearer ${token}` } : {};
}
