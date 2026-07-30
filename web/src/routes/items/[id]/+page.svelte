<script lang="ts">
	import { onMount, untrack } from 'svelte';
	import { page } from '$app/state';
	import { api } from '$lib/api/client';
	import { copy } from '$lib/copy';
	import SubtitleSwitcher from '$lib/components/SubtitleSwitcher.svelte';
	import {
		attachHls,
		watchProgressiveSubtitles,
		type HlsHandle
	} from '$lib/hlsPlayer';
	import {
		attachModeFromSearch,
		LatencyProbe,
		probeEnabled,
		type AttachMode
	} from '$lib/latencyProbe';
	import {
		isHlsSelectable,
		isSoftReady,
		selectionNeedsBurnIn
	} from '$lib/subtitleSwitcher';
	import type { components } from '$lib/api/schema';

	type MediaItem = components['schemas']['MediaItem'];
	type PlaybackInfo = components['schemas']['PlaybackInfo'];
	type TranscodeSession = components['schemas']['TranscodeSession'];
	type AudioTrack = components['schemas']['AudioTrack'];
	// Not in lib.dom: only Safari exposes the media element track list today.
	type BrowserAudioTracks = { length: number; [index: number]: { enabled: boolean } };

	let item = $state<MediaItem | null>(null);
	let playback = $state<PlaybackInfo | null>(null);
	let error = $state<string | null>(null);
	let playlistUrl = $state<string | null>(null);
	let sessionEncoder = $state<Pick<TranscodeSession, 'videoEncoder' | 'encoderKind'> | null>(
		null
	);
	let preparingSession = $state(false);
	let switchingAudio = $state(false);
	let switchingSubtitles = $state(false);
	let audioNote = $state<string | null>(null);
	let selectedAudioTrackId = $state<string | null>(null);
	/** Selected inventory trackId; null = Off. */
	let selectedSubtitleTrackId = $state<string | null>(null);
	/** ASS/SSA only: on = burn-in, off = house-styled WebVTT. */
	let originalStyling = $state(false);
	/** Burn-in track active on the current session (ADR-0018). */
	let burningSubtitleTrackId = $state<string | null>(null);
	let videoEl = $state<HTMLVideoElement | null>(null);
	// Mutable holder so onMount cleanup / pagehide always DELETE the live
	// session even if the $state read in a stale closure is still null.
	const sessionRef: { id: string | null } = { id: null };
	// Non-reactive so changing it does not re-run the attach effect on its own.
	const resumeRef = { seconds: 0 };
	// Current attach handle; its positionSeconds() is title-absolute where
	// raw currentTime is not after a mid-title switch (see hlsPlayer).
	const playerRef: { handle: HlsHandle | null } = { handle: null };
	// Read by every await loop so an unmount mid-flight stops the loop and
	// reaps whatever it already started.
	const liveRef = { alive: true };
	/** True when playbackInfo opened as DirectPlay (burn-in may start a session). */
	const openedAsDirectPlay = { value: false };

	const itemId = $derived(Number(page.params.id));
	const audioTracks = $derived(playback?.audioTracks ?? []);
	/** Full inventory — one row per track (Rule 4.11), soft and burn-in together. */
	const subtitleTracks = $derived(playback?.subtitleTracks ?? []);
	/**
	 * HLS master MEDIA order (complete + url only). Must match
	 * sessions.rs snapshot_hls_tracks — not partial soft tracks.
	 */
	const hlsSubtitleTracks = $derived(subtitleTracks.filter((t) => isHlsSelectable(t)));
	// Investigation: ?njAttach=land|first|two and ?njProbe=1 (see latencyProbe.ts).
	const attachMode = $derived(attachModeFromSearch(page.url.search));
	const probeOn = $derived(probeEnabled(page.url.search));

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

	function resumePlayback(video: HTMLVideoElement) {
		void video.play().catch(() => {
			/* autoplay block — user can press play */
		});
	}

	function applySoftSubtitle(trackId: string | null) {
		const handle = playerRef.handle;
		if (playlistUrl && handle) {
			if (trackId == null) {
				handle.setSubtitleTrack(-1);
				return;
			}
			const idx = hlsSubtitleTracks.findIndex((t) => t.trackId === trackId);
			handle.setSubtitleTrack(idx >= 0 ? idx : -1);
			return;
		}
		const video = videoEl;
		if (!video) return;
		for (const node of video.querySelectorAll('track')) {
			const el = node as HTMLTrackElement;
			const id = el.getAttribute('data-track-id');
			const tt = el.track;
			if (!tt) continue;
			const on = trackId != null && id === trackId;
			if (on) {
				// Firefox often needs a mode toggle before cues paint.
				tt.mode = 'hidden';
				tt.mode = 'showing';
			} else {
				tt.mode = 'disabled';
			}
		}
	}

	function onSubtitleSelect(trackId: string | null) {
		selectedSubtitleTrackId = trackId;
		void applySubtitleSelection(trackId);
	}

	async function applySubtitleSelection(trackId: string | null) {
		if (trackId == null) {
			await endBurnInIfNeeded();
			applySoftSubtitle(null);
			return;
		}
		const track = subtitleTracks.find((t) => t.trackId === trackId);
		if (!track) return;
		if (selectionNeedsBurnIn(track, originalStyling)) {
			applySoftSubtitle(null);
			await startOrSwitchBurnIn(track.trackId);
			return;
		}
		// Soft on an HLS session: only complete tracks are in the master.
		if (playlistUrl && !isHlsSelectable(track) && track.render === 'soft') {
			applySoftSubtitle(null);
			return;
		}
		await endBurnInIfNeeded();
		applySoftSubtitle(track.trackId);
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

	function sessionAssetUrl(playlistUrl: string, name: string): string {
		return playlistUrl.replace(/\/master\.m3u8$/, `/${name}`);
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
	 *  one. Init and prior segments carry the old audio/burn config, so this is
	 *  never a window move inside the seek path (ADR-0012 / ADR-0018). Cook the
	 *  new land while the old session keeps playing, then cut over — park-then-
	 *  wait made every switch feel like a reload even when server land was
	 *  fast. */
	async function restartSession(opts: {
		audioTrackId?: string | null;
		subtitleTrackId?: string | null;
		busy: 'audio' | 'subtitles';
	}) {
		const seconds = playerRef.handle?.positionSeconds() ?? videoEl?.currentTime ?? 0;
		const startMs = Math.max(0, Math.floor(seconds * 1000));
		const previous = sessionRef.id;
		const probe = new LatencyProbe(attachMode, probeOn);
		const unspy = probe.installFetchSpy();
		probe.mark('switch_requested', `startMs=${startMs} mode=${attachMode}`);
		if (opts.busy === 'audio') switchingAudio = true;
		else switchingSubtitles = true;
		try {
			const started = await api.startTranscodeSession(
				itemId,
				startMs,
				opts.audioTrackId ?? selectedAudioTrackId ?? undefined,
				opts.subtitleTrackId ?? undefined
			);
			probe.mark('session_post_ok', started.sessionId);
			probe.mark('wait_begin', attachMode);
			const landIdx = Math.floor(startMs / 2000);
			const windowIdx = landIdx;
			const ready = await waitForAttachReady(
				started.playlistUrl,
				windowIdx,
				landIdx,
				attachMode,
				probe
			);
			if (!ready || !liveRef.alive) {
				void api.deleteTranscodeSession(started.sessionId);
				if (liveRef.alive) error = copy.sessionFailed;
				return false;
			}
			resumeRef.seconds = startMs / 1000;
			sessionRef.id = started.sessionId;
			sessionEncoder = started;
			burningSubtitleTrackId = opts.subtitleTrackId ?? null;
			probe.mark('attach', started.playlistUrl);
			if (videoEl) probe.wireVideo(videoEl);
			playlistUrl = started.playlistUrl;
			if (previous && previous !== started.sessionId) {
				void api.deleteTranscodeSession(previous);
				probe.mark('old_session_deleted', previous);
			}
			if (probeOn) {
				setTimeout(() => {
					console.info('[nj-probe-summary]', JSON.stringify(probe.summary()));
				}, 8000);
			}
			return true;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
			return false;
		} finally {
			unspy();
			switchingAudio = false;
			switchingSubtitles = false;
		}
	}

	async function startOrSwitchBurnIn(trackId: string) {
		if (burningSubtitleTrackId === trackId && playlistUrl) return;
		await restartSession({
			subtitleTrackId: trackId,
			busy: 'subtitles'
		});
	}

	/** Leave burn-in: restore DirectPlay when that was the open path, else
	 *  restart the session without subtitleTrackId. */
	async function endBurnInIfNeeded() {
		if (!burningSubtitleTrackId) return;
		if (openedAsDirectPlay.value && playback?.streamUrl) {
			const seconds = playerRef.handle?.positionSeconds() ?? videoEl?.currentTime ?? 0;
			resumeRef.seconds = seconds;
			releaseSession();
			playlistUrl = null;
			burningSubtitleTrackId = null;
			return;
		}
		if (!sessionRef.id) {
			burningSubtitleTrackId = null;
			return;
		}
		await restartSession({
			subtitleTrackId: null,
			busy: 'subtitles'
		});
	}

	async function switchSessionAudio(trackId: string) {
		await restartSession({
			audioTrackId: trackId,
			subtitleTrackId: burningSubtitleTrackId,
			busy: 'audio'
		});
	}

	/** Investigation attach gate: land (shipped), first window seg, or two segs. */
	async function waitForAttachReady(
		playlist: string,
		windowIdx: number,
		landIdx: number,
		mode: AttachMode,
		probe: LatencyProbe
	): Promise<boolean> {
		const masterOk = await waitForReady(playlist);
		if (masterOk) probe.mark('master_ready');
		if (!masterOk) return false;
		if (mode === 'land') {
			const landName = `seg${String(landIdx).padStart(3, '0')}.m4s`;
			const ok = await waitForReady(sessionAssetUrl(playlist, landName));
			if (ok) probe.mark('land_seg_ready', landName);
			return ok;
		}
		const firstName = `seg${String(windowIdx).padStart(3, '0')}.m4s`;
		const firstOk = await waitForReady(sessionAssetUrl(playlist, firstName));
		if (firstOk) probe.mark('first_seg_ready', firstName);
		if (!firstOk) return false;
		if (mode === 'first') return true;
		const secondName = `seg${String(windowIdx + 1).padStart(3, '0')}.m4s`;
		const secondOk = await waitForReady(sessionAssetUrl(playlist, secondName));
		if (secondOk) probe.mark('second_seg_ready', secondName);
		return secondOk;
	}

	const playable = $derived(
		playback != null &&
			(playback.playbackMethod === 'directPlay' ||
				playlistUrl != null ||
				switchingAudio ||
				switchingSubtitles)
	);

	const subtitlesPreparing = $derived(
		subtitleTracks.some((t) => t.readiness === 'preparing') ||
			(playback?.subtitleStatus === 'pending' &&
				!subtitleTracks.some((t) => t.url))
	);

	// Stable while readiness changes so the watcher is not torn down on every poll.
	const progressiveKey = $derived.by(() => {
		if (!videoEl) return null;
		if (playback?.playbackMethod !== 'directPlay' || !playback.streamUrl) return null;
		return `${itemId}:${playback.streamUrl}`;
	});

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
			openedAsDirectPlay.value = playback.playbackMethod === 'directPlay';
			selectedAudioTrackId =
				playback.audioTracks?.find((t) => t.default)?.trackId ?? null;
			const tracks = playback.subtitleTracks ?? [];
			const softFirst =
				playback.playbackMethod === 'directPlay'
					? tracks.find((t) => isSoftReady(t))
					: tracks.find((t) => isHlsSelectable(t));
			selectedSubtitleTrackId = softFirst?.trackId ?? null;

			// Remux and transcode both play through a session (ADR-0011).
			if (playback.playbackMethod !== 'directPlay') {
				preparingSession = true;
				let started: TranscodeSession | null = null;
				for (let attempt = 0; liveRef.alive && attempt < 5; attempt++) {
					try {
						started = await api.startTranscodeSession(itemId);
						sessionRef.id = started.sessionId;
						sessionEncoder = started;
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

	// Attach only when the playlist URL changes. Subtitle selection must not
	// be a dependency here — reading selectedSubtitleTrackId in this effect
	// destroyed hls.js on every soft switch and left playback stopped.
	$effect(() => {
		const video = videoEl;
		const url = playlistUrl;
		if (!video || !url) {
			return;
		}
		if (probeOn) console.warn('[nj-subs] item page attaching HLS', url);
		const handle = attachHls(video, url, resumeRef.seconds);
		playerRef.handle = handle;
		untrack(() => {
			const softId =
				burningSubtitleTrackId != null ? null : selectedSubtitleTrackId;
			if (softId && hlsSubtitleTracks.some((t) => t.trackId === softId)) {
				const idx = hlsSubtitleTracks.findIndex((t) => t.trackId === softId);
				handle.setSubtitleTrack(idx >= 0 ? idx : -1);
			} else {
				handle.setSubtitleTrack(-1);
			}
		});
		resumePlayback(video);
		return () => {
			playerRef.handle = null;
			handle.destroy();
		};
	});

	// Restore DirectPlay land after leaving a burn-in session.
	$effect(() => {
		const video = videoEl;
		if (!video || playlistUrl || !playback?.streamUrl) return;
		const land = resumeRef.seconds;
		if (land <= 0) return;
		const onMeta = () => {
			if (Math.abs(video.currentTime - land) > 0.5) {
				video.currentTime = land;
			}
			resumePlayback(video);
		};
		if (video.readyState >= 1) onMeta();
		else video.addEventListener('loadedmetadata', onMeta, { once: true });
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
				untrack(() => {
					if (selectedSubtitleTrackId && !burningSubtitleTrackId) {
						applySoftSubtitle(selectedSubtitleTrackId);
					}
				});
			}
		});
		untrack(() => {
			if (selectedSubtitleTrackId) applySoftSubtitle(selectedSubtitleTrackId);
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
		</header>

		{#if playable && playlistUrl}
			<!-- svelte-ignore a11y_media_has_caption -->
			<video bind:this={videoEl} controls playsinline></video>
			{#if subtitlesPreparing}
				<p class="preparing" role="status">{copy.subtitlesPreparing}</p>
			{/if}
		{:else if playable && playback.streamUrl}
			{#if subtitlesPreparing}
				<p class="preparing" role="status">{copy.subtitlesPreparing}</p>
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
		{:else if preparingSession}
			<p class="preparing" role="status">{copy.preparingSession}</p>
		{:else if playback.playbackMethod !== 'directPlay'}
			<p class="error">{copy.sessionFailed}</p>
		{/if}

		{#if switchingAudio}
			<p class="preparing" role="status">{copy.switchingAudio}</p>
		{/if}
		{#if switchingSubtitles}
			<p class="preparing" role="status">{copy.switchingSubtitles}</p>
		{/if}

		{#if playable && subtitleTracks.length > 0}
			<SubtitleSwitcher
				tracks={subtitleTracks}
				selectedTrackId={selectedSubtitleTrackId}
				bind:originalStyling
				disabled={switchingAudio || switchingSubtitles}
				onSelect={onSubtitleSelect}
			/>
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
							disabled={switchingAudio || switchingSubtitles}
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
	/* House subtitle styling (V1_PLAN Phase 2 item 7). Opaque night box —
	   light moth text fails contrast on bright scenes (~1.4:1) without it.
	   Platform caption prefs still win where the browser applies them. */
	:global(video::cue) {
		font-family: 'Instrument Sans', system-ui, sans-serif;
		font-size: 1.125rem;
		color: var(--moth);
		background-color: var(--night);
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
