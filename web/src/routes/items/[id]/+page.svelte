<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { api } from '$lib/api/client';
	import { copy } from '$lib/copy';
	import { resumePositionMs } from '$lib/resumePosition';
	import type { components } from '$lib/api/schema';

	type MediaItemDetail = components['schemas']['MediaItemDetail'];
	type PlaybackInfo = components['schemas']['PlaybackInfo'];
	type ItemArtwork = components['schemas']['ItemArtwork'];

	let item = $state<MediaItemDetail | null>(null);
	let playback = $state<PlaybackInfo | null>(null);
	let error = $state<string | null>(null);
	let resumeMs = $state(0);

	const itemId = $derived(Number(page.params.id));

	function artworkUrl(kind: ItemArtwork['kind']): string | null {
		return item?.artwork?.find((a) => a.kind === kind)?.url ?? null;
	}

	// Absent means this title has no artwork of that kind, so the layout is
	// drawn without it rather than with a broken image (ADR-0027 §2 step 3).
	const backdrop = $derived(artworkUrl('backdrop'));
	const poster = $derived(artworkUrl('poster'));
	const logo = $derived(artworkUrl('logo'));

	const playHref = $derived.by(() => {
		const params = new URLSearchParams({ play: '1' });
		if (resumeMs > 0) params.set('startMs', String(resumeMs));
		return `/items/${itemId}/watch?${params}`;
	});

	onMount(() => {
		(async () => {
			item = await api.getItem(itemId);
			// playbackInfo on item-page load, not at press-play. ADR-0023 §9.1
			// makes this the demand trigger for the keyframe map, and the ADR
			// justified that on the client fetching it "seconds before play".
			// This page is what makes that true: a synopsis takes longer to
			// read than a ~130 ms map build takes to finish.
			playback = await api.getPlaybackInfo(itemId);
			resumeMs = (await resumePositionMs(itemId)) ?? 0;
		})().catch((e: Error) => {
			error = e.message;
		});
	});

	function fileFacts(): string {
		if (!item) return '';
		const bits: string[] = [];
		if (item.container) bits.push(item.container);
		if (item.videoCodec) bits.push(item.videoCodec);
		if (item.audioCodec) bits.push(item.audioCodec);
		if (item.width && item.height) bits.push(`${item.width}x${item.height}`);
		bits.push(`${(item.sizeBytes / 1_000_000_000).toFixed(2)} GB`);
		bits.push(`probe ${item.probeStatus}`);
		bits.push(`metadata ${item.metadataStatus}`);
		if (item.scanError) bits.push('probe error');
		return bits.join(' · ');
	}

	function ratingLine(r: components['schemas']['Rating']): string {
		return r.votes != null
			? `${r.source} ${r.value} (${r.votes.toLocaleString()} votes)`
			: `${r.source} ${r.value}`;
	}

	function castLine(c: components['schemas']['CastMember']): string {
		return c.role ? `${c.name} as ${c.role}` : c.name;
	}
</script>

<svelte:head>
	<title>{item?.canonicalTitle ?? item?.title ?? 'item'} · nightjar</title>
</svelte:head>

<main>
	<p class="crumb">
		<a href="/">nightjar</a>
		{#if item}
			/ <a href="/libraries/{item.libraryId}">library</a>
			{#if item.seriesKey}
				/ <a href="/series?seriesKey={encodeURIComponent(item.seriesKey)}"
					>{item.showTitle ?? 'series'}</a
				>
			{/if}
			/
		{/if}
		item
	</p>

	{#if error}
		<p class="error" role="alert">{error}</p>
	{/if}

	{#if item}
		{#if backdrop}
			<img class="backdrop" src={backdrop} alt="" />
		{/if}

		<header>
			{#if logo}
				<img class="logo" src={logo} alt={item.canonicalTitle ?? item.title} />
			{:else}
				<h1>{item.canonicalTitle ?? item.title}</h1>
			{/if}
			<p class="meta">
				{item.kind}
				{#if item.season != null && item.episode != null}
					· S{String(item.season).padStart(2, '0')}E{String(item.episode).padStart(2, '0')}
				{/if}
				{#if item.year}· {item.year}{/if}
				{#if item.airDate}· {item.airDate}{/if}
				{#if item.runtimeMinutes}· {item.runtimeMinutes} min{/if}
				{#if item.genres?.length}· {item.genres.join(', ')}{/if}
			</p>

			<a class="play" href={playHref}>
				{resumeMs > 0 ? copy.playResume(resumeMs / 1000) : copy.play}
			</a>
		</header>

		<div class="body">
			{#if poster}
				<img class="poster" src={poster} alt="" />
			{/if}
			<div class="text">
				{#if item.plot}
					<p class="plot">{item.plot}</p>
				{/if}

				{#if item.ratings?.length}
					<h2>Ratings</h2>
					<ul class="facts">
						{#each item.ratings as rating (rating.source)}
							<li>{ratingLine(rating)}</li>
						{/each}
					</ul>
				{/if}

				{#if item.cast?.length}
					<h2>Cast</h2>
					<ul class="facts">
						{#each item.cast as member, i (`${member.name}:${i}`)}
							<li>{castLine(member)}</li>
						{/each}
					</ul>
				{/if}

				<h2>File</h2>
				<p class="facts">{fileFacts()}</p>
				<p class="facts path">{item.path}</p>
				<p class="facts">{item.itemKey}</p>
				{#if playback}
					<!-- Why this title plays the way it does: the one thing a
					     viewer cannot otherwise find out about a slow title. -->
					<p class="facts">{playback.playbackMethod} · {playback.reason}</p>
				{/if}
			</div>
		</div>
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
	h2 {
		font-family: 'Bricolage Grotesque', system-ui, sans-serif;
		font-size: 1.125rem;
		margin: 1.5rem 0 0.25rem;
	}
	.backdrop {
		display: block;
		width: 100%;
		border-radius: 8px;
	}
	.logo {
		display: block;
		max-width: 20rem;
		max-height: 6rem;
	}
	.poster {
		width: 12rem;
		height: auto;
		border-radius: 8px;
	}
	.body {
		display: flex;
		flex-wrap: wrap;
		gap: 1.5rem;
		margin-top: 1.5rem;
	}
	.text {
		flex: 1 1 20rem;
	}
	.meta,
	.facts,
	.plot {
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
		color: var(--moth-dim);
	}
	.path {
		word-break: break-all;
	}
	ul.facts {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.play {
		display: inline-block;
		margin-top: 1rem;
		font: inherit;
		font-weight: 600;
		padding: 0.6rem 0.9rem;
		border-radius: 8px;
		background: var(--dusk);
		color: var(--night);
		text-decoration: none;
	}
	.play:focus-visible {
		outline: 2px solid var(--dusk);
		outline-offset: 2px;
	}
	.error {
		color: var(--dusk);
	}
</style>
