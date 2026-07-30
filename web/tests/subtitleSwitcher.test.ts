/**
 * Subtitle switcher label helpers (one row per track, Rule 4.11).
 *
 *   npm test
 */
import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import {
	defaultOriginalStyling,
	isAssCodec,
	isHlsSelectable,
	isImageBurnCodec,
	selectionNeedsBurnIn,
	subtitleFormatLabel,
	subtitlePrimaryLabel,
	subtitleSecondaryLine,
	type SubtitleTrack
} from '../src/lib/subtitleSwitcher.ts';

function track(partial: Partial<SubtitleTrack> & Pick<SubtitleTrack, 'trackId' | 'codec' | 'render'>): SubtitleTrack {
	return {
		source: 'embedded',
		forced: false,
		sdh: false,
		language: 'eng',
		...partial
	};
}

describe('subtitleSwitcher helpers', () => {
	it('maps codecs to short format labels', () => {
		assert.equal(subtitleFormatLabel('subrip'), 'SRT');
		assert.equal(subtitleFormatLabel('hdmv_pgs_subtitle'), 'PGS');
		assert.equal(subtitleFormatLabel('ass'), 'ASS');
	});

	it('uses language as primary; unknown when missing', () => {
		assert.equal(subtitlePrimaryLabel(track({ trackId: 'e2', codec: 'subrip', render: 'soft' })), 'eng');
		assert.equal(
			subtitlePrimaryLabel(
				track({ trackId: 'e2', codec: 'subrip', render: 'soft', language: null })
			),
			'Unknown'
		);
	});

	it('image secondary always burned; ASS toggles with Original styling', () => {
		const pgs = track({ trackId: 'e3', codec: 'hdmv_pgs_subtitle', render: 'burnIn' });
		assert.equal(subtitleSecondaryLine(pgs, false), 'PGS · always burned in');
		const ass = track({ trackId: 'e2', codec: 'ass', render: 'burnIn' });
		assert.equal(subtitleSecondaryLine(ass, false), 'ASS · house style');
		assert.equal(subtitleSecondaryLine(ass, true), 'ASS · burned in');
		const soft = track({ trackId: 'e1', codec: 'subrip', render: 'soft', url: '/x.vtt' });
		assert.equal(subtitleSecondaryLine(soft, false), 'SRT · house style');
	});

	it('defaults Original styling on only for burn-in-only ASS', () => {
		assert.equal(
			defaultOriginalStyling(track({ trackId: 'e2', codec: 'ass', render: 'burnIn' })),
			true
		);
		assert.equal(
			defaultOriginalStyling(
				track({ trackId: 'e2', codec: 'ass', render: 'soft', url: '/x.vtt' })
			),
			false
		);
		assert.equal(
			defaultOriginalStyling(track({ trackId: 'e1', codec: 'subrip', render: 'soft' })),
			false
		);
	});

	it('selectionNeedsBurnIn for image and Original styling ASS', () => {
		assert.ok(isAssCodec('SSA'));
		assert.ok(isImageBurnCodec('hdmv_pgs_subtitle'));
		const pgs = track({ trackId: 'e3', codec: 'hdmv_pgs_subtitle', render: 'burnIn' });
		assert.equal(selectionNeedsBurnIn(pgs, false), true);
		const ass = track({ trackId: 'e2', codec: 'ass', render: 'burnIn' });
		assert.equal(selectionNeedsBurnIn(ass, false), false);
		assert.equal(selectionNeedsBurnIn(ass, true), true);
		const softAss = track({ trackId: 'e2', codec: 'ass', render: 'soft', url: '/x.vtt' });
		assert.equal(selectionNeedsBurnIn(softAss, true), true);
		assert.equal(selectionNeedsBurnIn(softAss, false), false);
	});

	it('HLS selectable requires complete readiness', () => {
		assert.equal(
			isHlsSelectable(
				track({
					trackId: 'e1',
					codec: 'subrip',
					render: 'soft',
					readiness: 'partial',
					url: '/x.vtt'
				})
			),
			false
		);
		assert.equal(
			isHlsSelectable(
				track({
					trackId: 'e1',
					codec: 'subrip',
					render: 'soft',
					readiness: 'complete',
					url: '/x.vtt'
				})
			),
			true
		);
	});
});
