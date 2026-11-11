//! The offline catalog: every tape ever seen, what its LTFS volume holds
//! (folders and files, as of an index generation) and where the tape is.
//! Browsing needs no tape; reading and writing check the tape's index
//! generation against the catalog's.

pub mod place;

use std::path::Path;

use ltfs::{Index, Node, Time};
use rusqlite::{params, Connection, OptionalExtension};

pub use rusqlite::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// What the catalog knows about a tape.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tape {
    pub barcode: String,
    /// The LTFS volume's UUID; empty when the tape isn't known to hold LTFS.
    pub uuid: String,
    /// The volume's name (its root folder's).
    pub name: String,
    /// The index generation the catalog holds.
    pub generation: u64,
    /// When that index was written.
    pub updated: Option<Time>,
    pub capacity: Option<u64>,
    pub remaining: Option<u64>,
    pub files: u64,
    pub bytes: u64,
    pub write_protected: bool,
    /// Where the tape is (`slot 3`, `drive 1`, `mail slot`), empty when outside the library.
    pub location: String,
    /// The user's note (`offsite: bank vault`).
    pub note: String,
    /// When the library last reported it.
    pub seen: Option<Time>,
    /// When its index was last read into the catalog.
    pub imported: Option<Time>,
}

impl Tape {
    /// Whether its contents are in the catalog.
    pub fn is_ltfs(&self) -> bool {
        !self.uuid.is_empty()
    }
    /// Whether the library has it now.
    pub fn in_library(&self) -> bool {
        !self.location.is_empty()
    }
}

/// An item of a tape's volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its LTFS name.
    pub name: String,
    pub dir: bool,
    pub size: u64,
    pub modified: Time,
    pub readonly: bool,
}

pub struct Catalog {
    db: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tapes(
    barcode TEXT PRIMARY KEY COLLATE NOCASE,
    uuid TEXT NOT NULL DEFAULT '',
    name TEXT NOT NULL DEFAULT '',
    generation INTEGER NOT NULL DEFAULT 0,
    updated INTEGER, updated_ns INTEGER,
    capacity INTEGER, remaining INTEGER,
    files INTEGER NOT NULL DEFAULT 0, bytes INTEGER NOT NULL DEFAULT 0,
    write_protected INTEGER NOT NULL DEFAULT 0,
    location TEXT NOT NULL DEFAULT '',
    note TEXT NOT NULL DEFAULT '',
    seen INTEGER, imported INTEGER
);
CREATE TABLE IF NOT EXISTS entries(
    tape TEXT NOT NULL COLLATE NOCASE,
    parent TEXT NOT NULL COLLATE NOCASE,
    name TEXT NOT NULL COLLATE NOCASE,
    dir INTEGER NOT NULL,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL, mtime_ns INTEGER NOT NULL,
    readonly INTEGER NOT NULL,
    PRIMARY KEY(tape, parent, name)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
INSERT OR IGNORE INTO meta VALUES('schema', '1');
";

fn time(secs: Option<i64>, ns: Option<i64>) -> Option<Time> {
    secs.map(|s| Time { secs: s, nanos: ns.unwrap_or(0) as u32 })
}

/// The parent folder and name of a path inside a tape (`\`-separated).
fn split(path: &str) -> (String, String) {
    let parts = ltfs::index::split(path);
    match parts.split_last() {
        Some((name, dirs)) => (dirs.join("\\"), (*name).to_owned()),
        None => (String::new(), String::new()),
    }
}

impl Catalog {
    /// Open (or create) the catalog at `path`.
    pub fn open(path: &Path) -> Result<Catalog> {
        let db = Connection::open(path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        db.busy_timeout(std::time::Duration::from_secs(10))?;
        db.execute_batch(SCHEMA)?;
        Ok(Catalog { db })
    }

    /// A catalog in memory (tests).
    pub fn in_memory() -> Result<Catalog> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(SCHEMA)?;
        Ok(Catalog { db })
    }

    fn tape_row(r: &rusqlite::Row) -> rusqlite::Result<Tape> {
        Ok(Tape {
            barcode: r.get(0)?,
            uuid: r.get(1)?,
            name: r.get(2)?,
            generation: r.get::<_, i64>(3)? as u64,
            updated: time(r.get(4)?, r.get(5)?),
            capacity: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
            remaining: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
            files: r.get::<_, i64>(8)? as u64,
            bytes: r.get::<_, i64>(9)? as u64,
            write_protected: r.get::<_, i64>(10)? != 0,
            location: r.get(11)?,
            note: r.get(12)?,
            seen: time(r.get(13)?, None),
            imported: time(r.get(14)?, None),
        })
    }

    const TAPE_COLS: &'static str = "barcode, uuid, name, generation, updated, updated_ns, capacity, remaining, files, bytes, write_protected, location, note, seen, imported";

    /// Every tape, by barcode.
    pub fn tapes(&self) -> Result<Vec<Tape>> {
        let mut st = self.db.prepare(&format!("SELECT {} FROM tapes ORDER BY barcode", Self::TAPE_COLS))?;
        let rows = st.query_map([], Self::tape_row)?;
        rows.collect()
    }

    pub fn tape(&self, barcode: &str) -> Result<Option<Tape>> {
        self.db.query_row(&format!("SELECT {} FROM tapes WHERE barcode = ?1", Self::TAPE_COLS), [barcode], Self::tape_row).optional()
    }

    fn ensure(&self, barcode: &str) -> Result<()> {
        self.db.execute("INSERT OR IGNORE INTO tapes(barcode) VALUES(?1)", [barcode])?;
        Ok(())
    }

    /// Put a tape's index into the catalog, replacing what it held for it.
    pub fn import(&mut self, barcode: &str, index: &Index, capacity: Option<u64>, remaining: Option<u64>, write_protected: bool) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("INSERT OR IGNORE INTO tapes(barcode) VALUES(?1)", [barcode])?;
        tx.execute("DELETE FROM entries WHERE tape = ?1", [barcode])?;
        let (mut files, mut bytes) = (0u64, 0u64);
        {
            let mut ins = tx.prepare("INSERT OR REPLACE INTO entries(tape, parent, name, dir, size, mtime, mtime_ns, readonly) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")?;
            let mut err = None;
            index.walk(&mut |path, node| {
                if err.is_some() {
                    return;
                }
                let (parent, name) = split(path);
                let (dir, size, ro) = match node {
                    Node::Dir(d) => (true, 0, d.readonly),
                    Node::File(f) => {
                        files += 1;
                        bytes += f.length;
                        (false, f.length, f.readonly)
                    }
                };
                let m = node.times().modify;
                if let Err(e) = ins.execute(params![barcode, parent, name, dir as i64, size as i64, m.secs, m.nanos as i64, ro as i64]) {
                    err = Some(e);
                }
            });
            if let Some(e) = err {
                return Err(e);
            }
        }
        tx.execute(
            "UPDATE tapes SET uuid = ?2, name = ?3, generation = ?4, updated = ?5, updated_ns = ?6, capacity = COALESCE(?7, capacity), remaining = COALESCE(?8, remaining),
             files = ?9, bytes = ?10, write_protected = ?11, imported = ?12 WHERE barcode = ?1",
            params![
                barcode,
                index.volume_uuid,
                index.root.name,
                index.generation as i64,
                index.update_time.secs,
                index.update_time.nanos as i64,
                capacity.map(|v| v as i64),
                remaining.map(|v| v as i64),
                files as i64,
                bytes as i64,
                write_protected as i64,
                Time::now().secs
            ],
        )?;
        tx.commit()
    }

    /// Record a tape that holds no LTFS volume (blank, or another format).
    pub fn mark_not_ltfs(&mut self, barcode: &str, write_protected: bool) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("INSERT OR IGNORE INTO tapes(barcode) VALUES(?1)", [barcode])?;
        tx.execute("DELETE FROM entries WHERE tape = ?1", [barcode])?;
        tx.execute(
            "UPDATE tapes SET uuid = '', name = '', generation = 0, files = 0, bytes = 0, write_protected = ?2, imported = ?3 WHERE barcode = ?1",
            params![barcode, write_protected as i64, Time::now().secs],
        )?;
        tx.commit()
    }

    /// The library's inventory: where each tape it holds is. Tapes it
    /// doesn't hold are marked as outside it.
    pub fn set_locations(&mut self, locations: &[(String, String)]) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("UPDATE tapes SET location = ''", [])?;
        let now = Time::now().secs;
        for (barcode, loc) in locations {
            tx.execute("INSERT OR IGNORE INTO tapes(barcode) VALUES(?1)", [barcode])?;
            tx.execute("UPDATE tapes SET location = ?2, seen = ?3 WHERE barcode = ?1", params![barcode, loc, now])?;
        }
        tx.commit()
    }

    pub fn set_note(&self, barcode: &str, note: &str) -> Result<()> {
        self.ensure(barcode)?;
        self.db.execute("UPDATE tapes SET note = ?2 WHERE barcode = ?1", params![barcode, note])?;
        Ok(())
    }

    pub fn set_space(&self, barcode: &str, capacity: Option<u64>, remaining: Option<u64>) -> Result<()> {
        self.db.execute(
            "UPDATE tapes SET capacity = COALESCE(?2, capacity), remaining = COALESCE(?3, remaining) WHERE barcode = ?1",
            params![barcode, capacity.map(|v| v as i64), remaining.map(|v| v as i64)],
        )?;
        Ok(())
    }

    /// Forget a tape and its contents.
    pub fn forget(&self, barcode: &str) -> Result<()> {
        self.db.execute("DELETE FROM entries WHERE tape = ?1", [barcode])?;
        self.db.execute("DELETE FROM tapes WHERE barcode = ?1", [barcode])?;
        Ok(())
    }

    /// The item at `path` of a tape ("" is its root: `None` here).
    pub fn entry(&self, barcode: &str, path: &str) -> Result<Option<Entry>> {
        let (parent, name) = split(path);
        if name.is_empty() {
            return Ok(None);
        }
        self.db
            .query_row("SELECT name, dir, size, mtime, mtime_ns, readonly FROM entries WHERE tape = ?1 AND parent = ?2 AND name = ?3", params![barcode, parent, name], entry_row)
            .optional()
    }

    /// Whether `path` is a folder of the tape (its root is one).
    pub fn is_dir(&self, barcode: &str, path: &str) -> Result<bool> {
        if ltfs::index::split(path).is_empty() {
            return Ok(self.tape(barcode)?.is_some_and(|t| t.is_ltfs()));
        }
        Ok(self.entry(barcode, path)?.is_some_and(|e| e.dir))
    }

    /// The items in the folder `path` of a tape; `None` when it isn't a folder.
    pub fn children(&self, barcode: &str, path: &str) -> Result<Option<Vec<Entry>>> {
        if !self.is_dir(barcode, path)? {
            return Ok(None);
        }
        let parent = ltfs::index::split(path).join("\\");
        let mut st = self.db.prepare_cached("SELECT name, dir, size, mtime, mtime_ns, readonly FROM entries WHERE tape = ?1 AND parent = ?2")?;
        let rows = st.query_map(params![barcode, parent], entry_row)?;
        rows.collect::<Result<Vec<_>>>().map(Some)
    }

    /// The files at or below `path` of a tape: (path, size).
    pub fn files_under(&self, barcode: &str, path: &str) -> Result<Vec<(String, u64)>> {
        let p = ltfs::index::split(path).join("\\");
        if let Some(e) = self.entry(barcode, &p)? {
            if !e.dir {
                return Ok(vec![(p, e.size)]);
            }
        }
        let like = if p.is_empty() { "%".to_string() } else { format!("{}\\%", p.replace('!', "!!").replace('%', "!%").replace('_', "!_")) };
        let mut st = self.db.prepare_cached(
            "SELECT parent, name, size FROM entries WHERE tape = ?1 AND dir = 0 AND (parent = ?2 OR parent LIKE ?3 ESCAPE '!')",
        )?;
        let rows = st.query_map(params![barcode, p, like], |r| {
            let (parent, name, size): (String, String, i64) = (r.get(0)?, r.get(1)?, r.get(2)?);
            Ok((if parent.is_empty() { name } else { format!("{parent}\\{name}") }, size as u64))
        })?;
        rows.collect()
    }

    /// Files whose name contains `text` (ignoring ASCII case), on any tape:
    /// (barcode, path, size), at most `limit`.
    pub fn search(&self, text: &str, limit: usize) -> Result<Vec<(String, String, u64)>> {
        let like = format!("%{}%", text.replace('!', "!!").replace('%', "!%").replace('_', "!_"));
        let mut st = self.db.prepare("SELECT tape, parent, name, size FROM entries WHERE name LIKE ?1 ESCAPE '!' ORDER BY tape, parent, name LIMIT ?2")?;
        let rows = st.query_map(params![like, limit as i64], |r| {
            let (tape, parent, name, size): (String, String, String, i64) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
            Ok((tape, if parent.is_empty() { name } else { format!("{parent}\\{name}") }, size as u64))
        })?;
        rows.collect()
    }
}

fn entry_row(r: &rusqlite::Row) -> rusqlite::Result<Entry> {
    Ok(Entry {
        name: r.get(0)?,
        dir: r.get::<_, i64>(1)? != 0,
        size: r.get::<_, i64>(2)? as u64,
        modified: Time { secs: r.get(3)?, nanos: r.get::<_, i64>(4)? as u32 },
        readonly: r.get::<_, i64>(5)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ltfs::{File, Times};

    fn file(name: &str, len: u64) -> Node {
        Node::File(File { name: name.into(), length: len, readonly: false, open_for_write: false, times: Times::all(Time { secs: 100, nanos: 5 }), uid: 0, xattrs: vec![], extents: vec![], symlink: None, extra: vec![] })
    }

    #[test]
    fn import_and_browse() {
        let mut c = Catalog::in_memory().unwrap();
        let mut ix = Index::new("uuid-1", "t", "Vol");
        let now = Time::now();
        ix.put("photos\\2026", file("a_1.jpg", 10), now).unwrap();
        ix.put("photos", file("b.jpg", 20), now).unwrap();
        ix.put("", file("readme.txt", 3), now).unwrap();
        ix.make_dirs("empty", now).unwrap();
        ix.generation = 5;
        c.import("ABC123L5", &ix, Some(1000), Some(900), false).unwrap();

        let t = c.tape("abc123l5").unwrap().unwrap();
        assert_eq!((t.generation, t.files, t.bytes, t.name.as_str(), t.is_ltfs()), (5, 3, 33, "Vol", true));
        let mut root: Vec<String> = c.children("ABC123L5", "").unwrap().unwrap().into_iter().map(|e| e.name).collect();
        root.sort();
        assert_eq!(root, ["empty", "photos", "readme.txt"]);
        assert_eq!(c.children("ABC123L5", "PHOTOS").unwrap().unwrap().len(), 2);
        assert!(c.children("ABC123L5", "readme.txt").unwrap().is_none());
        assert!(c.children("ABC123L5", "nope").unwrap().is_none());
        assert_eq!(c.children("ABC123L5", "empty").unwrap().unwrap(), vec![]);
        let e = c.entry("ABC123L5", "photos\\2026\\a_1.jpg").unwrap().unwrap();
        assert_eq!((e.size, e.modified), (10, Time { secs: 100, nanos: 5 }));
        let mut under = c.files_under("ABC123L5", "photos").unwrap();
        under.sort();
        assert_eq!(under, vec![("photos\\2026\\a_1.jpg".to_string(), 10), ("photos\\b.jpg".to_string(), 20)]);
        assert_eq!(c.files_under("ABC123L5", "").unwrap().len(), 3);
        assert_eq!(c.search("A_1", 10).unwrap(), vec![("ABC123L5".to_string(), "photos\\2026\\a_1.jpg".to_string(), 10)]);
        assert_eq!(c.search("_", 10).unwrap().len(), 1, "_ is literal");

        // imported again: replaced
        ix.remove("photos", now);
        ix.generation = 6;
        c.import("ABC123L5", &ix, None, None, true).unwrap();
        let t = c.tape("ABC123L5").unwrap().unwrap();
        assert_eq!((t.generation, t.files, t.capacity, t.write_protected), (6, 1, Some(1000), true));
        assert!(c.children("ABC123L5", "photos").unwrap().is_none());

        c.set_locations(&[("ABC123L5".into(), "slot 3".into()), ("NEW001L5".into(), "mail slot".into())]).unwrap();
        assert_eq!(c.tape("NEW001L5").unwrap().unwrap().location, "mail slot");
        assert!(!c.tape("NEW001L5").unwrap().unwrap().is_ltfs());
        c.set_locations(&[]).unwrap();
        assert!(!c.tape("ABC123L5").unwrap().unwrap().in_library());
        c.set_note("ABC123L5", "offsite").unwrap();
        assert_eq!(c.tapes().unwrap().len(), 2);
        c.forget("ABC123L5").unwrap();
        assert_eq!(c.tapes().unwrap().len(), 1);
        assert!(c.files_under("ABC123L5", "").unwrap().is_empty());
    }
}
