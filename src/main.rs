//! gphotosync: resumable Google Photos backup. Safe to kill at any point and re-run from cron.

/// eprintln! with a UTC timestamp, so cron and docker logs show when things happened.
macro_rules! log {
    ($($a:tt)*) => { eprintln!("{} {}", crate::stamp(crate::now_ms()), format_args!($($a)*)) };
}

mod db;
mod gphotos;
mod immich;

use anyhow::{Context, Result, bail};
use db::Db;
use futures_util::StreamExt;
use gphotos::{Client, Fatal, Jar};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::AsyncWriteExt;

fn env(k: &str, default: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() {
    let res = if std::env::args().nth(1).as_deref() == Some("status") {
        status()
    } else {
        run().await
    };
    match res {
        Ok(()) => {}
        Err(e) => {
            log!("error: {e:#}");
            // 75 = EX_TEMPFAIL: blocked, try again later. 77 = EX_NOPERM: cookies need re-exporting.
            let code = match e.downcast_ref::<Fatal>() {
                Some(Fatal::Blocked(_)) => 75,
                Some(Fatal::SignedOut(_)) => 77,
                None => 1,
            };
            std::process::exit(code);
        }
    }
}

async fn run() -> Result<()> {
    let data = PathBuf::from(env("GPS_DATA", "./backup"));
    let cookies = PathBuf::from(env("GPS_COOKIES", "./cookies.txt"));
    let account: u32 = env("GPS_ACCOUNT", "0").parse().context("GPS_ACCOUNT")?;
    let delay = Duration::from_millis(env("GPS_DELAY_MS", "500").parse().context("GPS_DELAY_MS")?);
    std::fs::create_dir_all(data.join("media"))?;

    // Overlapping cron runs would double-download; the OS drops this lock if we're killed.
    let lock = std::fs::File::create(data.join(".lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            log!(
                "another run holds {}, exiting",
                data.join(".lock").display()
            );
            return Ok(());
        }
        Err(e) => return Err(e).context("locking .lock"),
    }

    // Google rotates session cookies; we save the rotated jar and prefer it while it's newer than the input.
    let saved = data.join("cookies.txt");
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let src = if mtime(&saved) > mtime(&cookies) {
        &saved
    } else {
        &cookies
    };
    let jar = Jar::parse(
        &std::fs::read_to_string(src).with_context(|| format!("reading {}", src.display()))?,
    );

    let mut db = Db::open(&data.join("state.db"))?;
    // After a block, back off across runs: hammering again at the next cron tick escalates Google's response.
    if let Some(until) = db.get("blocked_until")?.and_then(|v| v.parse::<i64>().ok())
        && until > now_ms()
    {
        log!(
            "cooling down after a block until {}, skipping this run",
            stamp(until)
        );
        return Ok(());
    }
    let (total, done) = db.counts()?;
    log!(
        "state: {done}/{total} backed up ({:.1}%), {} failing",
        pct(done, total),
        db.failing()?
    );
    db.set("run_start", Some(&now_ms().to_string()))?;
    db.set("run_end", None)?;
    db.set("run_outcome", Some("running"))?;
    let t0 = Instant::now();
    let mut st = Stats::default();
    let mut signal = None;
    let res = match Client::new(jar, account, delay).await {
        Err(e) => Err(e),
        Ok(mut gp) => {
            // docker stop / cron timeout send SIGTERM: stop syncing but still save the rotated cookies below.
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            let res = tokio::select! {
                r = sync(&mut gp, &mut db, &data, &mut st) => r,
                _ = term.recv() => { signal = Some("terminated"); Err(anyhow::anyhow!("terminated, will resume next run")) }
                _ = tokio::signal::ctrl_c() => { signal = Some("interrupted"); Err(anyhow::anyhow!("interrupted, will resume next run")) }
            };
            let saved_jar = save_jar(&gp, &saved);
            res.and(saved_jar)
        }
    };
    // Escalating cooldown: 1h, 4h, 24h, then 72h for repeat blocks. Only a clean run resets the strikes:
    // a sign-out, network error or SIGTERM proves nothing about whether Google still has us flagged.
    let blocked = res
        .as_ref()
        .err()
        .and_then(|e| e.downcast_ref::<Fatal>())
        .is_some_and(|f| matches!(f, Fatal::Blocked(_)));
    let prev: usize = db
        .get("block_strikes")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let strikes = if blocked {
        prev + 1
    } else if res.is_ok() {
        0
    } else {
        prev
    };
    db.set("block_strikes", Some(&strikes.to_string()))?;
    let hours = [0, 1, 4, 24, 72][strikes.min(4)];
    db.set(
        "blocked_until",
        blocked
            .then(|| (now_ms() + hours * 3_600_000).to_string())
            .as_deref(),
    )?;
    if blocked {
        log!(
            "blocked (strike {strikes}): next run allowed after {}",
            stamp(now_ms() + hours * 3_600_000)
        );
    }
    let outcome = match &res {
        Ok(()) => "finished".to_string(),
        Err(e) => match (e.downcast_ref::<Fatal>(), signal) {
            (Some(Fatal::Blocked(_)), _) => "rate limited, resume later".to_string(),
            (Some(Fatal::SignedOut(_)), _) => "session expired, re-export cookies".to_string(),
            (None, Some(s)) => format!("{s}, resume later"),
            (None, None) => format!("error: {e:#}"),
        },
    };
    let (total, done) = db.counts()?;
    let elapsed = fmt_dur(t0.elapsed().as_secs());
    log!("== run summary: {outcome}");
    log!(
        "   this run: {} downloaded ({}), {} failed, elapsed {elapsed}",
        st.items,
        fmt_bytes(st.bytes),
        st.failed
    );
    log!(
        "   library: {done}/{total} backed up ({:.1}%)",
        pct(done, total)
    );
    for (k, v) in [
        ("run_end", now_ms().to_string()),
        ("run_outcome", outcome),
        ("run_items", st.items.to_string()),
        ("run_bytes", st.bytes.to_string()),
        ("run_failed", st.failed.to_string()),
    ] {
        db.set(k, Some(&v))?;
    }
    res
}

/// Write-then-rename: a kill mid-write must not leave a truncated jar that's newer than the input.
/// 0600: these cookies are full access to the Google account.
fn save_jar(gp: &Client, saved: &Path) -> Result<()> {
    let tmp = saved.with_extension("txt.tmp");
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?
        .write_all(gp.jar.dump().as_bytes())?;
    std::fs::rename(&tmp, saved)?;
    Ok(())
}

/// Counters for this run, for the progress lines and the end-of-run summary.
#[derive(Default)]
struct Stats {
    items: u64,
    bytes: u64,
    failed: u64,
}

async fn sync(gp: &mut Client, db: &mut Db, data: &Path, st: &mut Stats) -> Result<()> {
    log!("== indexing library");
    index_library(gp, db).await?;
    if env("GPS_ALBUMS", "1") == "1" {
        log!("== indexing albums");
        // One broken (e.g. shared) album must not block every backup run.
        match index_albums(gp, db).await {
            Err(e) if !e.is::<Fatal>() => log!("album indexing failed, continuing: {e:#}"),
            r => r?,
        }
    }
    // Link even when downloads stop early (block, or a tail of permanently failing items).
    log!("== downloading");
    let res = download(gp, db, data, st).await;
    link_albums(db, data)?;
    let url = env("GPS_IMMICH_URL", "");
    if !url.is_empty() {
        log!("== syncing albums to Immich");
        let key = std::env::var("GPS_IMMICH_API_KEY")
            .context("GPS_IMMICH_URL is set but GPS_IMMICH_API_KEY isn't")?;
        let im = immich::Immich::new(&url, &key, &env("GPS_IMMICH_PATH", "/gphotosync/media"))?;
        // Immich being down must never fail the backup itself.
        if let Err(e) = im.sync_albums(db).await {
            log!("immich album sync failed, retried next run: {e:#}");
        }
    }
    res
}

/// Page the whole library into the DB, checkpointing the page token so a stopped run resumes mid-listing.
// ponytail: full listing every run (500 items/RPC, so 50k items is ~100 calls). This also catches
// old-dated uploads that a "stop at first known item" incremental scan would miss.
async fn index_library(gp: &mut Client, db: &mut Db) -> Result<()> {
    let mut token = db.get("lib_token")?;
    let (mut pages, mut seen, mut new_total) = (0, 0, 0);
    loop {
        let (items, next) = match gp.library_page(token.as_deref()).await {
            Err(e) if token.is_some() && !e.is::<Fatal>() => {
                // ponytail: assumes a failing saved token has expired; restarting is safe, upserts are idempotent.
                log!("saved page token rejected ({e:#}), restarting listing from the top");
                token = None;
                continue;
            }
            r => r?,
        };
        let new = db.upsert_items(&items)?;
        (pages, seen, new_total) = (pages + 1, seen + items.len(), new_total + new);
        log!("indexed page {pages}: {seen} items seen this run, {new_total} new");
        db.set("lib_token", next.as_deref())?;
        if next.is_none() {
            return Ok(());
        }
        token = next;
    }
}

/// Re-reads every album each run: album edits don't show up in the library listing.
async fn index_albums(gp: &mut Client, db: &mut Db) -> Result<()> {
    let mut token = None;
    let (mut i, mut n) = (0, 0);
    loop {
        let (albums, next) = gp.albums_page(token.as_deref()).await?;
        // The album count is only known once the last page of albums is in; until then show "N+".
        n += albums.len();
        let more = if next.is_some() { "+" } else { "" };
        for (key, title, auth, raw) in albums {
            i += 1;
            db.upsert_album(&key, &title, &raw.to_string())?;
            let mut t = None;
            loop {
                let (items, n) = gp.album_page(&key, auth.as_deref(), t.as_deref()).await?;
                db.add_album_items(&key, &items)?;
                if n.is_none() {
                    break;
                }
                t = n;
            }
            log!("album {i}/{n}{more} indexed: {title:?}");
        }
        if next.is_none() {
            return Ok(());
        }
        token = next;
    }
}

async fn download(gp: &mut Client, db: &mut Db, data: &Path, st: &mut Stats) -> Result<()> {
    let (total, mut done) = db.counts()?;
    let t0 = Instant::now();
    // Items that failed this run; they stay pending, so over-fetch by that many to always see untried ones.
    let mut tried = HashSet::new();
    let mut consecutive_failures = 0;
    // Spread a big first sync over several runs instead of one 30h burst (docs/bot-risk.md). 0 = unlimited.
    let max_items: u64 = env("GPS_MAX_ITEMS", "2000")
        .parse()
        .context("GPS_MAX_ITEMS")?;
    let mut next_pause = gphotos::jitter(50.0, 200.0) as u64;
    loop {
        let batch: Vec<_> = db
            .pending(200 + tried.len())?
            .into_iter()
            .filter(|i| !tried.contains(&i.key))
            .collect();
        if batch.is_empty() {
            return Ok(());
        }
        for item in batch {
            if max_items > 0 && st.items >= max_items {
                log!("reached GPS_MAX_ITEMS={max_items} for this run; the rest resumes next run");
                return Ok(());
            }
            if st.items >= next_pause {
                let secs = gphotos::jitter(30.0, 180.0) as u64;
                log!("pausing {secs}s");
                tokio::time::sleep(Duration::from_secs(secs)).await;
                next_pause = st.items + gphotos::jitter(50.0, 200.0) as u64;
            }
            match download_one(gp, &item, data).await {
                Ok((path, size)) => {
                    consecutive_failures = 0;
                    db.mark_done(&item.key, &path)?;
                    (done, st.items, st.bytes) = (done + 1, st.items + 1, st.bytes + size);
                    let (rate, eta) = rate_eta(
                        st.items,
                        (total - done).max(0) as u64,
                        t0.elapsed().as_secs(),
                    );
                    log!(
                        "[{done}/{total} {:.1}%] saved {path} ({}) - {rate} items/h, ETA {eta} this run",
                        pct(done, total),
                        fmt_bytes(size)
                    );
                }
                Err(e) if e.is::<Fatal>() => return Err(e),
                Err(e) => {
                    st.failed += 1;
                    log!("failed {}: {e:#}", item.key);
                    tried.insert(item.key.clone());
                    db.mark_failed(&item.key, &format!("{e:#}"))?;
                    consecutive_failures += 1;
                    if consecutive_failures >= 20 {
                        bail!("20 downloads failed in a row, giving up until next run");
                    }
                }
            }
        }
    }
}

/// Download one item to media/YYYY/MM/<original filename>, with a JSON sidecar.
/// Returns the path relative to `data` and the bytes fetched (including any Live-photo video).
// ponytail: one download at a time; it's gentle on rate limits. Add a JoinSet if bandwidth goes underused.
async fn download_one(gp: &mut Client, item: &db::Item, data: &Path) -> Result<(String, u64)> {
    let (url, live, full) = gp.item_urls(&item.key).await?;
    let d = &full[0];
    let name = d[2]
        .as_str()
        .map(sanitize)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            format!(
                "{}.{}",
                sanitize(&item.key),
                if item.is_video { "mp4" } else { "jpg" }
            )
        });
    let (y, m) = year_month(item.ts_ms);
    let dir = format!("media/{y:04}/{m:02}");
    std::fs::create_dir_all(data.join(&dir))?;
    let rel = unique(data, &dir, &name, &item.key);
    let mut sidecar = json!({ "media_key": item.key, "timestamp_ms": item.ts_ms, "listing": raw(&item.meta), "info": full });
    if live.is_some() {
        let stem = rel.rsplit_once('.').map_or(rel.as_str(), |(s, _)| s);
        sidecar["live_video"] = json!(format!("{stem}.live.mov"));
    }
    // Sidecar first: it claims `rel` for this key, so a run killed mid-download reuses the same name.
    std::fs::write(
        data.join(format!("{rel}.json")),
        serde_json::to_vec_pretty(&sidecar)?,
    )?;
    let mut size = fetch_to(gp, &url, &data.join(&rel)).await?;
    if let Some(expect) = d[5].as_u64()
        && expect != size
    {
        // Logged, not fatal: the "original" download of edited items can legitimately differ in size.
        log!("note: {rel} is {size} bytes, Google reports {expect}");
    }
    if let (Some(v), Some(vrel)) = (live, sidecar["live_video"].as_str()) {
        size += fetch_to(gp, &v, &data.join(vrel)).await?;
    }
    Ok((rel, size))
}

fn raw(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or(Value::Null)
}

/// Stream to `<path>.part` then rename, so a killed run never leaves a truncated file that looks complete.
async fn fetch_to(gp: &mut Client, url: &str, path: &Path) -> Result<u64> {
    let resp = gp.fetch(url, None).await?;
    if !resp.status().is_success() {
        bail!("HTTP {} downloading {url}", resp.status());
    }
    // An expired URL or interstitial can come back as a 200 HTML page (even via photos.google.com).
    if resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"text/html"))
    {
        let url = resp.url().to_string();
        // Only sniff the start: a mislabelled multi-GB video must not be buffered in memory.
        let (mut head, mut body) = (Vec::new(), resp.bytes_stream());
        while head.len() < 1 << 20
            && let Some(Ok(chunk)) = body.next().await
        {
            head.extend_from_slice(&chunk);
        }
        if gphotos::is_captcha(&String::from_utf8_lossy(&head)) {
            Err(Fatal::Blocked(format!(
                "captcha instead of media from {url}"
            )))?
        }
        bail!("got HTML instead of media from {url}");
    }
    let part = PathBuf::from(format!("{}.part", path.display()));
    let mut f = tokio::fs::File::create(&part).await?;
    let mut n = 0u64;
    let mut body = resp.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        n += chunk.len() as u64;
        f.write_all(&chunk).await?;
    }
    f.sync_all().await?;
    tokio::fs::rename(&part, path).await?;
    Ok(n)
}

/// albums/<title>/<file> symlinks into media/, rebuilt from scratch each run so renames/removals don't leave stale links.
fn link_albums(db: &Db, data: &Path) -> Result<()> {
    let albums = data.join("albums");
    if albums.exists() {
        std::fs::remove_dir_all(&albums)?;
    }
    for (key, title, path) in db.album_links()? {
        let dir = albums.join(format!(
            "{} [{}]",
            sanitize(&title),
            &key[key.len().saturating_sub(6)..]
        ));
        std::fs::create_dir_all(&dir)?;
        let file = Path::new(&path)
            .file_name()
            .context("bad path")?
            .to_string_lossy();
        let mut link = dir.join(&*file);
        if link.symlink_metadata().is_ok() {
            // Same filename from different months: disambiguate with the month dir.
            link = dir.join(format!(
                "{}_{file}",
                path.split('/')
                    .skip(1)
                    .take(2)
                    .collect::<Vec<_>>()
                    .join("-")
            ));
        }
        if link.symlink_metadata().is_err() {
            std::os::unix::fs::symlink(Path::new("../..").join(&path), &link)?;
        }
    }
    Ok(())
}

/// Safe single path component, capped well under the 255-byte filename limit (room for suffixes and `.json`).
fn sanitize(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| {
            if c == '/' || c == '\0' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let s = s.trim().trim_start_matches('.');
    if s.len() <= 180 {
        return s.to_string();
    }
    // Keep the extension when truncating.
    let (stem, ext) = s
        .rsplit_once('.')
        .filter(|(_, e)| e.len() <= 10)
        .unwrap_or((s, ""));
    let mut end = 180 - ext.len() - 1;
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    if ext.is_empty() {
        stem[..end].to_string()
    } else {
        format!("{}.{ext}", &stem[..end])
    }
}

/// `name` in dir unless another item already claimed it (via its sidecar); then insert the key tail.
fn unique(data: &Path, dir: &str, name: &str, key: &str) -> String {
    let rel = format!("{dir}/{name}");
    let claimed_by = std::fs::read_to_string(data.join(format!("{rel}.json")))
        .ok()
        .map(|s| raw(&s)["media_key"].clone());
    if claimed_by.is_none_or(|k| k == key) {
        return rel;
    }
    let tag = &key[key.len().saturating_sub(8)..];
    match name.rsplit_once('.') {
        Some((s, e)) => format!("{dir}/{s}_{tag}.{e}"),
        None => format!("{dir}/{name}_{tag}"),
    }
}

fn year_month(ms: i64) -> (i64, u32) {
    let (y, m, _) = civil(ms);
    (y, m)
}

/// UTC (year, month, day) from epoch ms. Howard Hinnant's civil_from_days.
fn civil(ms: i64) -> (i64, u32, u32) {
    let z = ms.div_euclid(86_400_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// `2025-10-29 15:50:48Z`
fn stamp(ms: i64) -> String {
    let (y, mo, d) = civil(ms);
    let s = ms.div_euclid(1000).rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02} {:02}:{:02}:{:02}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

fn pct(done: i64, total: i64) -> f64 {
    if total == 0 {
        100.0
    } else {
        done as f64 * 100.0 / total as f64
    }
}

fn fmt_bytes(b: u64) -> String {
    match b {
        0..1_000_000 => format!("{:.1} kB", b as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1} MB", b as f64 / 1e6),
        _ => format!("{:.2} GB", b as f64 / 1e9),
    }
}

/// `3h12m`, `4m05s`, `9s`
fn fmt_dur(s: u64) -> String {
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s / 60 % 60),
    }
}

/// (items per hour, ETA for `left` items) from `n` items done in `secs` this run.
fn rate_eta(n: u64, left: u64, secs: u64) -> (u64, String) {
    if n == 0 {
        return (0, "?".into());
    }
    let secs = secs.max(1); // the first items can finish within the first second
    (n * 3600 / secs, fmt_dur(left * secs / n))
}

/// `gphotosync status`: print progress from state.db without contacting Google or taking the .lock,
/// so it is safe to run alongside a sync (WAL lets this reader see committed progress).
fn status() -> Result<()> {
    let path = PathBuf::from(env("GPS_DATA", "./backup")).join("state.db");
    if !path.exists() {
        println!("no state at {}: no sync has run yet", path.display());
        return Ok(());
    }
    let db = Db::open_ro(&path)?;
    let (total, done) = db.counts()?;
    println!(
        "backed up: {done}/{total} ({:.1}%), {} pending",
        pct(done, total),
        total - done
    );
    println!("albums:    {}", db.album_count()?);
    let failing = db.failing()?;
    println!("failing:   {failing} (retried next run)");
    for (key, attempts, err) in db.recent_errors(5)? {
        println!("  {key} ({attempts}x): {err}");
    }
    let kv = |k| db.get(k).map(|v| v.unwrap_or_default());
    let when = |k| kv(k).map(|v| v.parse().map_or("-".into(), stamp));
    match kv("run_outcome")?.as_str() {
        "" => println!("last run:  none recorded"),
        "running" => println!(
            "last run:  started {}, still running (or was killed)",
            when("run_start")?
        ),
        outcome => {
            println!(
                "last run:  {} -> {}: {outcome}",
                when("run_start")?,
                when("run_end")?
            );
            let bytes = kv("run_bytes")?.parse().unwrap_or(0);
            println!(
                "           {} downloaded ({}), {} failed",
                kv("run_items")?,
                fmt_bytes(bytes),
                kv("run_failed")?
            );
        }
    }
    if let Ok(until) = kv("blocked_until")?.parse::<i64>()
        && until > now_ms()
    {
        println!(
            "cooldown:  blocked by Google, next run allowed after {}",
            stamp(until)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(year_month(0), (1970, 1));
        assert_eq!(year_month(1761753048000), (2025, 10));
        assert_eq!(year_month(951_782_400_000), (2000, 2)); // 2000-02-29
        assert_eq!(year_month(-1), (1969, 12));
        assert_eq!(sanitize("../a/b.jpg"), "_a_b.jpg");
        assert_eq!(sanitize(" .."), "");
        let long = sanitize(&format!("{}.jpeg", "é".repeat(200)));
        assert!(long.len() <= 180 && long.ends_with(".jpeg"), "{long}");
        let t = std::env::temp_dir().join(format!("gps-test-{}", std::process::id()));
        std::fs::create_dir_all(t.join("d")).unwrap();
        assert_eq!(unique(&t, "d", "x.jpg", "AF1Qip12345678"), "d/x.jpg");
        std::fs::write(t.join("d/x.jpg.json"), r#"{"media_key":"AF1Qip12345678"}"#).unwrap();
        assert_eq!(unique(&t, "d", "x.jpg", "AF1Qip12345678"), "d/x.jpg"); // own claim: resume in place
        assert_eq!(
            unique(&t, "d", "x.jpg", "AF1QipOTHERKEY"),
            "d/x_OTHERKEY.jpg"
        );
        std::fs::remove_dir_all(t).unwrap();
    }

    #[test]
    fn progress_formatting() {
        assert_eq!(stamp(1761753048000), "2025-10-29 15:50:48Z");
        assert_eq!(stamp(951_868_799_000), "2000-02-29 23:59:59Z");
        assert_eq!(fmt_dur(9), "9s");
        assert_eq!(fmt_dur(245), "4m05s");
        assert_eq!(fmt_dur(3 * 3600 + 12 * 60 + 30), "3h12m");
        assert_eq!(fmt_bytes(3_200_000), "3.2 MB");
        assert_eq!(fmt_bytes(512), "0.5 kB");
        assert_eq!(rate_eta(0, 10, 0), (0, "?".into()));
        assert_eq!(rate_eta(5, 10, 0), (18_000, "2s".into())); // sub-second start: no division by zero
        assert_eq!(rate_eta(41, 123, 3600), (41, "3h00m".into()));
        assert_eq!(pct(0, 0), 100.0);
    }

    #[test]
    fn jitter_range() {
        for _ in 0..1000 {
            let j = gphotos::jitter(0.6, 1.8);
            assert!((0.6..1.8).contains(&j), "{j}");
        }
        assert_ne!(gphotos::jitter(0.0, 1.0), gphotos::jitter(0.0, 1.0));
    }
}
