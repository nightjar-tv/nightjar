/**
 * Self-check for web/src/lib/nativeHlsSubs.ts (web/ has no unit-test runner).
 * Run: node --experimental-strip-types scripts/check_native_hls_subs.mjs
 */
import assert from 'node:assert/strict';
import {
	SEGMENT_MS,
	parseSubtitleTrackIdsFromMaster,
	parseWebVttCues,
	segmentIndexAtSeconds,
	segmentVttName,
	sessionBaseFromMaster,
	subtitleSegmentUrl
} from '../web/src/lib/nativeHlsSubs.ts';

assert.equal(SEGMENT_MS, 2000);
assert.equal(segmentIndexAtSeconds(0), 0);
assert.equal(segmentIndexAtSeconds(1.999), 0);
assert.equal(segmentIndexAtSeconds(2), 1);
assert.equal(segmentIndexAtSeconds(656.2), 328);
assert.equal(segmentVttName(0), 'seg000.vtt');
assert.equal(segmentVttName(60), 'seg060.vtt');
assert.equal(segmentVttName(1000), 'seg1000.vtt');
assert.equal(
	sessionBaseFromMaster('/api/v0/sessions/s1/master.m3u8#t=10'),
	'/api/v0/sessions/s1'
);
assert.equal(
	subtitleSegmentUrl('/api/v0/sessions/s1', 't0', 60),
	'/api/v0/sessions/s1/subs/t0/seg060.vtt'
);

const master = `#EXTM3U
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI="subs/0.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=5000000,SUBTITLES="subs"
index.m3u8
`;
assert.deepEqual(parseSubtitleTrackIdsFromMaster(master), ['0']);

const vtt = `WEBVTT

120000
00:02:00.000 --> 00:02:03.500
Hello there

NOTE skip me

120500
00:02:00.500 --> 00:02:01.000

`;
const cues = parseWebVttCues(vtt);
assert.equal(cues.length, 1);
assert.equal(cues[0].id, '120000');
assert.equal(cues[0].startSec, 120);
assert.equal(cues[0].endSec, 123.5);
assert.equal(cues[0].text, 'Hello there');

console.log('check_native_hls_subs: ok');
