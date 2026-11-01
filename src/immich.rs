//! Mirror Google Photos album membership into Immich, for photos Immich has indexed from an external
//! library pointed at our media/ directory. Never touches files; only creates albums and adds assets.
use crate::db::Db;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

pub struct Immich {
    http: reqwest::Client,
    url: String,
    key: String,
    /// Where Immich sees our `media/` directory, e.g. `/gphotosync/media`.
    prefix: String,
}

impl Immich {
    pub fn new(url: &str, key: &str, prefix: &str) -> Result<Immich> {
        Ok(Immich {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()?,
            url: url.trim_end_matches('/').to_string(),
            key: key.to_string(),
            prefix: prefix.trim_end_matches('/').to_string(),
        })
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut rb = self
            .http
            .request(method, format!("{}/api{path}", self.url))
            .header("x-api-key", &self.key);
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let resp = rb.send().await.with_context(|| format!("Immich {path}"))?;
        let st = resp.status();
        let text = resp.text().await?;
        if !st.is_success() {
            bail!("Immich {path}: HTTP {st}: {text:.300}");
        }
        Ok(if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text)?
        })
    }

    /// originalPath -> asset id for everything Immich has under our prefix.
    // ponytail: one paged scan per run (~30 requests for 30k assets); per-asset lookups if this gets slow.
    async fn assets(&self) -> Result<HashMap<String, String>> {
        let mut map = HashMap::new();
        let mut page = 1;
        loop {
            // Immich matches originalPath as a substring, so prefix + "/" selects our tree.
            let body =
                json!({ "originalPath": format!("{}/", self.prefix), "page": page, "size": 1000 });
            let r = self
                .call(reqwest::Method::POST, "/search/metadata", Some(body))
                .await?;
            for a in r["assets"]["items"].as_array().into_iter().flatten() {
                if let (Some(p), Some(id)) = (a["originalPath"].as_str(), a["id"].as_str()) {
                    map.insert(p.to_string(), id.to_string());
                }
            }
            match r["assets"]["nextPage"]
                .as_str()
                .and_then(|p| p.parse().ok())
            {
                Some(n) => page = n,
                None => return Ok(map),
            }
        }
    }

    /// Our `media/2024/05/x.jpg` as Immich sees it.
    fn immich_path(&self, rel: &str) -> Option<String> {
        Some(format!("{}/{}", self.prefix, rel.strip_prefix("media/")?))
    }

    pub async fn sync_albums(&self, db: &Db) -> Result<()> {
        let assets = self.assets().await?;
        log!(
            "immich: {} assets indexed under {}",
            assets.len(),
            self.prefix
        );
        let existing: HashMap<String, String> = self
            .call(reqwest::Method::GET, "/albums", None)
            .await?
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| {
                Some((
                    a["albumName"].as_str()?.to_string(),
                    a["id"].as_str()?.to_string(),
                ))
            })
            .collect();

        // Group downloaded album members by Google album.
        let mut albums: HashMap<String, (String, Vec<String>)> = HashMap::new();
        let mut missing = 0;
        for (key, title, path) in db.album_links()? {
            match self.immich_path(&path).and_then(|p| assets.get(&p)) {
                Some(id) => albums
                    .entry(key)
                    .or_insert_with(|| (title, vec![]))
                    .1
                    .push(id.clone()),
                None => missing += 1,
            }
        }

        let (mut added, mut created) = (0, 0);
        for (key, (title, ids)) in albums {
            // Remembered id first (survives renames in either app), then same-name album, else create.
            // ponytail: two Google albums with the same title merge into one Immich album on first sync.
            let kv = format!("immich_album:{key}");
            let mut id = db.get(&kv)?.or_else(|| existing.get(&title).cloned());
            if id.is_none() {
                let r = self
                    .call(
                        reqwest::Method::POST,
                        "/albums",
                        Some(json!({ "albumName": title })),
                    )
                    .await?;
                id = r["id"].as_str().map(String::from);
                created += 1;
            }
            let id = id.context("Immich returned no album id")?;
            db.set(&kv, Some(&id))?;
            for chunk in ids.chunks(500) {
                let r = self
                    .call(
                        reqwest::Method::PUT,
                        &format!("/albums/{id}/assets"),
                        Some(json!({ "ids": chunk })),
                    )
                    .await;
                let r = match r {
                    // Album deleted in Immich: forget it so next run recreates it.
                    Err(e)
                        if format!("{e}").contains("HTTP 400")
                            || format!("{e}").contains("HTTP 404") =>
                    {
                        db.set(&kv, None)?;
                        return Err(e.context(format!(
                            "album {title:?} gone from Immich; will recreate next run"
                        )));
                    }
                    r => r?,
                };
                added += r
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|x| x["success"] == true)
                    .count();
            }
        }
        log!("immich: {created} albums created, {added} photos added to albums");
        if missing > 0 {
            log!(
                "immich: {missing} album photos not in Immich yet (library not rescanned?); retried next run"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_mapping() {
        let im = Immich::new("http://x:2283/", "k", "/gphotosync/media/").unwrap();
        assert_eq!(im.url, "http://x:2283");
        assert_eq!(
            im.immich_path("media/2024/05/a b.jpg").as_deref(),
            Some("/gphotosync/media/2024/05/a b.jpg")
        );
        assert_eq!(im.immich_path("albums/x.jpg"), None);
    }
}
