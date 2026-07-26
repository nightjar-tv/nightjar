import type { paths } from './schema';

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
			...init?.headers
		}
	});
	if (!res.ok) {
		let message = res.statusText;
		try {
			const body = (await res.json()) as { error?: string };
			if (body.error) message = body.error;
		} catch {
			/* ignore */
		}
		throw new Error(message);
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
	getItem: (itemId: number) =>
		request<Resp<'/api/v0/items/{itemId}', 'get'>>(`/api/v0/items/${itemId}`),
	getPlaybackInfo: (itemId: number) =>
		request<Resp<'/api/v0/items/{itemId}/playback-info', 'get'>>(
			`/api/v0/items/${itemId}/playback-info`
		),
	startTranscodeSession: (itemId: number, startMs = 0) => {
		const q = startMs > 0 ? `?startMs=${startMs}` : '';
		return request<Resp<'/api/v0/items/{itemId}/sessions', 'post'>>(
			`/api/v0/items/${itemId}/sessions${q}`,
			{ method: 'POST' }
		);
	},
	deleteTranscodeSession: async (sessionId: string) => {
		await request<undefined>(`/api/v0/sessions/${sessionId}`, { method: 'DELETE' });
	},
	getTranscodeCapabilities: () =>
		request<Resp<'/api/v0/system/transcode', 'get'>>('/api/v0/system/transcode')
};
