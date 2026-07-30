<script lang="ts">
	import { copy } from '$lib/copy';
	import DuskStrip from '$lib/components/DuskStrip.svelte';
	import {
		defaultOriginalStyling,
		showOriginalStylingControl,
		subtitlePrimaryLabel,
		subtitleSecondaryLine,
		type SubtitleTrack
	} from '$lib/subtitleSwitcher';

	let {
		tracks,
		selectedTrackId = null,
		originalStyling = $bindable(false),
		disabled = false,
		onSelect
	}: {
		tracks: SubtitleTrack[];
		selectedTrackId?: string | null;
		originalStyling?: boolean;
		disabled?: boolean;
		onSelect: (trackId: string | null) => void;
	} = $props();

	const selected = $derived(
		tracks.find((t) => t.trackId === selectedTrackId) ?? null
	);
	const showOriginal = $derived(showOriginalStylingControl(selected));

	function pick(trackId: string | null) {
		if (disabled) return;
		if (trackId == null) {
			originalStyling = false;
			onSelect(null);
			return;
		}
		const track = tracks.find((t) => t.trackId === trackId);
		if (!track) return;
		originalStyling = defaultOriginalStyling(track);
		onSelect(trackId);
	}

	function readinessOf(
		track: SubtitleTrack
	): 'preparing' | 'partial' | 'complete' | null {
		if (track.render !== 'soft') return null;
		if (track.readiness === 'preparing') return 'preparing';
		if (track.readiness === 'partial') return 'partial';
		if (track.readiness === 'complete') return 'complete';
		return null;
	}
</script>

<div class="switcher">
	<p class="legend" id="subtitle-switcher-label">{copy.subtitleTrack}</p>
	<ul class="rows" role="listbox" aria-labelledby="subtitle-switcher-label">
		<li role="option" aria-selected={selectedTrackId == null}>
			<button
				type="button"
				class="row"
				class:selected={selectedTrackId == null}
				{disabled}
				onclick={() => pick(null)}
			>
				<span class="check" aria-hidden="true">
					{#if selectedTrackId == null}
						<svg
							xmlns="http://www.w3.org/2000/svg"
							width="16"
							height="16"
							viewBox="0 0 24 24"
							fill="none"
							stroke="currentColor"
							stroke-width="1.5"
							stroke-linecap="round"
							stroke-linejoin="round"
						>
							<path d="M20 6 9 17l-5-5" />
						</svg>
					{/if}
				</span>
				<span class="body">
					<span class="primary">{copy.subtitleOff}</span>
				</span>
			</button>
		</li>
		{#each tracks as track (track.trackId)}
			{@const readiness = readinessOf(track)}
			<li role="option" aria-selected={selectedTrackId === track.trackId}>
				<button
					type="button"
					class="row"
					class:selected={selectedTrackId === track.trackId}
					{disabled}
					onclick={() => pick(track.trackId)}
				>
					<span class="check" aria-hidden="true">
						{#if selectedTrackId === track.trackId}
							<svg
								xmlns="http://www.w3.org/2000/svg"
								width="16"
								height="16"
								viewBox="0 0 24 24"
								fill="none"
								stroke="currentColor"
								stroke-width="1.5"
								stroke-linecap="round"
								stroke-linejoin="round"
							>
								<path d="M20 6 9 17l-5-5" />
							</svg>
						{/if}
					</span>
					<span class="body">
						<span class="primary-row">
							<span class="primary">{subtitlePrimaryLabel(track)}</span>
							{#if track.forced}
								<span class="badge">{copy.subtitleForced}</span>
							{/if}
							{#if track.sdh}
								<span class="badge">{copy.subtitleSdh}</span>
							{/if}
							{#if readiness === 'preparing' || readiness === 'partial'}
								<DuskStrip
									{readiness}
									label={copy.subtitlesPreparing}
								/>
							{/if}
						</span>
						<span class="secondary">
							{subtitleSecondaryLine(
								track,
								selectedTrackId === track.trackId && originalStyling
							)}
						</span>
					</span>
				</button>
			</li>
		{/each}
	</ul>

	{#if showOriginal}
		<label class="original">
			<input
				type="checkbox"
				checked={originalStyling}
				{disabled}
				onchange={(e) => {
					originalStyling = e.currentTarget.checked;
					if (selectedTrackId) onSelect(selectedTrackId);
				}}
			/>
			<span>{copy.subtitleOriginalStyling}</span>
		</label>
	{/if}
</div>

<style>
	.switcher {
		margin-top: 1rem;
		padding: 0.75rem 1rem;
		border: 1px solid var(--moth-dim);
		border-radius: 8px;
		background: var(--night-raised);
	}
	.legend {
		margin: 0 0 0.5rem;
		font-size: 0.875rem;
		color: var(--moth-dim);
	}
	.rows {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
	}
	.row {
		display: flex;
		align-items: flex-start;
		gap: 0.5rem;
		width: 100%;
		margin: 0;
		padding: 0.5rem 0.6rem;
		border: 2px solid transparent;
		border-radius: 8px;
		background: transparent;
		color: var(--moth);
		text-align: left;
		cursor: pointer;
		font: inherit;
	}
	.row:hover:not(:disabled) {
		background: var(--night);
	}
	.row:focus-visible {
		outline: 2px solid var(--dusk);
		outline-offset: 2px;
	}
	.row:disabled {
		opacity: 0.55;
		cursor: not-allowed;
	}
	.row.selected {
		border-color: var(--dusk);
	}
	.check {
		display: flex;
		align-items: center;
		justify-content: center;
		width: 1rem;
		height: 1.25rem;
		flex-shrink: 0;
		color: var(--dusk);
	}
	.body {
		display: flex;
		flex-direction: column;
		gap: 0.15rem;
		min-width: 0;
	}
	.primary-row {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.35rem 0.5rem;
	}
	.primary {
		font-size: 1rem;
		line-height: 1.25;
	}
	.secondary {
		font-family: 'Spline Sans Mono', ui-monospace, monospace;
		font-size: 0.875rem;
		color: var(--moth-dim);
	}
	.badge {
		display: inline-block;
		padding: 0.1rem 0.35rem;
		border-radius: 4px;
		background: var(--dusk);
		color: var(--night);
		font-size: 0.75rem;
		font-weight: 600;
		line-height: 1.2;
	}
	.original {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		margin-top: 0.75rem;
		padding-top: 0.75rem;
		border-top: 1px solid var(--moth-dim);
		font-size: 0.875rem;
		cursor: pointer;
	}
	.original input {
		width: 1rem;
		height: 1rem;
		accent-color: var(--dusk);
	}
	.original input:focus-visible {
		outline: 2px solid var(--dusk);
		outline-offset: 2px;
	}
</style>
