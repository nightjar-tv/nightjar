/**
 * Regression checks for ADR-0017 attach pick and seek-suppress transitions.
 *
 *   npm test
 *   node --experimental-strip-types --test tests/hlsAttachBackend.test.ts
 */
import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import {
	chooseAttachBackend,
	isAppleMobileOs,
	isAppleWebKitHlsEngine,
	seekSuppressOnSeeked,
	seekSuppressRafMayClear
} from '../src/lib/hlsAttachBackend.ts';

const DESKTOP_SAFARI =
	'Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15';
const IPHONE_SAFARI =
	'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1';
const IPAD_DESKTOP_UA =
	'Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15';
const CHROME =
	'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36';

describe('isAppleWebKitHlsEngine', () => {
	it('accepts desktop Safari', () => {
		assert.equal(isAppleWebKitHlsEngine(DESKTOP_SAFARI), true);
	});
	it('rejects Chromium even when UA contains Safari', () => {
		assert.equal(isAppleWebKitHlsEngine(CHROME), false);
	});
});

describe('isAppleMobileOs', () => {
	it('detects iPhone', () => {
		assert.equal(isAppleMobileOs(IPHONE_SAFARI, 5), true);
	});
	it('detects iPadOS masquerading as Macintosh via maxTouchPoints', () => {
		assert.equal(isAppleMobileOs(IPAD_DESKTOP_UA, 5), true);
	});
	it('does not treat desktop Safari as mobile', () => {
		assert.equal(isAppleMobileOs(DESKTOP_SAFARI, 0), false);
		assert.equal(isAppleMobileOs(DESKTOP_SAFARI, 1), false);
	});
});

describe('chooseAttachBackend (ADR-0017)', () => {
	it('desktop Safari → hls-js when MSE works', () => {
		assert.equal(
			chooseAttachBackend({
				ua: DESKTOP_SAFARI,
				maxTouchPoints: 0,
				canPlayNativeHls: true,
				hlsJsSupported: true,
				forceNativeHls: false
			}),
			'hls-js'
		);
	});
	it('iPhone → native-hls', () => {
		assert.equal(
			chooseAttachBackend({
				ua: IPHONE_SAFARI,
				maxTouchPoints: 5,
				canPlayNativeHls: true,
				hlsJsSupported: true,
				forceNativeHls: false
			}),
			'native-hls'
		);
	});
	it('iPadOS desktop UA + touch → native-hls', () => {
		assert.equal(
			chooseAttachBackend({
				ua: IPAD_DESKTOP_UA,
				maxTouchPoints: 5,
				canPlayNativeHls: true,
				hlsJsSupported: true,
				forceNativeHls: false
			}),
			'native-hls'
		);
	});
	it('njNativeHls=1 forces native on desktop Safari', () => {
		assert.equal(
			chooseAttachBackend({
				ua: DESKTOP_SAFARI,
				maxTouchPoints: 0,
				canPlayNativeHls: true,
				hlsJsSupported: true,
				forceNativeHls: true
			}),
			'native-hls'
		);
	});
	it('Chrome stays hls-js', () => {
		assert.equal(
			chooseAttachBackend({
				ua: CHROME,
				maxTouchPoints: 0,
				canPlayNativeHls: true,
				hlsJsSupported: true,
				forceNativeHls: false
			}),
			'hls-js'
		);
	});
});

describe('seekSuppress transitions', () => {
	it('first seeked with land pending applies and keeps suppress', () => {
		assert.deepEqual(seekSuppressOnSeeked(true), {
			applyLand: true,
			clearSuppress: false
		});
	});
	it('second seeked clears suppress', () => {
		assert.deepEqual(seekSuppressOnSeeked(false), {
			applyLand: false,
			clearSuppress: true
		});
	});
	it('rAF safety clears one-shot suppress of matching gen', () => {
		assert.equal(seekSuppressRafMayClear(3, 3, false), true);
	});
	it('rAF safety does not clear mid land-retarget (same-value stuck risk)', () => {
		// Direct-target arm can miss seeked; two-step still needs timeout,
		// but rAF must not clear while landRetarget is pending.
		assert.equal(seekSuppressRafMayClear(3, 3, true), false);
	});
	it('rAF safety ignores stale gen after re-arm', () => {
		assert.equal(seekSuppressRafMayClear(2, 3, false), false);
	});
});
