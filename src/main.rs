//! gphotosync: resumable Google Photos backup. Safe to kill at any point and re-run from cron.
mod db;
mod gphotos;

use anyhow::{Context, Result, bail};
use db::Db;
use futures_util::StreamExt;
use gphotos::{Client, Fatal, Jar};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

fn env(k: &str, default: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() {
    match run().await {
        Ok(()) => {}
        Err(e) => {
            eprintln!("error: {e:#}");
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
    if lock.try_lock().is_err() {
        eprintln!("another run holds {}, exiting", data.join(".lock").display());
        return Ok(());
    }

    // Google rotates session cookies; we save the rotated jar and prefer it while it's newer than the input.
    let saved = data.join("cookies.txt");
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let src = if mtime(&saved) > mtime(&cookies) { &saved } else { &cookies };
    let jar = Jar::parse(&std::fs::read_to_string(src).with_context(|| format!("reading {}", src.display()))?);

    let mut db = Db::open(&data.join("state.db"))?;
    let mut gp = Client::new(jar, account, delay).await?;
    let res = sync(&mut gp, &mut db, &data).await;
    std::fs::write(&saved, gp.jar.dump())?;
    let (total, done) = db.counts()?;
    eprintln!("{done}/{total} items backed up");
    res
}

async fn sync(gp: &mut Client, db: &mut Db, data: &Path) -> Result<()> {
    index_library(gp, db).await?;
    if env("GPS_ALBUMS", "1") == "1" {
        index_albums(gp, db).await?;
    }
    download(gp, db, data).await?;
    link_albums(db, data)
}

/// Page the whole library into the DB, checkpointing the page token so a stopped run resumes mid-listing.
// ponytail: full listing every run (500 items/RPC, so 50k items is ~100 calls). This also catches
// old-dated uploads that a "stop at first known item" incremental scan would miss.
async fn index_library(gp: &mut Client, db: &mut Db) -> Result<()> {
    let mut token = db.get("lib_token")?;
    loop {
        let (items, next) = match gp.library_page(token.as_deref()).await {
            Err(e) if token.is_some() && !e.is::<Fatal>() => {
                // ponytail: assumes a failing saved token has expired; restarting is safe, upserts are idempotent.
                eprintln!("saved page token rejected ({e:#}), restarting listing from the top");
                token = None;
                continue;
            }
            r => r?,
        };
        let new = db.upsert_items(&items)?;
        eprintln!("indexed page: {} items, {new} new", items.len());
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
    loop {
        let (albums, next) = gp.albums_page(token.as_deref()).await?;
        for (key, title, auth, raw) in albums {
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
            eprintln!("album {title:?} indexed");
        }
        if next.is_none() {
            return Ok(());
        }
        token = next;
    }
}

async fn download(gp: &mut Client, db: &mut Db, data: &Path) -> Result<()> {
    let mut tried = HashSet::new();
    let mut consecutive_failures = 0;
    loop {
        let batch: Vec<_> = db.pending(200)?.into_iter().filter(|i| !tried.contains(&i.key)).collect();
        if batch.is_empty() {
            return Ok(());
        }
        for item in batch {
            tried.insert(item.key.clone());
            match download_one(gp, &item, data).await {
                Ok(path) => {
                    consecutive_failures = 0;
                    db.mark_done(&item.key, &path)?;
                    eprintln!("saved {path}");
                }
                Err(e) if e.is::<Fatal>() => return Err(e),
                Err(e) => {
                    eprintln!("failed {}: {e:#}", item.key);
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

/// Download one item to media/YYYY/MM/<original filename>, with a JSON sidecar. Returns the path relative to `data`.
// ponytail: one download at a time; it's gentle on rate limits. Add a JoinSet if bandwidth goes underused.
async fn download_one(gp: &mut Client, item: &db::Item, data: &Path) -> Result<String> {
    let (url, live, full) = gp.item_urls(&item.key).await?;
    let d = &full[0];
    let name = d[2].as_str().map(sanitize).filter(|s| !s.is_empty()).unwrap_or_else(|| {
        format!("{}.{}", sanitize(&item.key), if item.is_video { "mp4" } else { "jpg" })
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
    std::fs::write(data.join(format!("{rel}.json")), serde_json::to_vec_pretty(&sidecar)?)?;
    let size = fetch_to(gp, &url, &data.join(&rel)).await?;
    if let Some(expect) = d[5].as_u64()
        && expect != size
    {
        // Logged, not fatal: the "original" download of edited items can legitimately differ in size.
        eprintln!("note: {rel} is {size} bytes, Google reports {expect}");
    }
    if let (Some(v), Some(vrel)) = (live, sidecar["live_video"].as_str()) {
        fetch_to(gp, &v, &data.join(vrel)).await?;
    }
    Ok(rel)
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

/// albums/<title>/<file> symlinks into media/, rebuilt each run.
fn link_albums(db: &Db, data: &Path) -> Result<()> {
    let albums = data.join("albums");
    for (key, title, path) in db.album_links()? {
        let dir = albums.join(format!("{} [{}]", sanitize(&title), &key[key.len().saturating_sub(6)..]));
        std::fs::create_dir_all(&dir)?;
        let file = Path::new(&path).file_name().context("bad path")?;
        let link = dir.join(file);
        if link.symlink_metadata().is_err() {
            std::os::unix::fs::symlink(Path::new("../..").join(&path), &link)?;
        }
    }
    Ok(())
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c == '/' || c == '\0' || c.is_control() { '_' } else { c }).collect::<String>().trim_start_matches('.').trim().to_string()
}

/// `name` in dir unless another item already claimed it (via its sidecar); then insert the key tail.
fn unique(data: &Path, dir: &str, name: &str, key: &str) -> String {
    let rel = format!("{dir}/{name}");
    let claimed_by = std::fs::read_to_string(data.join(format!("{rel}.json"))).ok().map(|s| raw(&s)["media_key"].clone());
    if claimed_by.is_none_or(|k| k == key) {
        return rel;
    }
    let tag = &key[key.len().saturating_sub(8)..];
    match name.rsplit_once('.') {
        Some((s, e)) => format!("{dir}/{s}_{tag}.{e}"),
        None => format!("{dir}/{name}_{tag}"),
    }
}

/// UTC (year, month) from epoch ms. Howard Hinnant's civil_from_days.
fn year_month(ms: i64) -> (i64, u32) {
    let z = ms.div_euclid(86_400_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m)
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
        let t = std::env::temp_dir().join(format!("gps-test-{}", std::process::id()));
        std::fs::create_dir_all(t.join("d")).unwrap();
        assert_eq!(unique(&t, "d", "x.jpg", "AF1Qip12345678"), "d/x.jpg");
        std::fs::write(t.join("d/x.jpg.json"), r#"{"media_key":"AF1Qip12345678"}"#).unwrap();
        assert_eq!(unique(&t, "d", "x.jpg", "AF1Qip12345678"), "d/x.jpg"); // own claim: resume in place
        assert_eq!(unique(&t, "d", "x.jpg", "AF1QipOTHERKEY"), "d/x_OTHERKEY.jpg");
        std::fs::remove_dir_all(t).unwrap();
    }
}
