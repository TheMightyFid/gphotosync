# photos.google.com internal RPC spec (cookie-based, no browser)

Research notes for gphotosync. Everything here is reverse-engineered by third parties and
undocumented by Google; it can change without notice. Items marked **[UNCERTAIN]** were not
confirmed in source or disagree between sources.

## Sources (fetched 2026-10-31)

| tag | repo / URL | files |
|---|---|---|
| GPTK | https://github.com/xob0t/Google-Photos-Toolkit (main) | `src/api/api.ts`, `src/api/parser.ts`, `src/api/api-utils.ts`, `src/api/api-utils-default-presets.ts`, `src/windowGlobalData.ts`, `src/gptk-core.ts`, `tests/fixtures/parser/*.json` |
| gpwc | https://github.com/xob0t/google_photos_web_client (HEAD) | `gpwc/client.py`, `gpwc/payloads.py`, `gpwc/parser.py`, `gpwc/utils.py`, `README.md` (Python, **cookies.txt + plain HTTP**: closest to what we are building) |
| mcp | https://github.com/benjamin-di/google-photos-mcp | `docs/PAYLOADS.md`, `README.md`, `src/photos_mcp/{session,transport}.py` |
| nbarari | https://github.com/nbarari/gphotos-storage-recovery | `README.md`, `download-test.mjs`, `batch-download.mjs` |
| kovatch | https://kovatch.medium.com/deciphering-google-batchexecute-74991e4e446c | batchexecute framing |

GPTK has moved from `api.js`/`parser.js` to TypeScript; the code is otherwise the same.

## 1. Auth and transport

### 1.1 Cookies
gpwc loads a whole Netscape `cookies.txt` (all Google cookies) into a `requests.Session` and
sends them unchanged (`client.py` `load_cookies_from_file`). The relevant Google session
cookies (the set mcp accepts and merges from `Set-Cookie`, `session.py` `SET_COOKIE_NAMES`):

```
SID HSID SSID APISID SAPISID SIDCC NID
__Secure-1PSID __Secure-3PSID __Secure-1PSIDTS __Secure-3PSIDTS
__Secure-1PAPISID __Secure-3PAPISID __Secure-1PSIDCC __Secure-3PSIDCC OSID __Secure-OSID
```

Recommendation: send every cookie whose domain matches the request host (`.google.com`,
`photos.google.com`, `.googleusercontent.com`) and merge `Set-Cookie` responses back into
the jar. The minimal subset is **[UNCERTAIN]**; no source tests it.
`__Secure-*PSIDTS` rotates. gpwc README's advice: export the cookies from a private window,
then close it, "so the session is never opened in the browser again". If the browser keeps
using the same session it rotates the cookies and the exported copy goes stale.

Multi-account: use `https://photos.google.com/u/N/` as the base path (gpwc `account_index`).
If you get redirected away from `/u/N/`, that account is not in the cookies.

### 1.2 Bootstrap: `WIZ_global_data`
`GET https://photos.google.com/` (or `/u/N/`) with the cookies. The HTML contains
`<script data-id="_gd">window.WIZ_global_data = {...};</script>`. gpwc reads it like this
(`client.py`):

```python
script_text: str = xml_page.xpath('//script[@data-id="_gd"]/text()')[0]
script_json = script_text.replace("window.WIZ_global_data = ", "").rstrip().rstrip(";")
return json.loads(script_json)
```

The keys we need (GPTK `windowGlobalData.ts`, gpwc `client.py`):

| key | used as | notes |
|---|---|---|
| `SNlM0e` | `at` form field (XSRF token) | required |
| `FdrFJe` | `f.sid` query param | numeric string, may be negative |
| `cfb2h` | `bl` query param | build label, e.g. `boq_photosuiserver_20261019...` |
| `eptZe` (GPTK) / `Im6cmf` (gpwc) | RPC path prefix | GPTK builds `photos.google.com${eptZe}data/batchexecute` (so `eptZe` ends with `/`); gpwc builds `photos.google.com{Im6cmf}/data/batchexecute`. Expected result: `/_/PhotosUi/data/batchexecute` (with `/u/N` in front for other accounts **[UNCERTAIN]**) |
| `oPEP7c` | account email | sanity check |
| `Dbw5Ud` | `rapt` query param | only for locked-folder calls (GPTK) |

Regex fallback (our own suggestion; the sources parse JSON). Run it over the `_gd` script text:

```
"SNlM0e":"([^"]+)"
"FdrFJe":"(-?\d+)"
"cfb2h":"([^"]+)"
"eptZe":"([^"]+)"        (JSON-escaped: "\/_\/PhotosUi\/" -> unescape before use)
"oPEP7c":"([^"]+)"
```

Parsing the whole object with a JSON parser (gpwc approach) is more robust than regexes.
Signed out means a redirect to `accounts.google.com` or `www.google.com/photos/about`, or no `_gd` script
(mcp README "Errors"). Fetch the globals again after a 400 or 403 from batchexecute (mcp `stale_token`).

### 1.3 batchexecute request

```
POST https://photos.google.com/_/PhotosUi/data/batchexecute
     ?rpcids=<id>[,<id>...]&source-path=<path, e.g. />&f.sid=<FdrFJe>&bl=<cfb2h>&pageId=none&rt=c
Content-Type: application/x-www-form-urlencoded;charset=UTF-8
Cookie: ...
body: f.req=<urlencoded JSON>&at=<urlencoded SNlM0e>&
```

GPTK `api.ts` `makeApiRequest` (verbatim):

```ts
const wrappedData: unknown[][][] = [[[rpcid, JSON.stringify(requestData), null, 'generic']]];
const requestDataString = `f.req=${encodeURIComponent(JSON.stringify(wrappedData))}&at=${encodeURIComponent(windowGlobalData.at)}&`;
const params = { rpcids: rpcid, 'source-path': window.location.pathname,
  'f.sid': windowGlobalData['f.sid'], bl: windowGlobalData.bl, pageId: 'none', rt: 'c' };
```

- `f.req` = `[[ [rpcid, JSON-string(payload), null, reqId], ... ]]`. The payload is
  **double-encoded**: a JSON string inside JSON. `reqId` is `"generic"` for a single call.
  gpwc batches several calls in one POST. It sets `rpcids` to the comma-joined list and gives each
  entry a unique id (`uuid4().hex`), which comes back at envelope index 6.
- gpwc omits `pageId`, and `source-path` is its base path (`/` or `/u/N/`). Both variants work.
- Headers: gpwc sends only `content-type`, through `requests`. mcp copies the browser's headers
  as a precaution against bot detection. Its synthesised RPC template is `x-same-domain: 1`,
  `origin: https://photos.google.com`, `referer: https://photos.google.com/`, `accept: */*`,
  a real Chrome `user-agent` plus `sec-ch-ua*` and `sec-fetch-site: same-origin`,
  `sec-fetch-mode: cors` and `sec-fetch-dest: empty` (`session.py`). It also uses TLS
  impersonation (curl_cffi). Recommended: at least `user-agent`, `origin`, `referer` and `x-same-domain: 1`.

### 1.4 Response framing
With `rt=c`, the body is `)]}'` and a blank line, followed by length-prefixed chunks. Each chunk is a
decimal length on its own line, then a JSON array (kovatch). A typical body:

```
)]}'

1234
[["wrb.fr","lcxiM","[[...payload json string...]]",null,null,null,"generic"]]
57
[["di",38],["af.httprm",37,"5314567270682293609",6]]
25
[["e",4,null,null,1290]]
```

Envelope `["wrb.fr", rpcid, payloadJsonString|null, _, _, errInfo?, reqId]`:
- `[1]` is the rpcid, `[2]` is the result as a **JSON string** (parse it a second time), and `[6]` is the reqId.
- If `[2]` is null or empty, the call failed (gpwc sets `success=False`). In practice index `[5]` often
  carries an error status array, e.g. `[3]` **[UNCERTAIN, from general batchexecute
  knowledge, not in these sources]**.
- Entries whose `[0]` is `"er"` are errors (mcp treats "all `er`" as a failure).

Both GPTK and gpwc skip the length prefixes and keep only the lines that contain `wrb.fr`:

```ts
const jsonLines = responseBody.split('\n').filter((line) => line.includes('wrb.fr'));
const parsedData = JSON.parse(jsonLines[0]);
return JSON.parse(parsedData[0][2]);
```

A Rust parser should be more careful. Strip the first line, then read `<len>\n<json>` chunks; the
length counts characters, and some implementations treat it as UTF-16 code units
**[UNCERTAIN]**. Alternatively, drop `rt` and the server returns a single JSON array of all
envelopes after the `)]}'` prefix (kovatch, 2021 edit). Match responses to requests by `[6]`, not by order.

### 1.5 Data conventions
- Timestamps are epoch **ms**. Timezone offsets are ms east of UTC (e.g. `-18000000` = -05:00).
- Coordinates are E7 integers (`[407696000, -739964000]` = 40.7696, -73.9964).
- `mediaKey` (`AF1Qip...`) identifies an item or album. `dedupKey` is a different id that the
  mutation RPCs take; you need neither for read-only backup, but store both.
- Sparse arrays: long records may end in an object keyed by **1-based** field number
  (e.g. `{"11": "", "13": "<url>"}` stands in for indices 10 and 12). The trailing extension object
  holds numeric-string keys such as `"76647426"`. A tolerant reader looks for index *i* positionally
  and otherwise in the trailing object under `str(i+1)` (mcp PAYLOADS.md "Encoding quirks").
- Extension keys in the trailing `{}` of an item:
  - `76647426`: video. `[0]` is the duration in ms. Its presence means is-video.
  - `146008172`: Live/motion photo. `[1]` is the duration; `[3]` is the video URL, in VrseUb only.
  - `163238866`: `[0]` is the favourite flag.
  - `129168200`: location. `[1][0]` is the coordinates; `[1][4][0][1][0][0]` is the place name.
  - `396644657`: `[0]` is the short description.
  - `318563170`: `[0]` = `[takesUpSpace(1), bytesUsed, quality(2=original), ...]`.
  - `225032867`: `[0]` is the trash timestamp.

## 2. RPCs

Payloads below are the inner `requestData` (before `JSON.stringify`). `data` is the parsed
result.

### 2.1 `lcxiM`: library timeline (by taken date, newest first)
GPTK `getItemsByTakenDate`:
```ts
const requestData = [pageId, timestamp, pageSize, null, 1, sourceCode]; // pageSize default 500
// sourceCode: 1 = library, 2 = archive, 3 = both
```
gpwc `GetLibraryPageByTakenDate` is identical (default source `both`, page_size 500).
mcp says it asks for 100 per page and that the archive and library "use fixed Google pages of 100"
**[UNCERTAIN whether >100 is honoured]**.

Response (`parser.ts` `libraryTimelinePage`): `data[0]` items, `data[1]` nextPageId (string or
null), `data[2]` lastItemTimestamp (a string; GPTK `parseInt`s it).

Pagination: GPTK `gptk-core.ts` (full-library path) passes **both** the token and a timestamp
cursor:
```ts
const mediaPage = await this.api.getItemsByTakenDate(nextPageTimestamp, source ?? null, nextPageId);
nextPageId = mediaPage?.nextPageId ?? null;
nextPageTimestamp = mediaPage.lastItemTimestamp - 1;
} while (nextPageId);
```
gpwc and nbarari pass `timestamp = null` and only the page token. Either way, stop when
`data[1]` is null or empty. Use source `3` (or run `1` and `2`) so archived items are included.
nbarari: "Items in the Google Photos archive won't appear in library enumeration (mode 1)."

Item fields (`libraryItemParse`, also used for `EzkLib`):

| field | index |
|---|---|
| mediaKey | `[0]` |
| thumb / base URL | `[1][0]` (`https://photos.fife.usercontent.google.com/pw/...`) |
| width, height | `[1][1]`, `[1][2]` (original pixel size) |
| timestamp (taken) | `[2]` |
| dedupKey | `[3]` |
| timezoneOffset | `[4]` |
| creationTimestamp (uploaded) | `[5]` |
| isPartialUpload | `[12]?.[0] === 20` |
| isArchived | `[13]` (absent in short live `lcxiM` records; infer it from the source) |
| isOwned | `[7]` has no sub-array containing `27` |
| isVideo / duration | ext `76647426` present / `[0]` |
| isLivePhoto | ext `146008172` present |
| isFavorite, geo, descriptionShort | ext `163238866[0]`, `129168200`, `396644657[0]` |

Example record (fixture `lcxiM.json`, anonymised):
```json
["AF1QipUT...", ["https://photos.fife.usercontent.google.com/pw/ANON_2",1080,1920,null,null,null,null,null,[null,null,1],[8877406]],
 1761753048000, "sK5VhIg2...", 3600000, 1761755803160, null, [[1],[2],[3],...], 2, null,null,null,null,false,53246,
 {"76647426":[22932,null,1080,1920,...], "129168200":[[],[null,null,null,false]], "163238866":[false]}]
```

Alternative: `EzkLib` with `['', [[4, 'ra', 0, 0]], pageId]` lists by **upload date**
(`getItemsByUploadedDate`). The response is `data[0]` items and `data[1]` nextPageId. This is useful for
incremental sync, because new uploads appear first even when their taken dates are old.

### 2.2 `Z5xsfc`: list albums
```ts
const requestData = [pageId, null, null, null, 1, null, null, pageSize, [2], 5]; // pageSize 100
```
The response is `data[0]` albums and `data[1]` nextPageId. Album fields (`albumParse`), where `E = item.at(-1)["72930366"]`:

| field | path |
|---|---|
| mediaKey | `[0]` |
| cover thumb | `[1][0]` |
| ownerActorId | `[6][0]` |
| title | `E[1]` |
| itemCount | `E[3]` |
| creation / modified ts | `E[2][4]` / `E[2][9]` |
| timestampRange | `[E[2][5], E[2][6]]` |
| isShared | `E[4]` |
| authKey | `E[5]` (pass it to `snAcKc` for shared albums) |

Shared links / shared albums can be listed with `F2A0H` `[pageId, null, 2, null, 3]` (`data[0]`
items: `[6]` mediaKey, `[17]` linkId, `[3]` itemCount; `data[1]` next). **[UNCERTAIN]** whether
`Z5xsfc` includes albums that others have shared with you.

### 2.3 `snAcKc`: items in an album
```ts
const requestData = [albumMediaKey, pageId, null, authKey]; // authKey null for own albums
```
Response (`albumItemsPage`): `data[1]` items, `data[2]` nextPageId (`""` on the last page),
`data[3]` album metadata: `[3][0]` key, `[3][1]` title, `[3][2][5..9]` timestamps,
`[3][5]` owner actor, `[3][9]` members, `[3][19]` authKey, `[3][21]` itemCount. Item fields as
in 2.1 (`albumItemParse`: indices 0-5 plus ext keys). The trailing ext object sits at index 9 instead of 15,
and the page size is chosen by the server. Album item order is the album's order.

### 2.4 Per-item metadata

**`fDcn4b`: extended info** (GPTK: "Only works with mediaKey belonging to your library")
```ts
const requestData = [mediaKey, 1, authKey, null, 1];
```
`itemInfoExtParse`, with `d = data[0]`:

| field | path |
|---|---|
| mediaKey / dedupKey | `d[0]` / `d[11]` |
| description (full) | `d[1]` |
| **fileName** | `d[2]` |
| timestamp / tz | `d[3]` / `d[4]` |
| **size (bytes)** | `d[5]` |
| width / height | `d[6]` / `d[7]` |
| location coords | `d[9][0]` ?? `d[13][0]` (E7 pair) |
| location name | `d[13][2][0][1][0][0]` |
| albums containing item | `d[19]` (album records, as in 2.2) |
| cameraInfo | `d[23]` (layout undocumented; null in the video fixture) **[UNCERTAIN]** |
| source | `d[27][0]`: 1 mobile, 2 web, 3 shared, 4 partnerShared, 7 drive, 8 pc, 11 gmail; `d[27][1][2]`: 1 android, 3 ios |
| owner | `d[27][3][0]` ?? `d[27][4][0]` ?? `d[28]` (actor: `[0]` actorId, `[1]` gaiaId, `[11][0]` name) |
| takesUpSpace / bytes / original quality | `d[30][0]==1` / `d[30][1]` / `d[30][2]==2` |
| map thumb | `data[1]` |

**`VrseUb`: item info** (also works for shared items with `authKey` and `albumMediaKey`)
```ts
const requestData = [mediaKey, null, authKey, null, albumMediaKey];
```
`itemInfoParse`: `data[0]` is a list record (indices as in 2.1, plus `[0][1][8][4]` cameraInfo
fallback). Then:
- `data[1]`: **downloadUrl**
- `data[7]`: **downloadOriginalUrl**
- `data[10]`: full description
- `data[12]`: thumb URL

The ext object is found by key, not by position (index 15 for library items, 9 for album items);
`ext["146008172"][3]` is the **Live-photo video download URL** (gpwc reads `[0][15]["146008172"][3]`).
The 5-element fixture `VrseUb.json` (a video) has only `[1]`. `[7]` was absent, so mcp falls back to `[1]`.

**`EWgK9e`: bulk info** (filename and size for many keys in one call; GPTK sends up to 5000)
```ts
const requestData = [[[mediaKeyArray.map(id => [id])], [[null x24, [], null x10, []]]]];
// exact literal in api.ts: 24 nulls, [], 10 nulls, []
```
The result is `data[0][1]`, one row per item: `[0]` mediaKey, `[1][2]` description, `[1][3]` **fileName**,
`[1][6]` timestamp, `[1][7]` tz, `[1][8]` creationTimestamp, `[1][9]` **size**, `[1].at(-1)` =
`[takesUpSpace, bytesUsed, quality]`. This RPC has no location or camera fields, so use `fDcn4b` for those.

Suggested backup flow: page `lcxiM` (source 3) → `EWgK9e` per page for filenames and sizes →
`fDcn4b` per item for location, camera and albums (optional) → `VrseUb` per item for download URLs →
`Z5xsfc` + `snAcKc` for album membership.

## 3. Downloading original bytes

### 3.1 `VrseUb` download URLs (recommended starting point)
`data[7]` (`downloadOriginalUrl`) and `data[1]` (`downloadUrl`) are plain HTTPS URLs. Fetch them
with `GET` and the same cookie jar, following redirects. mcp only follows redirects to
`photos.google.com`, `photos.fife.usercontent.google.com` and `lh[0-9].googleusercontent.com`.
The precise difference between `[1]` and `[7]` is **[UNCERTAIN]**. The names suggest that `[7]` is the
unmodified upload, while `[1]` is the "Download" button result, which for edited items is the edited
version. Take the filename from `fDcn4b d[2]` or `EWgK9e [1][3]`, and check the length against `size`.

### 3.2 `pLFTfd`: signed download URL. **Verified 2026-11-01 for videos**
VrseUb returns neither `[1]` nor `[7]` for videos. `pLFTfd` with `[[mediaKey],[1]]` (also tested: `[[k],null,null]`
and `[[k],null,null,1]`) returns `["https://video-downloads.googleusercontent.com/..."]`. Fetching that URL with the
cookies gives 200 `video/mp4`, `Content-Disposition: attachment; filename="<original name>"`, and a byte count
equal to ext `318563170[0][1]`. Without cookies, the response is 200 with an empty body.

Earlier notes, from before verification:
| source | payload | claim |
|---|---|---|
| GPTK `getDownloadUrl` | `[mediaKeyArray, null, authKey]`, returns `response[0]` | none |
| gpwc `GetUnsupportedVideoDownloadUrl` | `[media_keys, null, null, 1]` | for "unsupported videos" (listed by `TLvKMb [page_token]`) |
| nbarari `download-test.mjs` | `[[mediaKey], [1]]` | `// pLFTfd request format: [[mediaKey], [1]] requests original quality download` |

nbarari takes the first `https://` string in the result, then fetches it **outside** the page with
cookies for `photos.google.com` and `video-downloads.googleusercontent.com` ("In-page fetch is
CORS-blocked"). It says it preserves EXIF and uses this for re-upload, so the bytes should be
originals. Its downloads ran with a 500 ms delay between items.

### 3.3 `yCLA7` + `dnv2s`: zip export
`yCLA7 [[[k1],[k2],...]]` returns a token at `response[0]`. Poll `dnv2s [[token]]` until it is ready
(fixture: `[[[[uuid, 4, [["file_0.zip", "https://doc-export.googleusercontent.com/download/...",
8295635, 8293971]], 0], 100, 2]]]`. `[0][0][0][1]==4` and the `100` look like status and percent
**[UNCERTAIN]**). Parsed: `[0][0][0][2][0]` = `[fileName, downloadUrl, zipSize, unzippedSize]`.
This is the web UI's multi-select download path and returns originals in a ZIP. It is useful for
batching, but you have to unzip, and filenames may collide.

### 3.4 `=d` / `=dv` suffixes on base URLs
These come from the **official** Library/Picker APIs, where `baseUrl=d` gives a photo with "all the
Exif metadata except the location metadata", and `baseUrl=dv` gives a **transcoded** video rather than
the original (Google developer docs; StackOverflow 54782846). On the web thumbnail URLs (`[1][0]`),
GPTK only uses sizing suffixes (`parser.ts`: "Add =w417-h174-k-no?authuser=0 to thumbnail URL to
set custom size, remove 'video' watermark, remove auth requirement"). mcp states: "Original bytes
via the thumbnail URL (`=d`): not used by either project". Do **not** rely on `=d`/`=dv` for
originals. You can use them as a last-resort fallback (expect stripped GPS and transcoded video).

### 3.5 Caveats
- **Live photos (iOS)**: the still image and the `.MOV` are separate. Get the video from VrseUb ext
  `146008172[3]`; `[1]` is the duration. Save both, with the same stem.
- **Android motion photos**: the video is normally embedded in the JPEG/HEIC (MicroVideo/XMP). The
  original-download URL should keep it; a `=d` or thumbnail fetch will not **[UNCERTAIN, untested]**.
- **Edits**: the original and the edited version may differ; see 3.1.
- **Storage-saver items** (`isOriginalQuality` false) only exist in recompressed form, so their "original" is
  the compressed file.
- **Partial uploads** (`[12][0]==20`) and items still processing may lack download URLs.
- The download URLs are short-lived, so fetch them soon after the RPC.

## 4. Rate limits and errors

No source documents Google's limits. What the sources do:

| source | behaviour |
|---|---|
| GPTK `makeApiRequest` | 3 attempts, delay `2000 ms * attempt`; retries on non-2xx, empty body, missing `wrb.fr`, or empty `[0][2]`. The comment says these happen with "rate-limiting, timeouts". Concurrency 3 (`maxConcurrentSingleApiReq/BatchApiReq: 3`), batch sizes `operationSize: 250`, `infoSize: 5000` |
| gpwc `utils.py` | `Retry(total=5, backoff_factor=1, status_forcelist=[429, 502, 503, 504])` |
| mcp | serial RPCs ≥350 ms apart + jitter; images ≤4 concurrent, starts ≥100 ms apart; ≤2 retries on 5xx/network only; daily budgets of 3000 RPC / 2000 thumbnails (self-imposed) |
| nbarari | 500 ms between downloads |

Error signals (mcp README "Errors" and `transport.py`):
- **429**, a redirect to `google.com/sorry`, or a body that matches
  `g-recaptcha|/sorry/index|unusual traffic from your computer` means you are **blocked**. mcp's breaker
  stops all calls for 30 min, then 2 h, then 8 h.
- A redirect to `accounts.google.com` or `google.com/photos/about` means the session has expired
  (re-export the cookies). A bare 401 means the session has probably expired.
- A redirect to `consent.google.com` means a consent screen is required.
- **400/403** from batchexecute means the `at`/`f.sid`/`bl` values are stale. Fetch the globals again and retry once.
- 403/404 on a single media URL means that item is unavailable.
- 200 with HTML instead of `)]}'` is an upstream error or captcha. 200 with no `wrb.fr`, only `er` entries,
  or `[2]` null means the RPC failed or the schema has changed.
- Treat 5xx as retryable with backoff.

Suggested defaults for gphotosync: one RPC in flight, ≥300-500 ms apart with jitter; 2-4 parallel
downloads; exponential backoff on 5xx; hard stop and persist state on 429 or captcha.
