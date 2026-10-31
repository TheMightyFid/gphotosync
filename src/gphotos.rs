//! photos.google.com internal RPC client over plain HTTP, authenticated with exported browser cookies.
//! See docs/rpc-spec.md for where every index below comes from.
use crate::db::Item;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::header::{CONTENT_TYPE, COOKIE, LOCATION, SET_COOKIE, USER_AGENT};
use reqwest::{Response, StatusCode, Url};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

/// Conditions where continuing is pointless; the run stops and state is kept for next time.
#[derive(Debug)]
pub enum Fatal {
    Blocked(String),
    SignedOut(String),
}
impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Fatal::Blocked(s) => write!(f, "rate limited / blocked by Google: {s}"),
            Fatal::SignedOut(s) => write!(f, "session expired, re-export cookies: {s}"),
        }
    }
}
impl std::error::Error for Fatal {}

/// Netscape cookies.txt jar, keyed by (domain, name). Set-Cookie responses are merged back in.
#[derive(Default)]
pub struct Jar(Vec<(String, String, String)>);

impl Jar {
    pub fn parse(txt: &str) -> Jar {
        let mut j = Jar::default();
        for line in txt.lines() {
            let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
            let f: Vec<&str> = line.split('\t').collect();
            if line.starts_with('#') || f.len() < 7 {
                continue;
            }
            j.put(f[0], f[5], f[6]);
        }
        j
    }

    pub fn dump(&self) -> String {
        self.0.iter().map(|(d, n, v)| format!(".{d}\tTRUE\t/\tTRUE\t0\t{n}\t{v}\n")).collect()
    }

    fn put(&mut self, domain: &str, name: &str, value: &str) {
        let d = domain.trim_start_matches('.').to_ascii_lowercase();
        match self.0.iter_mut().find(|(od, on, _)| *od == d && on == name) {
            Some(c) => c.2 = value.to_string(),
            None => self.0.push((d, name.to_string(), value.to_string())),
        }
    }

    // ponytail: ignores cookie paths and expiry; Google auth cookies are all path=/.
    pub fn header(&self, host: &str) -> String {
        let cs: Vec<String> = self
            .0
            .iter()
            .filter(|(d, _, _)| host == d || host.ends_with(&format!(".{d}")))
            .map(|(_, n, v)| format!("{n}={v}"))
            .collect();
        cs.join("; ")
    }

    fn absorb(&mut self, resp: &Response, host: &str) {
        for h in resp.headers().get_all(SET_COOKIE) {
            let Ok(s) = h.to_str() else { continue };
            let mut parts = s.split(';');
            let Some((n, v)) = parts.next().and_then(|p| p.trim().split_once('=')) else { continue };
            let domain = parts
                .filter_map(|p| p.trim().split_once('='))
                .find(|(k, _)| k.eq_ignore_ascii_case("domain"))
                .map_or(host, |(_, d)| d);
            self.put(domain, n, v);
        }
    }
}

pub struct Client {
    http: reqwest::Client,
    pub jar: Jar,
    base: String,
    at: String,
    fsid: String,
    bl: String,
    delay: Duration,
    last_rpc: Instant,
}

impl Client {
    pub async fn new(jar: Jar, account: u32, delay: Duration) -> Result<Client> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(600))
            .connect_timeout(Duration::from_secs(30))
            .build()?;
        let mut c = Client {
            http,
            jar,
            base: format!("https://photos.google.com/u/{account}"),
            at: String::new(),
            fsid: String::new(),
            bl: String::new(),
            delay,
            last_rpc: Instant::now() - delay,
        };
        c.bootstrap().await?;
        Ok(c)
    }

    /// Scrape the per-session tokens out of WIZ_global_data on the main page.
    async fn bootstrap(&mut self) -> Result<()> {
        let html = self.fetch(&format!("{}/", self.base), None).await?.text().await?;
        let g = wiz_global_data(&html)
            .ok_or_else(|| Fatal::SignedOut("no WIZ_global_data on photos.google.com".into()))?;
        let s = |k: &str| g[k].as_str().map(String::from).ok_or_else(|| anyhow!("WIZ_global_data missing {k}"));
        self.at = s("SNlM0e").map_err(|_| Fatal::SignedOut("no SNlM0e token (not logged in?)".into()))?;
        self.fsid = s("FdrFJe")?;
        self.bl = s("cfb2h")?;
        if let Some(email) = g["oPEP7c"].as_str() {
            eprintln!("signed in as {email}");
        }
        Ok(())
    }

    /// GET (or POST a form), following redirects by hand so cookies go to every Google host
    /// in the chain and block/sign-out redirects are caught.
    pub async fn fetch(&mut self, url: &str, form: Option<&[(&str, &str)]>) -> Result<Response> {
        let mut url = Url::parse(url)?;
        let mut form = form;
        for _ in 0..10 {
            let host = url.host_str().unwrap_or_default().to_string();
            let rb = match form {
                Some(f) => self.http.post(url.clone()).form(f).header("x-same-domain", "1"),
                None => self.http.get(url.clone()),
            };
            let resp = rb
                .header(COOKIE, self.jar.header(&host))
                .header(USER_AGENT, UA)
                .header("origin", "https://photos.google.com")
                .header("referer", "https://photos.google.com/")
                .send()
                .await?;
            self.jar.absorb(&resp, &host);
            let st = resp.status();
            if st == StatusCode::TOO_MANY_REQUESTS {
                Err(Fatal::Blocked(format!("429 from {host}")))?
            }
            if !st.is_redirection() {
                if resp.headers().get(CONTENT_TYPE).is_some_and(|v| v.as_bytes().starts_with(b"text/html")) && form.is_none() && !host.starts_with("photos.") {
                    bail!("got HTML instead of media from {host}");
                }
                return Ok(resp);
            }
            let loc = resp.headers().get(LOCATION).and_then(|v| v.to_str().ok()).unwrap_or_default();
            url = url.join(loc)?;
            let u = url.as_str();
            if u.contains("google.com/sorry") {
                Err(Fatal::Blocked(u.into()))?
            }
            if u.contains("accounts.google.com") || u.contains("/photos/about") || u.contains("consent.google.com") {
                Err(Fatal::SignedOut(u.into()))?
            }
            form = None; // 30x after POST becomes GET, like a browser
        }
        bail!("too many redirects")
    }

    /// One batchexecute call. Retries transient failures; refreshes tokens once on 400/403.
    pub async fn rpc(&mut self, rpcid: &str, payload: Value) -> Result<Value> {
        let freq = json!([[[rpcid, payload.to_string(), null, "generic"]]]).to_string();
        let mut last = anyhow!("unreachable");
        for attempt in 1..=4u32 {
            if let Some(wait) = self.delay.checked_sub(self.last_rpc.elapsed()) {
                tokio::time::sleep(wait).await;
            }
            self.last_rpc = Instant::now();
            let url = format!(
                "https://photos.google.com/_/PhotosUi/data/batchexecute?rpcids={rpcid}&source-path=%2F&f.sid={}&bl={}&rt=c",
                self.fsid, self.bl
            );
            let at = self.at.clone();
            let res = match self.fetch(&url, Some(&[("f.req", &freq), ("at", &at)])).await {
                Ok(r) if matches!(r.status().as_u16(), 400 | 401 | 403) && attempt == 1 => {
                    eprintln!("{rpcid}: {} - refreshing session tokens", r.status());
                    self.bootstrap().await?;
                    continue;
                }
                Ok(r) if r.status().is_success() => parse_batch(&r.text().await?, rpcid),
                Ok(r) => Err(anyhow!("{rpcid}: HTTP {}", r.status())),
                Err(e) if e.is::<Fatal>() => return Err(e),
                Err(e) => Err(e),
            };
            match res {
                Ok(v) => return Ok(v),
                Err(e) => last = e,
            }
            let backoff = Duration::from_secs(2u64.pow(attempt));
            eprintln!("{rpcid} attempt {attempt} failed: {last:#}; retrying in {backoff:?}");
            tokio::time::sleep(backoff).await;
        }
        Err(last)
    }

    /// One page of the whole library (library + archive), newest first.
    pub async fn library_page(&mut self, token: Option<&str>) -> Result<(Vec<Item>, Option<String>)> {
        let d = self.rpc("lcxiM", json!([token, null, 500, null, 1, 3])).await?;
        Ok((items(&d[0]), next(&d[1])))
    }

    /// One page of albums: (key, title, authKey, raw record).
    pub async fn albums_page(&mut self, token: Option<&str>) -> Result<(Vec<(String, String, Option<String>, Value)>, Option<String>)> {
        let d = self.rpc("Z5xsfc", json!([token, null, null, null, 1, null, null, 100, [2], 5])).await?;
        let albums = d[0]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| {
                let e = &ext(a)?["72930366"];
                Some((a[0].as_str()?.into(), e[1].as_str().unwrap_or("untitled").into(), e[5].as_str().map(String::from), a.clone()))
            })
            .collect();
        Ok((albums, next(&d[1])))
    }

    pub async fn album_page(&mut self, album: &str, auth: Option<&str>, token: Option<&str>) -> Result<(Vec<Item>, Option<String>)> {
        let d = self.rpc("snAcKc", json!([album, token, null, auth])).await?;
        Ok((items(&d[1]), next(&d[2])))
    }

    /// (original download URL, live-photo video URL, extended info record).
    pub async fn item_urls(&mut self, key: &str) -> Result<(String, Option<String>, Value)> {
        let info = self.rpc("VrseUb", json!([key, null, null, null, null])).await?;
        let url = [&info[7], &info[1]]
            .into_iter()
            .find_map(|v| v.as_str().filter(|s| s.starts_with("https://")))
            .ok_or_else(|| anyhow!("no download URL for {key}"))?
            .to_string();
        let live = ext(&info[0]).and_then(|e| e.get("146008172")?[3].as_str().map(String::from));
        let full = self.rpc("fDcn4b", json!([key, 1, null, null, 1])).await?;
        Ok((url, live, full))
    }
}

/// The trailing `{ "<field>": ... }` extension object of a record.
fn ext(rec: &Value) -> Option<&serde_json::Map<String, Value>> {
    rec.as_array()?.last()?.as_object()
}

fn next(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(String::from)
}

pub fn parse_item(rec: &Value) -> Option<Item> {
    Some(Item {
        key: rec[0].as_str()?.into(),
        url: rec[1][0].as_str()?.into(),
        ts_ms: rec[2].as_i64()?,
        is_video: ext(rec).is_some_and(|e| e.contains_key("76647426")),
        meta: rec.to_string(),
    })
}

fn items(v: &Value) -> Vec<Item> {
    v.as_array().into_iter().flatten().filter_map(parse_item).collect()
}

pub fn wiz_global_data(html: &str) -> Option<Value> {
    let start = html.find("WIZ_global_data = ")? + "WIZ_global_data = ".len();
    let end = start + html[start..].find("</script>")?;
    serde_json::from_str(html[start..end].trim().trim_end_matches(';')).ok()
}

/// Pull the `wrb.fr` envelope for `rpcid` out of a `)]}'`-prefixed batchexecute body and decode
/// its double-encoded payload.
pub fn parse_batch(body: &str, rpcid: &str) -> Result<Value> {
    if body.contains("unusual traffic") || body.contains("g-recaptcha") {
        Err(Fatal::Blocked("captcha page".into()))?
    }
    for line in body.lines().filter(|l| l.contains("wrb.fr")) {
        let v: Value = serde_json::from_str(line).context("bad batchexecute chunk")?;
        for env in v.as_array().into_iter().flatten() {
            if env[0] == "wrb.fr" && env[1] == rpcid {
                let s = env[2].as_str().ok_or_else(|| anyhow!("{rpcid}: empty result (error {})", env[5]))?;
                return Ok(serde_json::from_str(s)?);
            }
        }
    }
    bail!("{rpcid}: no result in response: {:.200}", body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_parsing() {
        let body = ")]}'\n\n120\n[[\"wrb.fr\",\"lcxiM\",\"[[[\\\"AF1Q\\\",[\\\"https://x/y\\\",10,20],1761753048000,\\\"d\\\",3600000,1,{\\\"76647426\\\":[1]}]],\\\"tok\\\"]\",null,null,null,\"generic\"]]\n25\n[[\"e\",4,null,null,1290]]\n";
        let d = parse_batch(body, "lcxiM").unwrap();
        let it = items(&d[0]);
        assert_eq!((it[0].key.as_str(), it[0].ts_ms, it[0].is_video), ("AF1Q", 1761753048000, true));
        assert_eq!(next(&d[1]).as_deref(), Some("tok"));
        assert!(parse_batch(body, "Z5xsfc").is_err());
        let err = ")]}'\n\n50\n[[\"wrb.fr\",\"lcxiM\",null,null,null,[3],\"generic\"]]\n";
        assert!(parse_batch(err, "lcxiM").is_err());
    }

    #[test]
    fn wiz_and_jar() {
        let html = r#"<script data-id="_gd">window.WIZ_global_data = {"SNlM0e":"tok","FdrFJe":"-12"};</script>"#;
        assert_eq!(wiz_global_data(html).unwrap()["FdrFJe"], "-12");
        let j = Jar::parse("# c\n.google.com\tTRUE\t/\tTRUE\t0\tSID\tabc\n#HttpOnly_photos.google.com\tFALSE\t/\tTRUE\t0\tX\ty\n");
        assert_eq!(j.header("photos.google.com"), "SID=abc; X=y");
        assert_eq!(j.header("lh3.googleusercontent.com"), "");
        assert_eq!(Jar::parse(&j.dump()).header("photos.google.com"), "SID=abc; X=y");
    }
}
