import type { paths } from './schema';
import { authHeaders } from '../session';

type Resp<P extends keyof paths, M extends keyof paths[P]> = paths[P][M] extends {
	responses: {
		200: { content: { 'application/json': infer R } };
	};
}
	? R
	: paths[P][M] extends {
				responses: { 201: { content: { 'application/json': infer R } } };
		  }
		? R
		: paths[P][M] extends {
					responses: { 202: { content: { 'application/json': infer R } } };
			  }
			? R
			: never;

async function request<T>(path: string, init?: RequestInit): Promise<T> {
	const res = await fetch(path, {
		...init,
		headers: {
			Accept: 'application/json',
			...(init?.body ? { 'Content-Type': 'application/json' } : {}),
			// Every route but four needs this now (ADR-0034 item 11). Absent
			// when nobody is logged in, which is what the layout gate reacts to.
			...authHeaders(),
			...init?.headers
		}
	});
	if (!res.ok) {
		// `statusText` is the empty string over HTTP/2 and in several fetch
		// implementations, so alone it renders a blank error paragraph for
		// any error whose body is not our JSON envelope (a reverse-proxy
		// 502, a 413, an HTML error page). Say the status when the reason
		// phrase is absent; the envelope's own sentence still wins.
		let message = res.statusText || `HTTP ${res.status}`;
		let code: string | undefined;
		try {
			const body = (await res.json()) as { error?: string; code?: string };
			if (body.error) message = body.error;
			code = body.code;
		} catch {
			/* ignore */
		}
		// Carry the server's machine code and HTTP status beside the sentence.
		// The watch page's retry decision matches `code`; the sign-in gate
		// classifies on `status`, because a 401 is a dead credential and a
		// 5xx or 403 is not. Neither reads the wording of `message`. Plain
		// Error keeps one thrown-error path.
		const error = new Error(message) as Error & { code?: string; status?: number };
		if (code) error.code = code;
		error.status = res.status;
		throw error;
	}
	if (res.status === 204) return undefined as T;
	return (await res.json()) as T;
}

export const api = {
	listLibraries: () =>
		request<Resp<'/api/v0/libraries', 'get'>>('/api/v0/libraries'),
	createLibrary: (body: { name: string; path: string; kind: 'movies' | 'shows' }) =>
		request<Resp<'/api/v0/libraries', 'post'>>('/api/v0/libraries', {
			method: 'POST',
			body: JSON.stringify(body)
		}),
	getLibrary: (libraryId: number) =>
		request<Resp<'/api/v0/libraries/{libraryId}', 'get'>>(
			`/api/v0/libraries/${libraryId}`
		),
	scanLibrary: (libraryId: number) =>
		request<Resp<'/api/v0/libraries/{libraryId}/scan', 'post'>>(
			`/api/v0/libraries/${libraryId}/scan`,
			{ method: 'POST' }
		),
	getScanJob: (jobId: number) =>
		request<Resp<'/api/v0/scan-jobs/{jobId}', 'get'>>(`/api/v0/scan-jobs/${jobId}`),
	listItems: (libraryId: number) =>
		request<Resp<'/api/v0/libraries/{libraryId}/items', 'get'>>(
			`/api/v0/libraries/${libraryId}/items`
		),
	listUnits: (libraryId: number) =>
		request<Resp<'/api/v0/libraries/{libraryId}/units', 'get'>>(
			`/api/v0/libraries/${libraryId}/units`
		),
	getSeries: (seriesKey: string) => {
		const params = new URLSearchParams({ seriesKey });
		return request<Resp<'/api/v0/series', 'get'>>(`/api/v0/series?${params}`);
	},
	getScanProgress: (libraryId: number) =>
		request<Resp<'/api/v0/libraries/{libraryId}/scan-progress', 'get'>>(
			`/api/v0/libraries/${libraryId}/scan-progress`
		),
	getItem: (itemId: number) =>
		request<Resp<'/api/v0/items/{itemId}', 'get'>>(`/api/v0/items/${itemId}`),
	getPlaybackInfo: (itemId: number) =>
		request<Resp<'/api/v0/items/{itemId}/playback-info', 'get'>>(
			`/api/v0/items/${itemId}/playback-info`
		),
	startTranscodeSession: (
		itemId: number,
		startMs = 0,
		audioTrackId?: string,
		subtitleTrackId?: string
	) => {
		const params = new URLSearchParams();
		if (startMs > 0) params.set('startMs', String(startMs));
		if (audioTrackId) params.set('audioTrackId', audioTrackId);
		if (subtitleTrackId) params.set('subtitleTrackId', subtitleTrackId);
		const q = params.toString();
		return request<Resp<'/api/v0/items/{itemId}/sessions', 'post'>>(
			`/api/v0/items/${itemId}/sessions${q ? `?${q}` : ''}`,
			{ method: 'POST' }
		);
	},
	seekTranscodeSession: (sessionId: string, startMs: number) => {
		const params = new URLSearchParams();
		params.set('startMs', String(Math.max(0, Math.floor(startMs))));
		return request<Resp<'/api/v0/sessions/{sessionId}/seek', 'post'>>(
			`/api/v0/sessions/${sessionId}/seek?${params}`,
			{ method: 'POST' }
		);
	},
	getTranscodeSession: (sessionId: string) =>
		request<Resp<'/api/v0/sessions/{sessionId}', 'get'>>(
			`/api/v0/sessions/${sessionId}`
		),
	deleteTranscodeSession: async (sessionId: string) => {
		// keepalive: survives pagehide/unload so the DELETE reaches the
		// server instead of being killed mid-flight (Safari reports that as
		// a misleading CORS failure). Bodyless DELETE is well under the
		// 64 KB keepalive limit. The idle reaper remains the backstop when
		// keepalive is unsupported or still fails — do not remove it.
		await request<undefined>(`/api/v0/sessions/${sessionId}`, {
			method: 'DELETE',
			keepalive: true
		});
	},
	getTranscodeCapabilities: () =>
		request<Resp<'/api/v0/system/transcode', 'get'>>('/api/v0/system/transcode'),

	// Auth. Four of these five are the demo client's whole login; the fifth
	// narrows to a profile, without which the byte routes refuse to play.
	getSetupState: () =>
		request<Resp<'/api/v0/system/setup', 'get'>>('/api/v0/system/setup'),
	bootstrap: (body: { username: string; password: string; clientLabel: string }) =>
		request<Resp<'/api/v0/auth/bootstrap', 'post'>>('/api/v0/auth/bootstrap', {
			method: 'POST',
			body: JSON.stringify(body)
		}),
	login: (body: { username: string; password: string; clientLabel: string }) =>
		request<Resp<'/api/v0/auth/login', 'post'>>('/api/v0/auth/login', {
			method: 'POST',
			body: JSON.stringify(body)
		}),
	getSession: () =>
		request<Resp<'/api/v0/auth/session', 'get'>>('/api/v0/auth/session'),
	listProfiles: () => request<Resp<'/api/v0/profiles', 'get'>>('/api/v0/profiles'),
	selectProfile: (profileRef: string) =>
		request<undefined>('/api/v0/auth/session', {
			method: 'POST',
			body: JSON.stringify({ profileRef })
		}),
	/**
	 * Narrow to a profile if the session is not already there.
	 *
	 * **ADR-0034 item 3**: the two byte routes need to know who is watching, so
	 * they refuse account scope. Nothing else in the API does.
	 *
	 * **This ran in the root layout on every page load until 2026-08-31**, which
	 * made account scope unreachable and every account-powers route answer
	 * `insufficient_role` — including `POST /libraries`, so a fresh install
	 * could not be given a library through its own interface. The narrowing was
	 * right and its placement was not.
	 *
	 * One profile, chosen without asking. Choosing between profiles is product
	 * UI and belongs to Block 3.
	 */
	ensureProfileScope: async (): Promise<void> => {
		const session = await api.getSession();
		if (session.scope === 'profile') return;
		const { profiles } = await api.listProfiles();
		if (profiles.length === 0) throw new Error('account has no profile');
		await api.selectProfile(profiles[0].profileRef);
	},
	logout: () => request<undefined>('/api/v0/auth/logout', { method: 'POST' })
};
