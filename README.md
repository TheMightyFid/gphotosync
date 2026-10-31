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
2. Build the image (works on Linux and macOS, see "Images" below):
   ```sh
   nix build .#docker && docker load < result
   ```
3. Run:
   ```sh
   docker run --rm -v /srv/photos:/data -v /srv/gphotosync:/config:ro gphotosync:latest
   ```
   The image runs as uid/gid 1000:1000, so `/srv/photos` must be writable by it (or pass `--user`).
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

## Progress

Each run logs to stderr (so to `docker logs` or your cron log) with UTC timestamps. It logs the starting
state, a running total for each page of the library listing, `album i/N`, a line for each download with the
rate and ETA, and a summary that is printed however the run ends:

```
2025-10-29 03:17:02Z state: 1234/5678 backed up (21.7%), 12 failing
2025-10-29 03:17:02Z == indexing library
2025-10-29 03:17:04Z indexed page 3: 1500 items seen this run, 2 new
2025-10-29 03:18:10Z album 7/40 indexed: "Trip"
2025-10-29 03:18:40Z == downloading
2025-10-29 03:19:01Z [1235/5678 21.7%] saved media/2024/05/IMG_1.jpg (3.2 MB) - 41 items/h, ETA 3h12m this run
2025-10-29 05:02:13Z == run summary: rate limited, resume later
2025-10-29 05:02:13Z    this run: 71 downloaded (412.3 MB), 1 failed, elapsed 1h45m
2025-10-29 05:02:13Z    library: 1305/5678 backed up (23.0%)
```

`gphotosync status` prints the state without contacting Google. It also works while a sync is running:

```sh
GPS_DATA=/srv/photos gphotosync status
docker run --rm -v /srv/photos:/data gphotosync:latest status
```
```
backed up: 1305/5678 (23.0%), 4373 pending
albums:    40
failing:   12 (retried next run)
  AF1Qip... (3x): HTTP 403 Forbidden downloading https://...
last run:  2025-10-29 03:17:02Z -> 2025-10-29 05:02:13Z: rate limited, resume later
           71 downloaded (412.3 MB), 1 failed
```

On Unraid, either open the container's log from the Docker tab or run
`docker run --rm -v /mnt/user/photos/gphotosync:/data gphotosync:latest status`.

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

## Images

`nix build .#docker` builds an image for the host architecture (aarch64 when built on macOS, via cross-compilation).
`nix build .#docker-amd64` always builds an x86_64 image, which is what Unraid and most NAS boxes need.

## Unraid

1. Build and copy the image across: `nix build .#docker-amd64 && scp result root@tower:/tmp/gphotosync.tar.gz`.
   Then, on Unraid, run `docker load < /tmp/gphotosync.tar.gz`.
2. `cp unraid/gphotosync.xml /boot/config/plugins/dockerMan/templates-user/my-gphotosync.xml`.
3. Put `cookies.txt` in `/mnt/user/appdata/gphotosync/`. In **Docker → Add Container**, pick the
   `gphotosync` template, check the paths and click Apply. The container runs one sync and exits. The
   template runs it as `nobody:users` (99:100), so the backup share must be writable by that user.
4. Schedule it with the **User Scripts** plugin (custom cron, e.g. `17 3 * * *`):
   ```sh
   #!/bin/bash
   docker start -a gphotosync
   ```
