/**
 * Whether a failed session create is worth retrying.
 *
 * POST /sessions answers 503 when the server has no encoder capacity. The
 * stable signal is the body's `code`, `admission_refused`, declared on the
 * server in `server/crates/api/src/routes/sessions.rs`
 * (`ADMISSION_REFUSED_CODE`). The retry matches the code, never the sentence:
 * the sentence is copy, not an API, and rewording it must not stop retries.
 *
 * The prose match below is a fallback for one release, so a client talking to
 * a server that predates the `code` field still retries. Remove it once no
 * supported server omits the field.
 */
export const ADMISSION_REFUSED_CODE = 'admission_refused';

export function shouldRetrySessionStart(e: unknown): boolean {
	if (e && typeof e === 'object' && (e as { code?: unknown }).code === ADMISSION_REFUSED_CODE) {
		return true;
	}
	const msg = e instanceof Error ? e.message : String(e);
	return msg.includes('retry shortly') || msg.includes('in use');
}
