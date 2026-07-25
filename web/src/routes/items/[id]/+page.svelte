<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { api } from '$lib/api/client';
	import { copy } from '$lib/copy';
	import type { components } from '$lib/api/schema';

	type MediaItem = components['schemas']['MediaItem'];
	type PlaybackInfo = components['schemas']['PlaybackInfo'];

	let item = $state<MediaItem | null>(null);
	let playback = $state<PlaybackInfo | null>(null);
	let error = $state<string | null>(null);

	const itemId = $derived(Number(page.params.id));

	const playable = $derived(
		playback != null &&
			(playback.playbackMethod === 'directPlay' ||
				(playback.playbackMethod === 'remux' && playback.remuxState === 'ready'))
	);

	onMount(() => {
		let alive = true;

		(async () => {
			item = await api.getItem(itemId);
			playback = await api.getPlaybackInfo(itemId);
			// Remux runs as a background job (ADR-0006): re-POST while slots are
			// busy (notStarted), poll until ready or failed.
			while (
				alive &&
				playback.playbackMethod === 'remux' &&
				playback.remuxState !== 'ready' &&
				playback.remuxState !== 'failed'
			) {
				if (playback.remuxState === 'notStarted') {
					await api.startRemux(itemId);
				}
				await new Promise((r) => setTimeout(r, 400));
				if (!alive) return;
				playback = await api.getPlaybackInfo(itemId);
			}
		})().catch((e: Error) => {
			error = e.message;
		});

		return () => {
			alive = false;
		};
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
			</p>
			<p class="reason">{playback.reason}</p>
		</header>

		{#if playable}
			<!-- svelte-ignore a11y_media_has_caption -->
			<video controls playsinline src={playback.streamUrl}>
				Your browser cannot play this file directly.
			</video>
			{#if playback.playbackMethod === 'remux'}
				<p class="preparing">{copy.remuxSubtitleNote}</p>
			{/if}
		{:else if playback.playbackMethod === 'remux' && playback.remuxState === 'failed'}
			<p class="error" role="alert">
				{copy.remuxFailed}
				{#if playback.remuxError}({playback.remuxError}){/if}
			</p>
		{:else if playback.playbackMethod === 'remux'}
			<p class="preparing" role="status">{copy.preparingPlayback}</p>
		{:else}
			<p class="error">{copy.needsTranscode}</p>
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
	.error {
		color: var(--dusk);
	}
	.preparing {
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
		color: var(--moth-dim);
	}
</style>
