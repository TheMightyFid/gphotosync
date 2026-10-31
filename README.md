# gphotosync

Resumable Google Photos backup. It downloads original-quality photos and videos (including Live-photo
videos), album membership and metadata from photos.google.com. It uses your browser session cookies,
because the official Library API stopped exposing the full library in March 2025. It is built to run from
cron: each run picks up where the last one stopped.

## Setup

1. Open a **private/incognito** window, sign in at https://photos.google.com, and export its cookies in
   Netscape `cookies.txt` format (e.g. with the "Get cookies.txt LOCALLY" extension). Then close the
   window *without signing out*. If you keep using that session in a browser, Google rotates the
   cookies and the exported copy goes stale.
2. Build the image (Linux; on macOS this needs a Linux builder, see below):
   ```sh
   nix build .#docker && docker load < result
   ```
3. Run:
   ```sh
   docker run --rm -v /srv/photos:/data -v /srv/gphotosync:/config:ro gphotosync:latest
   ```
   Cron, e.g. nightly:
   ```cron
   17 3 * * * docker run --rm -v /srv/photos:/data -v /srv/gphotosync:/config:ro gphotosync:latest >> /var/log/gphotosync.log 2>&1
   ```

Without Docker: `nix run . ` or `cargo run --release`.

## Configuration (env)

| var | default | |
|---|---|---|
| `GPS_DATA` | `./backup` (`/data` in the image) | output + state directory |
| `GPS_COOKIES` | `./cookies.txt` (`/config/cookies.txt`) | exported cookies |
| `GPS_ACCOUNT` | `0` | `/u/N/` account index when several are signed in |
| `GPS_DELAY_MS` | `500` | minimum gap between RPCs |
| `GPS_ALBUMS` | `1` | set to `0` to skip album indexing |

## Output

```
/data/media/YYYY/MM/<original filename>        original bytes
/data/media/YYYY/MM/<file>.json                Google's raw metadata (listing + extended info)
/data/media/YYYY/MM/<stem>.live.mov            Live-photo video, when present
/data/albums/<title> [<id>]/<file>             symlinks into media/
/data/state.db                                 SQLite resume state
/data/cookies.txt                              rotated session cookies (preferred while newer than the input)
```

## Resuming and exit codes

All progress lives in `state.db`. Library paging checkpoints its page token, and each item is marked
done only after its file has been fully written (`.part` and rename). Killing the process at any point
is safe. A `.lock` file stops overlapping cron runs.

| code | meaning |
|---|---|
| 0 | done (or another run already in progress) |
| 75 | rate limited / captcha. Stopped early; the next run resumes |
| 77 | session expired. Re-export the cookies |
| 1 | other error |

Items that fail are retried on later runs, after the ones that have never been tried. `sqlite3 state.db
"select key,attempts,last_error from items where path is null and attempts>0"` lists them.

## Caveats

This relies on undocumented internal endpoints (see `docs/rpc-spec.md`), so a Google UI change can break it.
Storage-saver items only exist in their compressed form.

## macOS: building the image

`nix build .#docker` needs an `aarch64-linux` builder. With nix-darwin, set `nix.linux-builder.enable = true;`.
