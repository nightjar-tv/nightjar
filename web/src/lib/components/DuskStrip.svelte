<script lang="ts">
	/**
	 * Inline dusk strip for subtitle readiness (BRAND §5). Preparing = glow
	 * pass; partial = half lit; complete = full. Static under reduced motion.
	 */
	let {
		readiness = 'preparing',
		label
	}: {
		readiness?: 'preparing' | 'partial' | 'complete';
		label: string;
	} = $props();

	const lit = $derived(
		readiness === 'complete' ? 8 : readiness === 'partial' ? 4 : 0
	);
</script>

<div
	class="dusk-strip"
	class:preparing={readiness === 'preparing'}
	role="status"
	aria-label={label}
>
	{#each Array(8) as _, i}
		<span class="dot" class:lit={i < lit}></span>
	{/each}
</div>

<style>
	.dusk-strip {
		display: inline-flex;
		align-items: center;
		gap: 3px;
		height: 0.75rem;
		vertical-align: middle;
	}
	.dot {
		width: 4px;
		height: 4px;
		border-radius: 50%;
		background: var(--night);
		box-shadow: inset 0 0 0 1px var(--moth-dim);
		flex-shrink: 0;
	}
	.dot.lit {
		background: var(--dusk);
		box-shadow: none;
	}
	.preparing .dot {
		animation: dusk-pass 2.6s linear infinite;
	}
	.preparing .dot:nth-child(1) {
		animation-delay: 0s;
	}
	.preparing .dot:nth-child(2) {
		animation-delay: 0.12s;
	}
	.preparing .dot:nth-child(3) {
		animation-delay: 0.24s;
	}
	.preparing .dot:nth-child(4) {
		animation-delay: 0.36s;
	}
	.preparing .dot:nth-child(5) {
		animation-delay: 0.48s;
	}
	.preparing .dot:nth-child(6) {
		animation-delay: 0.6s;
	}
	.preparing .dot:nth-child(7) {
		animation-delay: 0.72s;
	}
	.preparing .dot:nth-child(8) {
		animation-delay: 0.84s;
	}
	@keyframes dusk-pass {
		0%,
		100% {
			background: var(--night);
			box-shadow: inset 0 0 0 1px var(--moth-dim);
		}
		40%,
		60% {
			background: var(--dusk);
			box-shadow: none;
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.preparing .dot {
			animation: none;
			background: var(--moth-dim);
			box-shadow: none;
		}
	}
</style>
