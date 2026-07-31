# ABR throttle probe (bake-off Step 3)

Static three-rung HLS ladder + byte-rate proxy. Compare mpv (libmpv / ffmpeg
HLS demuxer) with hls.js under the same ~1 Mbps cap.

## Ladder

Encoded from Elementary 3x05 @ t=600s, 60 s window, forced IDRs every 2 s:

| rung | target | resolution | BANDWIDTH tag |
|---|---:|---|---:|
| hi | 4 Mbps | 1280×720 | 4.5 Mbps |
| mid | 1.5 Mbps | 854×480 | 1.7 Mbps |
| lo | 0.6 Mbps | 640×360 | 0.75 Mbps |

`static/` is regenerated locally (gitignored media). Rebuild:

```bash
# see encode commands in the bake-off note / prior session transcript
```

## Serve

```bash
python3 scripts/abr_throttle_probe/throttle_serve.py --port 8765 --bps 125000
# 125000 B/s ≈ 1.0 Mbps
```

## Clients

```bash
# mpv — default --hls-bitrate=max (no auto ABR choice)
mpv --no-config --vo=null --ao=null --ytdl=no --hls-bitrate=max \
  http://127.0.0.1:8765/master.m3u8

# hls.js via system Chrome CDP
node scripts/abr_throttle_probe/hlsjs_cdp.mjs
```

Access log: `scripts/abr_throttle_probe/access.jsonl`  
Results: `notes/client-arch/abr-*.json`
