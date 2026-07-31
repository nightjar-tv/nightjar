<script lang="ts">
	import { onMount, untrack } from 'svelte';
	import { page } from '$app/state';
	import { api } from '$lib/api/client';
	import { copy, formatClock } from '$lib/copy';
	import {
		attachHls,
		watchProgressiveSubtitles,
		type HlsHandle
	} from '$lib/hlsPlayer';
	import { scrubRangeMs } from '$lib/hlsTimeline';
	import {
		attachModeFromSearch,
		LatencyProbe,
		probeEnabled,
		type AttachMode
	} from '$lib/latencyProbe';
	import type { components } from '$lib/api/schema';

	type MediaItem = components['schemas']['MediaItem'];
	type PlaybackInfo = components['schemas']['PlaybackInfo'];
	type TranscodeSession = components['schemas']['TranscodeSession'];
	type AudioTrack = components['schemas']['AudioTrack'];
	type SubtitleTrack = components['schemas']['SubtitleTrack'];
	// Not in lib.dom: only Safari exposes the media element track list today.
	type BrowserAudioTracks = { length: number; [index: number]: { enabled: boolean } };

	let item = $state<MediaItem | null>(null);
	let playback = $state<PlaybackInfo | null>(null);
	let error = $state<string | null>(null);
	let playlistUrl = $state<string | null>(null);
	let sessionEncoder = $state<Pick<
		TranscodeSession,
		'videoEncoder' | 'encoderKind' | 'usableExtentMs' | 'landedMs'
	> | null>(null);
	let preparingSession = $state(false);
	let switchingAudio = $state(false);
	let audioNote = $state<string | null>(null);
	let selectedAudioTrackId = $state<string | null>(null);
	/** HLS captions index into session MEDIA tracks; -1 = off. */
	let selectedSubtitleIndex = $state(0);
	let videoEl = $state<HTMLVideoElement | null>(null);
	/** Title-time scrub value (seconds); driven by the player handle. */
	let scrubSec = $state(0);
	let scrubDragging = $state(false);
	/** True while a far seek is in flight — hold the range on the commit target. */
	let scrubSeeking = $state(false);
	let playerShellEl = $state<HTMLElement | null>(null);
	let playerFullscreen = $state(false);
	// Mutable holder so onMount cleanup / pagehide always DELETE the live
	// session even if the $state read in a stale closure is still null.
	const sessionRef: { id: string | null } = { id: null };
	// Non-reactive so changing it does not re-run the attach effect on its own.
	const resumeRef = { seconds: 0 };
	// Current attach handle; its positionSeconds() is title-absolute (live
	// landedMs + media currentTime — ADR-0020).
	const playerRef: { handle: HlsHandle | null } = { handle: null };
	// Read by every await loop so an unmount mid-flight stops the loop and
	// reaps whatever it already started.
	const liveRef = { alive: true };

	const itemId = $derived(Number(page.params.id));
	const audioTracks = $derived(playback?.audioTracks ?? []);
	/** Tracks the HLS master can advertise (ready store VTT only). */
	const hlsSubtitleTracks = $derived(
		(playback?.subtitleTracks ?? []).filter((t) => t.readiness === 'complete' && t.url)
	);
	// Investigation: ?njAttach=land|first|two and ?njProbe=1 (see latencyProbe.ts).
	const attachMode = $derived(attachModeFromSearch(page.url.search));
	const probeOn = $derived(probeEnabled(page.url.search));
	/** Optional land for dogfood / deep link (`?startMs=`). Default 0. */
	const startMsFromSearch = $derived.by(() => {
		const raw = Number(page.url.searchParams.get('startMs') ?? 0);
		return Number.isFinite(raw) && raw > 0 ? Math.floor(raw) : 0;
	});

	function releaseSession() {
		const id = sessionRef.id;
		sessionRef.id = null;
		sessionEncoder = null;
		if (id) void api.deleteTranscodeSession(id);
	}

	function audioTrackLabel(track: AudioTrack): string {
		const name = track.label ?? track.language ?? track.trackId;
		return track.channelLayout ? `${name} · ${track.channelLayout}` : name;
	}

	function subtitleTrackLabel(track: SubtitleTrack): string {
		return track.label ?? track.language ?? track.trackId;
	}

	function selectSubtitle(index: number) {
		selectedSubtitleIndex = index;
		playerRef.handle?.setSubtitleTrack(index);
	}

	/** Poll until FFmpeg has written a servable response (playlist or segment). */
	async function waitForReady(url: string): Promise<boolean> {
		// Tag .m4s side-channel polls so dogfood logs can tell them from
		// Safari's own native segment GETs (same URL, no query).
		const fetchUrl = /\.m4s(?:\?|$)/i.test(url)
			? `${url}${url.includes('?') ? '&' : '?'}njFetcher=attach-wait`
			: url;
		for (let i = 0; liveRef.alive && i < 100; i++) {
			const res = await fetch(fetchUrl);
			if (res.ok) return true;
			// Gone for good (deleted / never created). 503 means still cooking.
			if (res.status === 404) return false;
			await new Promise((r) => setTimeout(r, 200));
		}
		return false;
	}

	function selectAudio(trackId: string) {
		if (trackId === selectedAudioTrackId) return;
		selectedAudioTrackId = trackId;
		audioNote = null;
		const track = audioTracks.find((t) => t.trackId === trackId);
		// Direct play is free only while the selected track fits the client
		// ceiling. An over-ceiling secondary (e.g. 5.1 commentary on a stereo
		// default) needs a hybrid session so the pan downmix still runs
		// (ADR-0012).
		if (
			playback?.playbackMethod === 'directPlay' &&
			track != null &&
			track.channels <= 2
		) {
			switchDirectPlayAudio(trackId);
		} else {
			void switchSessionAudio(trackId);
		}
	}

	/** Direct play: the container already holds every track, so the switch
	 *  is client-side and free where the browser exposes the list. */
	function switchDirectPlayAudio(trackId: string) {
		const list = (videoEl as (HTMLVideoElement & { audioTracks?: BrowserAudioTracks }) | null)
			?.audioTracks;
		const index = audioTracks.findIndex((t) => t.trackId === trackId);
		if (!list || index < 0) {
			audioNote = copy.audioSwitchUnsupported;
			return;
		}
		for (let i = 0; i < list.length; i++) {
			list[i].enabled = i === index;
		}
	}

	/** Sessions: a fresh session at the current position, then drop the old
	 *  one. Init and prior segments carry the old audio config, so this is
	 *  never a window move inside the seek path (ADR-0012). Cook the new
	 *  land while the old session keeps playing, then cut over — park-then-
	 *  wait made every switch feel like a reload even when server land was
	 *  fast. */
	async function switchSessionAudio(trackId: string) {
		const seconds = playerRef.handle?.positionSeconds() ?? videoEl?.currentTime ?? 0;
		const startMs = Math.max(0, Math.floor(seconds * 1000));
		const previous = sessionRef.id;
		const probe = new LatencyProbe(attachMode, probeOn);
		const unspy = probe.installFetchSpy();
		probe.mark('switch_requested', `startMs=${startMs} mode=${attachMode}`);
		switchingAudio = true;
		try {
			const started = await api.startTranscodeSession(itemId, startMs, trackId);
			probe.mark('session_post_ok', started.sessionId);
			probe.mark('wait_begin', attachMode);
			const landIdx = Math.floor(startMs / 2000);
			// Land wait only needs the play-land segment; encode lead-in (2)
			// cooks behind it on the server without changing this gate.
			const windowIdx = landIdx;
			const ready = await waitForAttachReady(
				started.playlistUrl,
				windowIdx,
				landIdx,
				attachMode,
				probe
			);
			// Never adopt a session the page no longer owns; leaving it for
			// the idle reaper burns a cap slot for a minute.
			if (!ready || !liveRef.alive) {
				void api.deleteTranscodeSession(started.sessionId);
				if (liveRef.alive) error = copy.sessionFailed;
				return;
			}
			resumeRef.seconds = startMs / 1000;
			sessionRef.id = started.sessionId;
			sessionEncoder = started;
			probe.mark('attach', started.playlistUrl);
			if (videoEl) probe.wireVideo(videoEl);
			// Swap playlist: $effect destroys the old handle and attaches.
			// Old session stays alive until after cutover so playback does
			// not go black while the new land cooks.
			playlistUrl = started.playlistUrl;
			if (previous && previous !== started.sessionId) {
				void api.deleteTranscodeSession(previous);
				probe.mark('old_session_deleted', previous);
			}
			if (probeOn) {
				// Defer summary until after first frame likely lands.
				setTimeout(() => {
					console.info('[nj-probe-summary]', JSON.stringify(probe.summary()));
				}, 8000);
			}
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
		} finally {
			unspy();
			switchingAudio = false;
		}
	}

	/** Attach gate: wait until the run master is ready (map has segments). */
	async function waitForAttachReady(
		playlist: string,
		_windowIdx: number,
		_landIdx: number,
		_mode: AttachMode,
		probe: LatencyProbe
	): Promise<boolean> {
		const masterOk = await waitForReady(playlist);
		if (masterOk) probe.mark('master_ready');
		return masterOk;
	}

	const playable = $derived(
		playback != null &&
			(playback.playbackMethod === 'directPlay' ||
				playlistUrl != null ||
				switchingAudio)
	);

	// Listed but not served (ASS/SSA): readiness absent. Preparing tracks have
	// readiness without url — do not call those "not rendered".
	const unrenderedSubtitles = $derived(
		(playback?.subtitleTracks ?? [])
			.filter((t) => !t.url && t.readiness == null)
			.map((t) => `${t.language ?? t.trackId} (${t.codec})`)
			.join(', ')
	);

	const subtitlesPreparing = $derived(
		(playback?.subtitleTracks ?? []).some((t) => t.readiness === 'preparing') ||
			(playback?.subtitleStatus === 'pending' &&
				!(playback?.subtitleTracks ?? []).some((t) => t.url))
	);

	// Stable while readiness changes so the watcher is not torn down on every poll.
	const progressiveKey = $derived.by(() => {
		if (!videoEl) return null;
		if (playback?.playbackMethod !== 'directPlay' || !playback.streamUrl) return null;
		return `${itemId}:${playback.streamUrl}`;
	});

	/** Title / usable duration for the scrub bar (not EVENT Σ EXTINF). */
	const scrubMaxSec = $derived(
		scrubRangeMs(item?.durationMs, sessionEncoder?.usableExtentMs) / 1000
	);

	onMount(() => {
		liveRef.alive = true;

		const onPageHide = () => releaseSession();
		// pagehide DELETE (keepalive): refresh and close must stop the
		// session. keepalive reduces orphans; the idle reaper is still the
		// Gate 2 backstop when the request never leaves the browser.
		window.addEventListener('pagehide', onPageHide);

		(async () => {
			item = await api.getItem(itemId);
			playback = await api.getPlaybackInfo(itemId);
			selectedAudioTrackId =
				playback.audioTracks?.find((t) => t.default)?.trackId ?? null;

			// Remux and transcode both play through a session (ADR-0011).
			if (playback.playbackMethod !== 'directPlay') {
				preparingSession = true;
				let started: TranscodeSession | null = null;
				for (let attempt = 0; liveRef.alive && attempt < 5; attempt++) {
					try {
						const landMs = startMsFromSearch;
						started = await api.startTranscodeSession(itemId, landMs);
						sessionRef.id = started.sessionId;
						sessionEncoder = started;
						// Session land is title-absolute; media element starts at 0.
						resumeRef.seconds = (started.landedMs ?? landMs) / 1000;
						break;
					} catch (e) {
						const msg = e instanceof Error ? e.message : String(e);
						if (msg.includes('retry shortly') || msg.includes('in use')) {
							await new Promise((r) => setTimeout(r, 1000));
							continue;
						}
						throw e;
					}
				}
				if (!started) {
					preparingSession = false;
					error = copy.sessionsBusy;
					return;
				}
				// Wait until init is ready so the VOD playlist is servable.
				if (await waitForReady(started.playlistUrl)) {
					playlistUrl = started.playlistUrl;
					preparingSession = false;
					// Producer EOF may set usableExtentMs after the POST returns
					// (damaged mid-title). Poll until known or the page dies.
					void (async () => {
						const sid = started.sessionId;
						for (let i = 0; liveRef.alive && i < 120; i++) {
							if (sessionRef.id !== sid) return;
							try {
								const view = await api.getTranscodeSession(sid);
								if (view.usableExtentMs != null) {
									sessionEncoder = {
										...(sessionEncoder ?? view),
										usableExtentMs: view.usableExtentMs,
										landedMs: view.landedMs,
										videoEncoder: view.videoEncoder,
										encoderKind: view.encoderKind
									};
									return;
								}
							} catch {
								return;
							}
							await new Promise((r) => setTimeout(r, 500));
						}
					})();
					return;
				}
				preparingSession = false;
				error = copy.sessionFailed;
			}
		})().catch((e: Error) => {
			preparingSession = false;
			error = e.message;
		});

		return () => {
			liveRef.alive = false;
			window.removeEventListener('pagehide', onPageHide);
			releaseSession();
		};
	});

	$effect(() => {
		const video = videoEl;
		const url = playlistUrl;
		if (!video || !url) {
			return;
		}
		if (probeOn) console.warn('[nj-subs] item page attaching HLS', url);
		const handle = attachHls(video, url, resumeRef.seconds);
		playerRef.handle = handle;
		const onTime = () => {
			if (!scrubDragging && !scrubSeeking) {
				scrubSec = handle.positionSeconds();
			}
		};
		video.addEventListener('timeupdate', onTime);
		return () => {
			video.removeEventListener('timeupdate', onTime);
			playerRef.handle = null;
			handle.destroy();
		};
	});

	function onScrubInput(ev: Event) {
		const v = Number((ev.currentTarget as HTMLInputElement).value);
		if (!Number.isFinite(v)) return;
		scrubDragging = true;
		scrubSec = Math.max(0, Math.min(scrubMaxSec || v, v));
	}

	async function onScrubCommit() {
		scrubDragging = false;
		const handle = playerRef.handle;
		if (!handle) return;
		scrubSeeking = true;
		const target = scrubSec;
		try {
			await handle.seekToTitleSeconds(target);
			scrubSec = handle.positionSeconds();
		} finally {
			scrubSeeking = false;
		}
	}

	function onFullscreenChange() {
		const shell = playerShellEl;
		playerFullscreen = !!(
			shell &&
			(document.fullscreenElement === shell ||
				// @ts-expect-error webkit prefix
				document.webkitFullscreenElement === shell)
		);
	}

	async function togglePlayerFullscreen() {
		const shell = playerShellEl;
		if (!shell) return;
		try {
			if (playerFullscreen) {
				if (document.exitFullscreen) await document.exitFullscreen();
				// @ts-expect-error webkit prefix
				else if (document.webkitExitFullscreen) document.webkitExitFullscreen();
			} else if (shell.requestFullscreen) {
				await shell.requestFullscreen();
			} else {
				// @ts-expect-error webkit prefix (iPad Safari)
				shell.webkitRequestFullscreen?.();
			}
		} catch {
			// Gesture / policy denied — leave chrome as-is.
		}
	}

	$effect(() => {
		document.addEventListener('fullscreenchange', onFullscreenChange);
		document.addEventListener('webkitfullscreenchange', onFullscreenChange);
		return () => {
			document.removeEventListener('fullscreenchange', onFullscreenChange);
			document.removeEventListener('webkitfullscreenchange', onFullscreenChange);
		};
	});

	// Direct-play progressive `<track>` reload (ADR-0013 §11). HLS captions
	// use segmented EXT-X-MEDIA (ready = store slice; cold = session demux).
	$effect(() => {
		const key = progressiveKey;
		const video = videoEl;
		if (!key || !video) {
			return;
		}
		const initial = untrack(() => playback);
		if (!initial) {
			return;
		}
		const handle = watchProgressiveSubtitles({
			video,
			initial,
			fetchPlaybackInfo: () => api.getPlaybackInfo(itemId),
			isAlive: () => liveRef.alive,
			onPlaybackInfo: (next) => {
				playback = next;
			}
		});
		return () => handle.destroy();
	});
</script>

<svelte:head>
	<title>{item?.title ?? 'item'} · nightjar</title>
</svelte:head>

<main>
	<p class="crumb">
		<a href="/">nightjar</a>
		{#if item}
			/ <a href="/libraries/{item.libraryId}">library</a> /
		{/if}
		item
	</p>

	{#if error}
		<p class="error" role="alert">{error}</p>
	{/if}

	{#if item && playback}
		<header>
			<h1>{item.title}</h1>
			<p class="meta">
				{item.kind}
				{#if item.year}· {item.year}{/if}
				{#if playback.videoCodec}· {playback.videoCodec}{/if}
				{#if playback.audioCodec}· {playback.audioCodec}{/if}
				{#if sessionEncoder?.encoderKind === 'copy'}
					· stream copy
				{:else if sessionEncoder}
					· transcoding · {sessionEncoder.videoEncoder} ({sessionEncoder.encoderKind})
				{/if}
			</p>
			<p class="reason">{playback.reason}</p>
			{#if sessionEncoder?.usableExtentMs != null && item.durationMs}
				<p class="preparing" role="status">
					{copy.titleDamagedUsable(
						sessionEncoder.usableExtentMs / 1000,
						item.durationMs / 1000
					)}
				</p>
			{/if}
		</header>

		{#if playable && playlistUrl}
			<div class="player" bind:this={playerShellEl}>
				<!-- svelte-ignore a11y_media_has_caption -->
				<!-- controlsList nofullscreen: use container fullscreen so our scrub stays -->
				<video bind:this={videoEl} controls controlsList="nofullscreen" playsinline></video>
				{#if scrubMaxSec > 0}
					<label class="scrub">
						<span class="scrub-label">{copy.scrubPosition}</span>
						<input
							type="range"
							min="0"
							max={scrubMaxSec}
							step="0.1"
							value={Math.min(scrubSec, scrubMaxSec)}
							aria-valuetext="{formatClock(scrubSec)} / {formatClock(scrubMaxSec)}"
							oninput={onScrubInput}
							onchange={onScrubCommit}
							onkeyup={(e) => {
								if (e.key === 'Enter' || e.key === ' ') onScrubCommit();
							}}
						/>
						<span class="scrub-clock" aria-hidden="true"
							>{formatClock(scrubSec)} / {formatClock(scrubMaxSec)}</span
						>
					</label>
				{/if}
				<button
					type="button"
					class="player-fs"
					onclick={togglePlayerFullscreen}
					aria-pressed={playerFullscreen}
				>
					{playerFullscreen ? copy.playerExitFullscreen : copy.playerFullscreen}
				</button>
			</div>
			{#if subtitlesPreparing}
				<p class="preparing">{copy.subtitlesPreparing}</p>
			{/if}
			{#if unrenderedSubtitles}
				<p class="preparing">{copy.subtitlesFoundNotRendered} {unrenderedSubtitles}</p>
			{/if}
		{:else if playable && playback.streamUrl}
			{#if subtitlesPreparing}
				<p class="preparing">{copy.subtitlesPreparing}</p>
			{/if}
			<!-- svelte-ignore a11y_media_has_caption (language subtitles are not captions; tracks attached by watchProgressiveSubtitles) -->
			<video
				bind:this={videoEl}
				controls
				playsinline
				src={playback.streamUrl}
				crossorigin="anonymous"
			>
				Your browser cannot play this file directly.
			</video>
			{#if unrenderedSubtitles}
				<p class="preparing">{copy.subtitlesFoundNotRendered} {unrenderedSubtitles}</p>
			{/if}
		{:else if preparingSession}
			<p class="preparing" role="status">{copy.preparingSession}</p>
		{:else if playback.playbackMethod !== 'directPlay'}
			<p class="error">{copy.sessionFailed}</p>
		{/if}

		{#if switchingAudio}
			<p class="preparing" role="status">{copy.switchingAudio}</p>
		{/if}

		{#if playable && playlistUrl && hlsSubtitleTracks.length > 0}
			<fieldset class="tracks">
				<legend>{copy.subtitleTrack}</legend>
				<label>
					<input
						type="radio"
						name="subtitle-track"
						value="-1"
						checked={selectedSubtitleIndex < 0}
						onchange={() => selectSubtitle(-1)}
					/>
					{copy.subtitleOff}
				</label>
				{#each hlsSubtitleTracks as track, i (track.trackId)}
					<label>
						<input
							type="radio"
							name="subtitle-track"
							value={i}
							checked={selectedSubtitleIndex === i}
							onchange={() => selectSubtitle(i)}
						/>
						{subtitleTrackLabel(track)}
					</label>
				{/each}
			</fieldset>
		{/if}

		{#if playable && audioTracks.length > 1}
			<fieldset class="tracks">
				<legend>{copy.audioTrack}</legend>
				{#each audioTracks as track (track.trackId)}
					<label>
						<input
							type="radio"
							name="audio-track"
							value={track.trackId}
							checked={track.trackId === selectedAudioTrackId}
							disabled={switchingAudio}
							onchange={() => selectAudio(track.trackId)}
						/>
						{audioTrackLabel(track)}
					</label>
				{/each}
			</fieldset>
			{#if audioNote}
				<p class="preparing" role="status">{audioNote}</p>
			{/if}
		{/if}
	{/if}
</main>

<style>
	main {
		max-width: 56rem;
		margin: 0 auto;
		padding: 2rem 1.25rem 4rem;
	}
	.crumb {
		color: var(--moth-dim);
		margin: 0 0 1.5rem;
	}
	.crumb a {
		color: var(--moth-dim);
	}
	h1 {
		font-family: 'Bricolage Grotesque', system-ui, sans-serif;
		font-size: 2rem;
		font-weight: 700;
		margin: 0;
	}
	.meta,
	.reason {
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
		color: var(--moth-dim);
	}
	video {
		display: block;
		width: 100%;
		margin-top: 1.5rem;
		background: #000;
		border-radius: 8px;
	}
	.player {
		margin-top: 1.5rem;
	}
	.player video {
		margin-top: 0;
	}
	.player:fullscreen,
	.player:-webkit-full-screen {
		display: flex;
		flex-direction: column;
		justify-content: center;
		box-sizing: border-box;
		width: 100%;
		height: 100%;
		padding: 1rem;
		background: #000;
	}
	.player:fullscreen video,
	.player:-webkit-full-screen video {
		flex: 1 1 auto;
		width: 100%;
		height: auto;
		max-height: calc(100% - 4rem);
		border-radius: 0;
		background: #000;
	}
	.player:fullscreen .scrub,
	.player:-webkit-full-screen .scrub {
		color: var(--moth);
	}
	.player-fs {
		margin-top: 0.5rem;
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
		color: var(--moth-dim);
		background: transparent;
		border: 1px solid var(--moth-dim);
		border-radius: 4px;
		padding: 0.35rem 0.75rem;
		cursor: pointer;
	}
	.player-fs:focus-visible {
		outline: 2px solid var(--dusk);
		outline-offset: 2px;
	}
	.scrub {
		display: grid;
		grid-template-columns: auto 1fr auto;
		gap: 0.75rem;
		align-items: center;
		margin-top: 0.75rem;
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
		color: var(--moth-dim);
	}
	.scrub input[type='range'] {
		width: 100%;
		accent-color: var(--dusk);
	}
	.scrub-clock {
		min-width: 7rem;
		text-align: right;
		font-variant-numeric: tabular-nums;
	}
	.error {
		color: var(--dusk);
	}
	.preparing {
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
		color: var(--moth-dim);
	}
	.tracks {
		display: flex;
		flex-wrap: wrap;
		gap: 0.25rem 1rem;
		align-items: center;
		margin-top: 1rem;
		padding: 0.75rem 1rem;
		border: 1px solid var(--moth-dim);
		border-radius: 8px;
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
	}
	.tracks legend {
		color: var(--moth-dim);
		padding: 0 0.35rem;
	}
	.tracks label {
		display: flex;
		align-items: center;
		gap: 0.4rem;
	}
	.tracks input:focus-visible {
		outline: 2px solid currentColor;
		outline-offset: 2px;
	}
</style>
