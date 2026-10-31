# Bot-detection and block risk (assessed 2026-10-31)

Overall: **HIGH** for the first full sync of a large library, **LOW-MEDIUM** for steady-state cron runs.
The session is more likely to die from invalid cookies (exit 77) than from rate limiting (exit 75).

## Facts vs estimates
- Facts: what the code does, the official API quotas, and the DBSC rollout.
- Estimates: per-item timing and block thresholds. Google publishes no limits for the web RPCs.

## Volume (estimated)
- Steady state: about N/500 listing RPCs plus album pages, a few hundred RPCs per run. That is human-scale.
- First sync, per item: `VrseUb`, `fDcn4b`, then the media GET.
  - 10k items: about 20k RPCs and 40-60 GB, 4-6 h.
  - 50k items: about 100k RPCs and 200-300 GB, 20-30 h of non-stop 1-2 RPC/s.
- Timing: RPCs were exactly 500 ms apart with no jitter, which is a textbook machine pattern.

## Fingerprint
- The UA claims Chrome, but the TLS ClientHello and HTTP/2 SETTINGS are reqwest+rustls, so JA4 says "not Chrome".
- Chrome's sec-ch-ua / sec-fetch-* headers are missing.
- gpwc (plain Python `requests`) works, so Google evidently does not hard-block on fingerprint.

## Session and cookies
- Google's page JS normally rotates `__Secure-*PSIDTS` through `accounts.google.com/RotateCookies` every ~10 min.
  We only save rotations that Google pushes to us.
- **DBSC (device-bound session credentials):**
  - Default-on in Chrome 146+ on Windows since 2026-05-25. The macOS rollout started with Chrome 147. Not on Linux.
  - Cookies exported from such a browser are likely unusable within minutes.
  - Export from **Firefox**, or Chrome on Linux.
- Using the exported session in a browser at the same time makes one copy go stale.
- Running from a datacenter/VPS IP is a known anti-abuse trigger. A home server (Unraid) on the same IP as the browser is fine.

## Evidence
- gotohp#114 (mobile API): 429 "PhotosFeService RPC calls per minute per user", limit 480/min.
- gphotos-sync #305/#502 (official API): bulk downloads of hundreds of GB hit 429s from lh3.googleusercontent.com,
  then roughly 24 h blocks.
- notebooklm-py#345: cookies left without a keepalive died at about 20 min. With RotateCookies they lasted weeks.
- Official API quota: 10k requests/day plus 75k media-byte requests/day per project. These don't apply to the web endpoints,
  but they show the scale Google considers normal.

## Expected failure modes
| Scenario | Expected outcome |
|---|---|
| Cookies exported from DBSC-enabled Chrome | exit 77 within minutes to hours |
| Cookies exported from Firefox / Linux Chrome / a closed private window | days to weeks |
| Unthrottled 50k-item first sync | probable 429 or `/sorry` somewhere in the first 5-20k items |

## Mitigations (ranked)
1. Daily item/byte budget, so the first sync spreads over days.
2. Cooldown persisted across runs and escalated after each block (1 h, then 4 h, 24 h, 72 h). Captcha/`/sorry` on media URLs counts as a block.
3. Jitter on the RPC delay, plus periodic pauses.
4. Fewer RPCs: `EWgK9e` bulk info instead of per-item `fDcn4b`. Loses location and camera data, and the payload is untested.
5. Cookie hygiene docs. Optional periodic RotateCookies, which is unverified.
6. Browser-like headers: current Chrome UA, accept-language, sec-fetch-*, no Origin on media GETs.
7. Re-index only albums that changed.
8. TLS/H2 impersonation (`wreq`): high effort (BoringSSL in the Nix cross-builds), unproven benefit.
