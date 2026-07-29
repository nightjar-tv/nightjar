/**
 * Investigation-only latency probe for Gate R attach experiments.
 * Enabled with ?njProbe=1. Attach wait policy with ?njAttach=land|first|two
 * (default land = current shipped behaviour).
 */

export type AttachMode = 'land' | 'first' | 'two';

export function attachModeFromSearch(search: string): AttachMode {
	const v = new URLSearchParams(search).get('njAttach');
	if (v === 'first' || v === 'two' || v === 'land') return v;
	return 'land';
}

export function probeEnabled(search: string): boolean {
	return new URLSearchParams(search).get('njProbe') === '1';
}

type Phase =
	| 'switch_requested'
	| 'session_post_ok'
	| 'old_session_deleted'
	| 'wait_begin'
	| 'master_ready'
	| 'first_seg_ready'
	| 'second_seg_ready'
	| 'land_seg_ready'
	| 'attach'
	| 'first_media_request'
	| 'loadedmetadata'
	| 'canplay'
	| 'playing'
	| 'first_decoded_frame'
	| 'playback_resumed';

export class LatencyProbe {
	readonly t0 = performance.now();
	readonly marks: { phase: Phase; ms: number; detail?: string }[] = [];
	readonly requests: {
		ms: number;
		url: string;
		status: number;
		resource: string;
	}[] = [];
	private origFetch = globalThis.fetch.bind(globalThis);

	constructor(
		readonly mode: AttachMode,
		readonly enabled: boolean
	) {}

	mark(phase: Phase, detail?: string) {
		const ms = Math.round(performance.now() - this.t0);
		this.marks.push({ phase, ms, detail });
		if (this.enabled) {
			console.info(`[nj-probe] ${ms}ms ${phase}${detail ? ` ${detail}` : ''}`);
		}
	}

	/** Wrap fetch to record HLS request order/status during a switch. */
	installFetchSpy() {
		if (!this.enabled) return () => {};
		const self = this;
		globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
			const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
			const t = Math.round(performance.now() - self.t0);
			const res = await self.origFetch(input, init);
			if (url.includes('/sessions/')) {
				const resource = url.split('/').pop()?.split('?')[0] ?? url;
				self.requests.push({ ms: t, url, status: res.status, resource });
				if (
					resource.endsWith('.m4s') ||
					resource === 'init.mp4' ||
					resource.endsWith('.m3u8')
				) {
					if (!self.marks.some((m) => m.phase === 'first_media_request')) {
						self.mark('first_media_request', `${resource} ${res.status}`);
					}
				}
				console.info(`[nj-probe] ${t}ms req ${res.status} ${resource}`);
			}
			return res;
		};
		return () => {
			globalThis.fetch = self.origFetch;
		};
	}

	wireVideo(video: HTMLVideoElement) {
		if (!this.enabled) return;
		const once = (ev: string, phase: Phase) => {
			video.addEventListener(
				ev,
				() => this.mark(phase, `currentTime=${video.currentTime.toFixed(3)}`),
				{ once: true }
			);
		};
		once('loadedmetadata', 'loadedmetadata');
		once('canplay', 'canplay');
		once('playing', 'playing');
		// rVFC ≈ first decoded frame when supported.
		const anyVideo = video as HTMLVideoElement & {
			requestVideoFrameCallback?: (cb: () => void) => number;
		};
		if (typeof anyVideo.requestVideoFrameCallback === 'function') {
			anyVideo.requestVideoFrameCallback(() => {
				this.mark(
					'first_decoded_frame',
					`currentTime=${video.currentTime.toFixed(3)}`
				);
				this.mark('playback_resumed', `currentTime=${video.currentTime.toFixed(3)}`);
			});
		} else {
			video.addEventListener(
				'playing',
				() => {
					this.mark(
						'playback_resumed',
						`currentTime=${video.currentTime.toFixed(3)}`
					);
				},
				{ once: true }
			);
		}
	}

	summary() {
		return {
			mode: this.mode,
			marks: this.marks,
			requests: this.requests
		};
	}
}

/** How many ready window segments to wait for before attach. */
export function segmentsToWait(mode: AttachMode): number {
	switch (mode) {
		case 'first':
			return 1;
		case 'two':
			return 2;
		case 'land':
			return -1; // sentinel: wait for land index
	}
}
