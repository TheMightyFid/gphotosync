//! SQLite state: everything needed to resume after a kill or rate limit.
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct Db(Connection);

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub key: String,
    pub ts_ms: i64,
    pub is_video: bool,
    pub meta: String,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let c = Connection::open(path)?;
        c.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS items(
               key TEXT PRIMARY KEY, ts_ms INTEGER NOT NULL,
               is_video INTEGER NOT NULL, meta TEXT NOT NULL,
               path TEXT, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT);
             CREATE INDEX IF NOT EXISTS items_pending ON items(path) WHERE path IS NULL;
             CREATE TABLE IF NOT EXISTS albums(key TEXT PRIMARY KEY, title TEXT NOT NULL, meta TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS album_items(album TEXT NOT NULL, item TEXT NOT NULL, PRIMARY KEY(album, item));
             CREATE TABLE IF NOT EXISTS kv(k TEXT PRIMARY KEY, v TEXT NOT NULL);",
        )?;
        Ok(Db(c))
    }

    pub fn get(&self, k: &str) -> Result<Option<String>> {
        Ok(self.0.query_row("SELECT v FROM kv WHERE k=?", [k], |r| r.get(0)).optional()?)
    }

    pub fn set(&self, k: &str, v: Option<&str>) -> Result<()> {
        match v {
            Some(v) => self.0.execute("INSERT OR REPLACE INTO kv VALUES(?,?)", [k, v])?,
            None => self.0.execute("DELETE FROM kv WHERE k=?", [k])?,
        };
        Ok(())
    }

    /// Upsert a page of items in one transaction. Returns how many were new.
    /// Metadata is refreshed on every sighting; download state is kept.
    pub fn upsert_items(&mut self, items: &[Item]) -> Result<usize> {
        let tx = self.0.transaction()?;
        let mut new = 0;
        {
            let mut exists = tx.prepare_cached("SELECT 1 FROM items WHERE key=?")?;
            let mut up = tx.prepare_cached(
                "INSERT INTO items(key, ts_ms, is_video, meta) VALUES(?,?,?,?)
                 ON CONFLICT(key) DO UPDATE SET meta=excluded.meta,
                   ts_ms=excluded.ts_ms, is_video=excluded.is_video",
            )?;
            for i in items {
                if !exists.exists([&i.key])? {
                    new += 1;
                }
                up.execute(params![i.key, i.ts_ms, i.is_video, i.meta])?;
            }
        }
        tx.commit()?;
        Ok(new)
    }

    pub fn upsert_album(&self, key: &str, title: &str, meta: &str) -> Result<()> {
        self.0.execute(
            "INSERT INTO albums VALUES(?,?,?) ON CONFLICT(key) DO UPDATE SET title=excluded.title, meta=excluded.meta",
            [key, title, meta],
        )?;
        Ok(())
    }

    pub fn add_album_items(&mut self, album: &str, items: &[Item]) -> Result<()> {
        self.upsert_items(items)?;
        let tx = self.0.transaction()?;
        {
            let mut st = tx.prepare_cached("INSERT OR IGNORE INTO album_items VALUES(?,?)")?;
            for i in items {
                st.execute([album, &i.key])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Items not yet on disk, fewest attempts first, so one poison item can't starve the rest.
    pub fn pending(&self, limit: usize) -> Result<Vec<Item>> {
        let mut st = self.0.prepare_cached(
            "SELECT key, ts_ms, is_video, meta FROM items WHERE path IS NULL
             ORDER BY attempts, ts_ms DESC LIMIT ?",
        )?;
        let rows = st.query_map([limit as i64], |r| {
            Ok(Item { key: r.get(0)?, ts_ms: r.get(1)?, is_video: r.get(2)?, meta: r.get(3)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn mark_done(&self, key: &str, path: &str) -> Result<()> {
        self.0.execute("UPDATE items SET path=?, last_error=NULL WHERE key=?", [path, key])?;
        Ok(())
    }

    pub fn mark_failed(&self, key: &str, err: &str) -> Result<()> {
        self.0.execute("UPDATE items SET attempts=attempts+1, last_error=? WHERE key=?", [err, key])?;
        Ok(())
    }

    /// (album title, item path) for every downloaded album member.
    pub fn album_links(&self) -> Result<Vec<(String, String, String)>> {
        let mut st = self.0.prepare(
            "SELECT a.key, a.title, i.path FROM album_items ai JOIN albums a ON a.key=ai.album
             JOIN items i ON i.key=ai.item WHERE i.path IS NOT NULL",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn counts(&self) -> Result<(i64, i64)> {
        Ok(self.0.query_row(
            "SELECT COUNT(*), COUNT(path) FROM items",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_state() {
        let mut db = Db::open(Path::new(":memory:")).unwrap();
        let it = |k: &str| Item { key: k.into(), ts_ms: 1, is_video: false, meta: "{}".into() };
        assert_eq!(db.upsert_items(&[it("a"), it("b")]).unwrap(), 2);
        assert_eq!(db.upsert_items(&[it("a")]).unwrap(), 0);
        db.mark_failed("a", "boom").unwrap();
        assert_eq!(db.pending(10).unwrap()[0].key, "b"); // failed item goes last
        db.mark_done("b", "x.jpg").unwrap();
        db.upsert_items(&[it("b")]).unwrap(); // re-sighting must not reset download state
        assert_eq!(db.counts().unwrap(), (2, 1));
        db.set("tok", Some("t1")).unwrap();
        assert_eq!(db.get("tok").unwrap().as_deref(), Some("t1"));
        db.set("tok", None).unwrap();
        assert_eq!(db.get("tok").unwrap(), None);
        db.upsert_album("al", "Trip", "{}").unwrap();
        db.add_album_items("al", &[it("b")]).unwrap();
        assert_eq!(db.album_links().unwrap(), vec![("al".into(), "Trip".into(), "x.jpg".into())]);
    }
}
