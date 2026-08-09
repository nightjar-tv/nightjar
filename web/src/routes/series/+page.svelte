<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { api } from '$lib/api/client';
	import { copy } from '$lib/copy';
	import type { components } from '$lib/api/schema';

	type SeriesDetail = components['schemas']['SeriesDetail'];
	type SeriesEpisode = components['schemas']['SeriesEpisode'];

	let series = $state<SeriesDetail | null>(null);
	let error = $state<string | null>(null);

	const seriesKey = $derived(page.url.searchParams.get('seriesKey') ?? '');
	// Which library the viewer came from, so the crumb can go back. UI state
	// the client already holds; the key itself is opaque and is never parsed.
	const from = $derived(page.url.searchParams.get('from'));

	onMount(() => {
		api
			.getSeries(seriesKey)
			.then((detail) => {
				series = detail;
			})
			.catch((e: Error) => {
				error = e.message;
			});
	});

	function number(episode: SeriesEpisode): string {
		if (episode.season == null || episode.episode == null) return '';
		return `S${String(episode.season).padStart(2, '0')}E${String(episode.episode).padStart(2, '0')}`;
	}

	/// Shown only when the filename disagrees with the canonical numbering, so
	/// an absolute-numbered or specials-shifted show is visible as such rather
	/// than silently reordered.
	function fileNumber(episode: SeriesEpisode): string {
		if (episode.fileSeason == null || episode.fileEpisode == null) return '';
		if (
			episode.fileSeason === episode.season &&
			episode.fileEpisode === episode.episode
		) {
			return '';
		}
		return `file S${String(episode.fileSeason).padStart(2, '0')}E${String(episode.fileEpisode).padStart(2, '0')}`;
	}

	function meta(episode: SeriesEpisode): string {
		const bits: string[] = [];
		const disagreement = fileNumber(episode);
		if (disagreement) bits.push(disagreement);
		if (episode.airDate) bits.push(episode.airDate);
		if (episode.metadataStatus !== 'ready') bits.push(episode.metadataStatus);
		if (episode.probeStatus === 'indexed') bits.push('probing');
		if (episode.probeStatus === 'error') bits.push('probe error');
		return bits.join(' · ');
	}
</script>

<svelte:head>
	<title>{series?.title ?? 'series'} · nightjar</title>
</svelte:head>

<main>
	<p class="crumb">
		<a href="/">nightjar</a> /
		{#if from}<a href="/libraries/{from}">library</a> / {/if}series
	</p>

	{#if error}
		<p class="error" role="alert">{error}</p>
	{/if}

	{#if series}
		<header>
			<h1>{series.title}</h1>
			<p class="path">
				{series.year ?? ''}
				{series.year ? ' · ' : ''}{series.kind === 'movie'
					? copy.versionCount(series.itemCount)
					: copy.episodeCount(series.itemCount)}
				{#if series.identity === 'entityOnly'}· {copy.identityEntityOnly}{/if}
				{#if series.identity === 'unidentified'}· {copy.identityUnidentified}{/if}
			</p>
			{#if series.plot}<p class="plot">{series.plot}</p>{/if}
		</header>

		{#each series.seasons as season (season.season)}
			<h2>{season.season === 0 ? 'Specials' : `Season ${season.season}`}</h2>
			<ul class="grid">
				{#each season.episodes as episode (episode.itemId)}
					<li>
						<a href="/items/{episode.itemId}">
							<span class="row">
								<span class="title">{number(episode)} {episode.title}</span>
							</span>
							<span class="meta">{meta(episode)}</span>
						</a>
					</li>
				{/each}
			</ul>
		{/each}

		{#if series.unnumbered.length > 0}
			<h2>{series.kind === 'movie' ? 'Versions' : 'Unnumbered'}</h2>
			<p class="hint">
				{series.kind === 'movie' ? copy.movieVersions : copy.seriesUnnumbered}
			</p>
			<ul class="grid">
				{#each series.unnumbered as episode (episode.itemId)}
					<li>
						<a href="/items/{episode.itemId}">
							<span class="row">
								<span class="title">{episode.title}</span>
								{#if series.kind !== 'movie'}
									<span class="badge bad">{copy.identityUnidentified}</span>
								{/if}
							</span>
							<span class="meta">{episode.path}</span>
						</a>
					</li>
				{/each}
			</ul>
		{/if}
	{/if}
</main>

<style>
	main {
		max-width: 48rem;
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
		font-size: 1.25rem;
		margin: 2rem 0 0;
	}
	.path,
	.plot,
	.meta,
	.hint {
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
		color: var(--moth-dim);
	}
	.row {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		gap: 0.75rem;
	}
	.badge {
		flex-shrink: 0;
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.75rem;
		padding: 0.15rem 0.4rem;
		border-radius: 4px;
		border: 1px solid var(--night-line);
		color: var(--moth-dim);
	}
	.badge.bad {
		border-color: var(--dusk);
		color: var(--dusk);
	}
	.grid {
		list-style: none;
		margin: 0.5rem 0 0;
		padding: 0;
		display: grid;
		gap: 0.25rem;
	}
	.grid a {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		padding: 0.6rem 1rem;
		border-radius: 8px;
		color: inherit;
		text-decoration: none;
	}
	.grid a:hover,
	.grid a:focus-visible {
		background: var(--night-raised);
	}
	.grid a:focus-visible {
		outline: 2px solid var(--dusk);
	}
	.title {
		font-size: 1rem;
	}
	.error {
		color: var(--dusk);
	}
</style>
