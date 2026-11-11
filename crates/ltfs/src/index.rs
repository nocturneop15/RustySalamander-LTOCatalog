//! The LTFS index: the volume's tree of folders and files with the extents
//! where each file's data is on tape. Read from XML, changed, written back.
//! Elements this crate doesn't know are kept and written back as they were.

use crate::time::Time;
use crate::xml::{Reader, Tok, Writer};

/// Where an index is: partition letter and block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Location {
    pub partition: char,
    pub block: u64,
}

/// The five times of a file or folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Times {
    pub creation: Time,
    pub change: Time,
    pub modify: Time,
    pub access: Time,
    pub backup: Option<Time>,
}

impl Times {
    pub fn all(t: Time) -> Times {
        Times { creation: t, change: t, modify: t, access: t, backup: None }
    }
}

/// An extended attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xattr {
    pub key: String,
    pub value: String,
    /// `type="base64"`: the value is base64 text.
    pub base64: bool,
}

/// A piece of a file on tape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    /// Where in the file the piece starts.
    pub file_offset: u64,
    pub partition: char,
    pub start_block: u64,
    /// Where in the first block the piece starts.
    pub byte_offset: u64,
    pub byte_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub name: String,
    pub length: u64,
    pub readonly: bool,
    pub open_for_write: bool,
    pub times: Times,
    pub uid: u64,
    pub xattrs: Vec<Xattr>,
    pub extents: Vec<Extent>,
    /// A symbolic link's target (then it has no data).
    pub symlink: Option<String>,
    pub extra: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dir {
    pub name: String,
    pub readonly: bool,
    pub times: Times,
    pub uid: u64,
    pub xattrs: Vec<Xattr>,
    pub children: Vec<Node>,
    pub extra: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Dir(Dir),
    File(File),
}

impl Node {
    pub fn name(&self) -> &str {
        match self {
            Node::Dir(d) => &d.name,
            Node::File(f) => &f.name,
        }
    }
    pub fn times(&self) -> &Times {
        match self {
            Node::Dir(d) => &d.times,
            Node::File(f) => &f.times,
        }
    }
}

/// An index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    pub version: String,
    pub creator: String,
    pub comment: Option<String>,
    pub volume_uuid: String,
    pub generation: u64,
    pub update_time: Time,
    pub location: Location,
    pub previous: Option<Location>,
    pub allow_policy_update: Option<bool>,
    /// `<dataplacementpolicy>` as it was.
    pub policy: Option<String>,
    pub volume_lock_state: Option<String>,
    pub highest_file_uid: u64,
    pub root: Dir,
    pub extra: Vec<String>,
}

impl Index {
    /// The index of a new, empty volume.
    pub fn new(uuid: &str, creator: &str, name: &str) -> Index {
        let now = Time::now();
        Index {
            version: crate::label::VERSION.into(),
            creator: creator.into(),
            comment: None,
            volume_uuid: uuid.into(),
            generation: 1,
            update_time: now,
            location: Location { partition: 'b', block: 0 },
            previous: None,
            allow_policy_update: Some(true),
            policy: None,
            volume_lock_state: None,
            highest_file_uid: 1,
            root: Dir { name: name.into(), times: Times::all(now), uid: 1, ..Dir::default() },
            extra: Vec::new(),
        }
    }

    /// A new file uid.
    pub fn next_uid(&mut self) -> u64 {
        self.highest_file_uid += 1;
        self.highest_file_uid
    }

    /// The node at `path` (`\` or `/` separated, no leading separator; ""
    /// is the root, returned as `None` here: use [`Index::root`]). Names are
    /// matched exactly, else ignoring case.
    pub fn find(&self, path: &str) -> Option<&Node> {
        let parts = split(path);
        let (last, dirs) = parts.split_last()?;
        let mut d = &self.root;
        for p in dirs {
            match child(&d.children, p)? {
                Node::Dir(sub) => d = sub,
                Node::File(_) => return None,
            }
        }
        child(&d.children, last)
    }

    /// The folder at `path` ("" is the root).
    pub fn dir(&self, path: &str) -> Option<&Dir> {
        if split(path).is_empty() {
            return Some(&self.root);
        }
        match self.find(path)? {
            Node::Dir(d) => Some(d),
            Node::File(_) => None,
        }
    }

    pub fn dir_mut(&mut self, path: &str) -> Option<&mut Dir> {
        let mut d = &mut self.root;
        for p in split(path) {
            let i = child_at(&d.children, p)?;
            match &mut d.children[i] {
                Node::Dir(sub) => d = sub,
                Node::File(_) => return None,
            }
        }
        Some(d)
    }

    /// The folder at `path`, made with its parents if needed. Fails when a
    /// file is in the way.
    pub fn make_dirs(&mut self, path: &str, now: Time) -> Result<&mut Dir, String> {
        let parts: Vec<String> = split(path).into_iter().map(str::to_owned).collect();
        let mut uid = self.highest_file_uid;
        let mut d = &mut self.root;
        for p in &parts {
            let i = match child_at(&d.children, p) {
                Some(i) => i,
                None => {
                    uid += 1;
                    d.children.push(Node::Dir(Dir { name: p.clone(), times: Times::all(now), uid, ..Dir::default() }));
                    d.times.modify = now;
                    d.times.change = now;
                    d.children.len() - 1
                }
            };
            match &mut d.children[i] {
                Node::Dir(sub) => d = sub,
                Node::File(f) => return Err(format!("\"{}\" is a file", f.name)),
            }
        }
        self.highest_file_uid = self.highest_file_uid.max(uid);
        // borrow again: the loop above moved `d` out of self
        Ok(self.dir_mut(path).expect("just made"))
    }

    /// Put `node` into the folder `dir` (made if needed), replacing an item
    /// of the same name (ignoring case). The replaced item comes back.
    pub fn put(&mut self, dir: &str, node: Node, now: Time) -> Result<Option<Node>, String> {
        let d = self.make_dirs(dir, now)?;
        d.times.modify = now;
        d.times.change = now;
        match child_at(&d.children, node.name()) {
            Some(i) => Ok(Some(std::mem::replace(&mut d.children[i], node))),
            None => {
                d.children.push(node);
                Ok(None)
            }
        }
    }

    /// Remove the item at `path`.
    pub fn remove(&mut self, path: &str, now: Time) -> Option<Node> {
        let parts = split(path);
        let (last, dirs) = parts.split_last()?;
        let d = self.dir_mut(&dirs.join("\\"))?;
        let i = child_at(&d.children, last)?;
        d.times.modify = now;
        d.times.change = now;
        Some(d.children.remove(i))
    }

    /// Move or rename `old` to `new`.
    pub fn rename(&mut self, old: &str, new: &str, now: Time) -> Result<(), String> {
        let new_parts = split(new);
        let Some((new_name, new_dirs)) = new_parts.split_last() else { return Err("no new name".into()) };
        if self.find(old).is_none() {
            return Err(format!("\"{old}\" doesn't exist"));
        }
        let same = split(old).iter().map(|s| s.to_lowercase()).eq(new_parts.iter().map(|s| s.to_lowercase()));
        if !same && self.find(new).is_some() {
            return Err(format!("\"{new}\" already exists"));
        }
        // a folder into itself
        let lo = split(old).join("\\").to_lowercase();
        let ln = new_parts.join("\\").to_lowercase();
        if ln.starts_with(&format!("{lo}\\")) {
            return Err("a folder can't be moved into itself".into());
        }
        let parent = new_dirs.join("\\");
        if self.dir(&parent).is_none() {
            return Err(format!("\"{parent}\" isn't a folder"));
        }
        let mut node = self.remove(old, now).expect("found above");
        match &mut node {
            Node::Dir(d) => {
                d.name = (*new_name).to_owned();
                d.times.change = now;
            }
            Node::File(f) => {
                f.name = (*new_name).to_owned();
                f.times.change = now;
            }
        }
        self.put(&parent, node, now)?;
        Ok(())
    }

    /// Every item, depth first, with its path (`\`-separated).
    pub fn walk(&self, f: &mut dyn FnMut(&str, &Node)) {
        fn go(d: &Dir, prefix: &str, f: &mut dyn FnMut(&str, &Node)) {
            for c in &d.children {
                let p = if prefix.is_empty() { c.name().to_owned() } else { format!("{prefix}\\{}", c.name()) };
                f(&p, c);
                if let Node::Dir(sub) = c {
                    go(sub, &p, f);
                }
            }
        }
        go(&self.root, "", f);
    }

    /// Whether any file has data in the given partition.
    pub fn has_data_in(&self, partition: char) -> bool {
        let mut found = false;
        self.walk(&mut |_, n| {
            if let Node::File(f) = n {
                found |= f.extents.iter().any(|e| e.partition == partition);
            }
        });
        found
    }

    /// Number of files and their total size.
    pub fn totals(&self) -> (u64, u64) {
        let (mut n, mut size) = (0, 0);
        self.walk(&mut |_, node| {
            if let Node::File(f) = node {
                n += 1;
                size += f.length;
            }
        });
        (n, size)
    }

    // ---- XML ----

    pub fn to_xml(&self) -> String {
        let mut w = Writer::new();
        w.open("ltfsindex", &[("version", &self.version)]);
        w.leaf("creator", &self.creator);
        if let Some(c) = &self.comment {
            w.leaf("comment", c);
        }
        w.leaf("volumeuuid", &self.volume_uuid);
        w.leaf("generationnumber", &self.generation.to_string());
        w.leaf("updatetime", &self.update_time.format());
        write_location(&mut w, "location", &self.location);
        if let Some(p) = &self.previous {
            write_location(&mut w, "previousgenerationlocation", p);
        }
        if let Some(a) = self.allow_policy_update {
            w.leaf("allowpolicyupdate", if a { "true" } else { "false" });
        }
        if let Some(p) = &self.policy {
            w.raw(p);
        }
        if let Some(s) = &self.volume_lock_state {
            w.leaf("volumelockstate", s);
        }
        w.leaf("highestfileuid", &self.highest_file_uid.to_string());
        write_dir(&mut w, &self.root);
        for e in &self.extra {
            w.raw(e);
        }
        w.close("ltfsindex");
        w.out
    }

    pub fn parse(xml: &str) -> Result<Index, String> {
        let mut r = Reader::new(xml);
        let version = loop {
            match r.next()? {
                Tok::Start { name: "ltfsindex", attrs, .. } => break attrs.into_iter().find(|(k, _)| *k == "version").map(|(_, v)| v).unwrap_or_default(),
                Tok::Start { name, .. } => return Err(format!("not an LTFS index (<{name}>)")),
                Tok::Eof => return Err("an empty LTFS index".into()),
                _ => {}
            }
        };
        let major = version.split('.').next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
        if major != 2 && major != 1 {
            return Err(format!("unsupported LTFS index version {version}"));
        }
        let mut ix = Index::new("", "", "");
        ix.version = version;
        ix.allow_policy_update = None;
        ix.highest_file_uid = 0;
        let mut root = None;
        loop {
            match r.next()? {
                Tok::Start { name, at, empty, .. } => match name {
                    "creator" => ix.creator = r.text(name)?,
                    "comment" => ix.comment = Some(r.text(name)?),
                    "volumeuuid" => ix.volume_uuid = r.text(name)?.trim().to_owned(),
                    "generationnumber" => ix.generation = number(&r.text(name)?, name)?,
                    "updatetime" => ix.update_time = time(&r.text(name)?)?,
                    "location" => ix.location = parse_location(&mut r, name)?,
                    "previousgenerationlocation" => ix.previous = Some(parse_location(&mut r, name)?),
                    "allowpolicyupdate" => ix.allow_policy_update = Some(r.text(name)?.trim() == "true"),
                    "volumelockstate" => ix.volume_lock_state = Some(r.text(name)?.trim().to_owned()),
                    "highestfileuid" => ix.highest_file_uid = number(&r.text(name)?, name)?,
                    "directory" if !empty => root = Some(parse_dir(&mut r)?),
                    "dataplacementpolicy" => ix.policy = Some(r.raw_element(name, at)?),
                    _ => ix.extra.push(r.raw_element(name, at)?),
                },
                Tok::End { name: "ltfsindex", .. } | Tok::Eof => break,
                _ => {}
            }
        }
        ix.root = root.ok_or("the LTFS index has no root folder")?;
        if ix.volume_uuid.is_empty() {
            return Err("the LTFS index has no volume UUID".into());
        }
        // indexes written by version 1 had no highest uid
        if ix.highest_file_uid == 0 {
            let mut m = ix.root.uid;
            ix.walk(&mut |_, n| {
                m = m.max(match n {
                    Node::Dir(d) => d.uid,
                    Node::File(f) => f.uid,
                })
            });
            ix.highest_file_uid = m.max(1);
        }
        Ok(ix)
    }
}

/// The parts of a path.
pub fn split(path: &str) -> Vec<&str> {
    path.split(['\\', '/']).filter(|p| !p.is_empty()).collect()
}

fn child_at(children: &[Node], name: &str) -> Option<usize> {
    children.iter().position(|c| c.name() == name).or_else(|| {
        let l = name.to_lowercase();
        children.iter().position(|c| c.name().to_lowercase() == l)
    })
}

fn child<'a>(children: &'a [Node], name: &str) -> Option<&'a Node> {
    child_at(children, name).map(|i| &children[i])
}

fn number(s: &str, what: &str) -> Result<u64, String> {
    s.trim().parse().map_err(|_| format!("bad <{what}> \"{s}\""))
}

fn time(s: &str) -> Result<Time, String> {
    Time::parse(s).ok_or_else(|| format!("bad time \"{s}\""))
}

fn partition(s: &str) -> Result<char, String> {
    s.trim().chars().next().filter(|c| c.is_ascii_lowercase()).ok_or_else(|| format!("bad partition \"{s}\""))
}

fn parse_location(r: &mut Reader, outer: &str) -> Result<Location, String> {
    let mut l = Location::default();
    loop {
        match r.next()? {
            Tok::Start { name: "partition", .. } => l.partition = partition(&r.text("partition")?)?,
            Tok::Start { name: "startblock", .. } => l.block = number(&r.text("startblock")?, "startblock")?,
            Tok::Start { name, .. } => r.skip(name)?,
            Tok::End { name, .. } if name == outer => return Ok(l),
            Tok::Eof => return Err(format!("the index ends inside <{outer}>")),
            _ => {}
        }
    }
}

/// A name, percent-decoded when the element says so.
fn parse_name(r: &mut Reader, attrs: &[(&str, String)]) -> Result<String, String> {
    let text = r.text("name")?;
    if attrs.iter().any(|(k, v)| *k == "percentencoded" && v == "true") {
        let b = text.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' && i + 2 < b.len() {
                let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                if let Some(v) = h {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
            out.push(b[i]);
            i += 1;
        }
        return String::from_utf8(out).map_err(|_| format!("a name that isn't UTF-8: \"{text}\""));
    }
    Ok(text)
}

fn parse_times(r: &mut Reader, name: &str, t: &mut Times) -> Result<bool, String> {
    let slot = match name {
        "creationtime" => &mut t.creation,
        "changetime" => &mut t.change,
        "modifytime" => &mut t.modify,
        "accesstime" => &mut t.access,
        "backuptime" => {
            t.backup = Some(time(&r.text(name)?)?);
            return Ok(true);
        }
        _ => return Ok(false),
    };
    *slot = time(&r.text(name)?)?;
    Ok(true)
}

fn parse_xattrs(r: &mut Reader) -> Result<Vec<Xattr>, String> {
    let mut out = Vec::new();
    loop {
        match r.next()? {
            Tok::Start { name: "xattr", .. } => {
                let mut x = Xattr { key: String::new(), value: String::new(), base64: false };
                loop {
                    match r.next()? {
                        Tok::Start { name: "key", .. } => x.key = r.text("key")?,
                        Tok::Start { name: "value", attrs, empty, .. } => {
                            x.base64 = attrs.iter().any(|(k, v)| *k == "type" && v == "base64");
                            x.value = r.text("value")?;
                            let _ = empty;
                        }
                        Tok::Start { name, .. } => r.skip(name)?,
                        Tok::End { name: "xattr", .. } => break,
                        Tok::Eof => return Err("the index ends inside <xattr>".into()),
                        _ => {}
                    }
                }
                out.push(x);
            }
            Tok::Start { name, .. } => r.skip(name)?,
            Tok::End { name: "extendedattributes", .. } => return Ok(out),
            Tok::Eof => return Err("the index ends inside <extendedattributes>".into()),
            _ => {}
        }
    }
}

fn parse_extents(r: &mut Reader) -> Result<Vec<Extent>, String> {
    let mut out = Vec::new();
    loop {
        match r.next()? {
            Tok::Start { name: "extent", .. } => {
                let mut e = Extent { file_offset: 0, partition: 'b', start_block: 0, byte_offset: 0, byte_count: 0 };
                loop {
                    match r.next()? {
                        Tok::Start { name, .. } => match name {
                            "fileoffset" => e.file_offset = number(&r.text(name)?, name)?,
                            "partition" => e.partition = partition(&r.text(name)?)?,
                            "startblock" => e.start_block = number(&r.text(name)?, name)?,
                            "byteoffset" => e.byte_offset = number(&r.text(name)?, name)?,
                            "bytecount" => e.byte_count = number(&r.text(name)?, name)?,
                            _ => r.skip(name)?,
                        },
                        Tok::End { name: "extent", .. } => break,
                        Tok::Eof => return Err("the index ends inside <extent>".into()),
                        _ => {}
                    }
                }
                out.push(e);
            }
            Tok::Start { name, .. } => r.skip(name)?,
            Tok::End { name: "extentinfo", .. } => return Ok(out),
            Tok::Eof => return Err("the index ends inside <extentinfo>".into()),
            _ => {}
        }
    }
}

fn parse_dir(r: &mut Reader) -> Result<Dir, String> {
    let mut d = Dir::default();
    loop {
        match r.next()? {
            Tok::Start { name, attrs, at, empty } => {
                if parse_times(r, name, &mut d.times)? {
                    continue;
                }
                match name {
                    "name" => d.name = parse_name(r, &attrs)?,
                    "readonly" => d.readonly = r.text(name)?.trim() == "true",
                    "fileuid" => d.uid = number(&r.text(name)?, name)?,
                    "extendedattributes" if !empty => d.xattrs = parse_xattrs(r)?,
                    "contents" if !empty => parse_contents(r, &mut d.children)?,
                    "contents" | "extendedattributes" => r.skip(name)?,
                    _ => d.extra.push(r.raw_element(name, at)?),
                }
            }
            Tok::End { name: "directory", .. } => return Ok(d),
            Tok::Eof => return Err("the index ends inside <directory>".into()),
            _ => {}
        }
    }
}

fn parse_contents(r: &mut Reader, out: &mut Vec<Node>) -> Result<(), String> {
    loop {
        match r.next()? {
            Tok::Start { name: "directory", .. } => out.push(Node::Dir(parse_dir(r)?)),
            Tok::Start { name: "file", .. } => out.push(Node::File(parse_file(r)?)),
            Tok::Start { name, .. } => r.skip(name)?,
            Tok::End { name: "contents", .. } => return Ok(()),
            Tok::Eof => return Err("the index ends inside <contents>".into()),
            _ => {}
        }
    }
}

fn parse_file(r: &mut Reader) -> Result<File, String> {
    let mut f = File {
        name: String::new(),
        length: 0,
        readonly: false,
        open_for_write: false,
        times: Times::default(),
        uid: 0,
        xattrs: Vec::new(),
        extents: Vec::new(),
        symlink: None,
        extra: Vec::new(),
    };
    loop {
        match r.next()? {
            Tok::Start { name, attrs, at, empty } => {
                if parse_times(r, name, &mut f.times)? {
                    continue;
                }
                match name {
                    "name" => f.name = parse_name(r, &attrs)?,
                    "length" => f.length = number(&r.text(name)?, name)?,
                    "readonly" => f.readonly = r.text(name)?.trim() == "true",
                    "openforwrite" => f.open_for_write = r.text(name)?.trim() == "true",
                    "fileuid" => f.uid = number(&r.text(name)?, name)?,
                    "symlink" => f.symlink = Some(r.text(name)?),
                    "extendedattributes" if !empty => f.xattrs = parse_xattrs(r)?,
                    "extentinfo" if !empty => f.extents = parse_extents(r)?,
                    "extendedattributes" | "extentinfo" => r.skip(name)?,
                    _ => f.extra.push(r.raw_element(name, at)?),
                }
            }
            Tok::End { name: "file", .. } => {
                f.extents.sort_by_key(|e| e.file_offset);
                return Ok(f);
            }
            Tok::Eof => return Err("the index ends inside <file>".into()),
            _ => {}
        }
    }
}

fn write_location(w: &mut Writer, tag: &str, l: &Location) {
    w.open(tag, &[]);
    w.leaf("partition", &l.partition.to_string());
    w.leaf("startblock", &l.block.to_string());
    w.close(tag);
}

/// A name, percent-encoded when it has characters XML can't carry.
fn write_name(w: &mut Writer, name: &str) {
    let bad = |c: char| (c as u32) < 0x20 && c != '\t' || c == '\u{7f}' || ('\u{fffe}'..='\u{ffff}').contains(&c);
    if name.chars().any(bad) {
        let mut out = String::new();
        for c in name.chars() {
            if bad(c) || c == '%' {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            } else {
                out.push(c);
            }
        }
        w.leaf_with("name", &[("percentencoded", "true")], &out);
    } else {
        w.leaf("name", name);
    }
}

fn write_times(w: &mut Writer, t: &Times) {
    w.leaf("creationtime", &t.creation.format());
    w.leaf("changetime", &t.change.format());
    w.leaf("modifytime", &t.modify.format());
    w.leaf("accesstime", &t.access.format());
    if let Some(b) = &t.backup {
        w.leaf("backuptime", &b.format());
    }
}

fn write_xattrs(w: &mut Writer, x: &[Xattr]) {
    if x.is_empty() {
        return;
    }
    w.open("extendedattributes", &[]);
    for a in x {
        w.open("xattr", &[]);
        w.leaf("key", &a.key);
        w.leaf_with("value", &[("type", if a.base64 { "base64" } else { "text" })], &a.value);
        w.close("xattr");
    }
    w.close("extendedattributes");
}

fn write_dir(w: &mut Writer, d: &Dir) {
    w.open("directory", &[]);
    write_name(w, &d.name);
    w.leaf("readonly", if d.readonly { "true" } else { "false" });
    write_times(w, &d.times);
    w.leaf("fileuid", &d.uid.to_string());
    write_xattrs(w, &d.xattrs);
    for e in &d.extra {
        w.raw(e);
    }
    w.open("contents", &[]);
    for c in &d.children {
        match c {
            Node::Dir(sub) => write_dir(w, sub),
            Node::File(f) => write_file(w, f),
        }
    }
    w.close("contents");
    w.close("directory");
}

fn write_file(w: &mut Writer, f: &File) {
    w.open("file", &[]);
    write_name(w, &f.name);
    w.leaf("length", &f.length.to_string());
    w.leaf("readonly", if f.readonly { "true" } else { "false" });
    if f.open_for_write {
        w.leaf("openforwrite", "true");
    }
    write_times(w, &f.times);
    w.leaf("fileuid", &f.uid.to_string());
    write_xattrs(w, &f.xattrs);
    if let Some(s) = &f.symlink {
        w.leaf("symlink", s);
    } else if !f.extents.is_empty() {
        w.open("extentinfo", &[]);
        for e in &f.extents {
            w.open("extent", &[]);
            w.leaf("fileoffset", &e.file_offset.to_string());
            w.leaf("partition", &e.partition.to_string());
            w.leaf("startblock", &e.start_block.to_string());
            w.leaf("byteoffset", &e.byte_offset.to_string());
            w.leaf("bytecount", &e.byte_count.to_string());
            w.close("extent");
        }
        w.close("extentinfo");
    }
    for e in &f.extra {
        w.raw(e);
    }
    w.close("file");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An index as IBM's LTFS writes it, with elements this crate doesn't know.
    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ltfsindex version="2.4.0">
	<creator>IBM LTFS 2.4.4.0 (Build 10) - Windows - ltfs.exe</creator>
	<volumeuuid>6d2f6a2b-6f59-4a0e-9b5a-1f4e2a4c9d11</volumeuuid>
	<generationnumber>7</generationnumber>
	<updatetime>2026-10-01T08:15:30.123456789Z</updatetime>
	<location>
		<partition>b</partition>
		<startblock>2045</startblock>
	</location>
	<previousgenerationlocation>
		<partition>b</partition>
		<startblock>1210</startblock>
	</previousgenerationlocation>
	<allowpolicyupdate>true</allowpolicyupdate>
	<dataplacementpolicy>
		<indexpartitioncriteria>
			<size>1048576</size>
			<name>*.txt</name>
		</indexpartitioncriteria>
	</dataplacementpolicy>
	<volumelockstate>unlocked</volumelockstate>
	<highestfileuid>5</highestfileuid>
	<directory>
		<name>Archive 2026</name>
		<readonly>false</readonly>
		<creationtime>2026-09-01T00:00:00.000000000Z</creationtime>
		<changetime>2026-10-01T08:15:30.000000000Z</changetime>
		<modifytime>2026-10-01T08:15:30.000000000Z</modifytime>
		<accesstime>2026-10-01T08:15:30.000000000Z</accesstime>
		<backuptime>2026-09-01T00:00:00.000000000Z</backuptime>
		<fileuid>1</fileuid>
		<contents>
			<directory>
				<name>fotky &amp; videa</name>
				<readonly>false</readonly>
				<creationtime>2026-09-01T00:00:00.000000000Z</creationtime>
				<changetime>2026-09-01T00:00:00.000000000Z</changetime>
				<modifytime>2026-09-01T00:00:00.000000000Z</modifytime>
				<accesstime>2026-09-01T00:00:00.000000000Z</accesstime>
				<fileuid>2</fileuid>
				<contents>
					<file>
						<name>léto 🦎.jpg</name>
						<length>1300000</length>
						<readonly>false</readonly>
						<creationtime>2026-09-01T00:00:00.000000000Z</creationtime>
						<changetime>2026-09-01T00:00:00.000000000Z</changetime>
						<modifytime>2026-08-15T12:00:00.000000000Z</modifytime>
						<accesstime>2026-09-01T00:00:00.000000000Z</accesstime>
						<backuptime>2026-09-01T00:00:00.000000000Z</backuptime>
						<fileuid>3</fileuid>
						<extendedattributes>
							<xattr>
								<key>ltfs.vendor.IBM.fileattribute</key>
								<value type="text">32</value>
							</xattr>
						</extendedattributes>
						<extentinfo>
							<extent>
								<fileoffset>0</fileoffset>
								<partition>b</partition>
								<startblock>1000</startblock>
								<byteoffset>0</byteoffset>
								<bytecount>1048576</bytecount>
							</extent>
							<extent>
								<fileoffset>1048576</fileoffset>
								<partition>b</partition>
								<startblock>1002</startblock>
								<byteoffset>0</byteoffset>
								<bytecount>251424</bytecount>
							</extent>
						</extentinfo>
						<vendorthing a="1">kept</vendorthing>
					</file>
				</contents>
			</directory>
			<file>
				<name percentencoded="true">bad%0Aname%25</name>
				<length>0</length>
				<readonly>true</readonly>
				<creationtime>2026-09-01T00:00:00Z</creationtime>
				<changetime>2026-09-01T00:00:00Z</changetime>
				<modifytime>2026-09-01T00:00:00Z</modifytime>
				<accesstime>2026-09-01T00:00:00Z</accesstime>
				<fileuid>4</fileuid>
			</file>
			<file>
				<name>link</name>
				<length>0</length>
				<readonly>false</readonly>
				<creationtime>2026-09-01T00:00:00Z</creationtime>
				<changetime>2026-09-01T00:00:00Z</changetime>
				<modifytime>2026-09-01T00:00:00Z</modifytime>
				<accesstime>2026-09-01T00:00:00Z</accesstime>
				<fileuid>5</fileuid>
				<symlink>fotky &amp; videa/léto 🦎.jpg</symlink>
			</file>
		</contents>
	</directory>
	<unknownvolumething>stays</unknownvolumething>
</ltfsindex>
"#;

    #[test]
    fn reads_an_ibm_index_and_writes_it_back_the_same() {
        let ix = Index::parse(SAMPLE).unwrap();
        assert_eq!(ix.generation, 7);
        assert_eq!(ix.location, Location { partition: 'b', block: 2045 });
        assert_eq!(ix.previous, Some(Location { partition: 'b', block: 1210 }));
        assert_eq!(ix.root.name, "Archive 2026");
        let Some(Node::File(f)) = ix.find("FOTKY & videa\\léto 🦎.jpg") else { panic!() };
        assert_eq!(f.length, 1_300_000);
        assert_eq!(f.extents.len(), 2);
        assert_eq!(f.extents[1].start_block, 1002);
        assert_eq!(f.extra, vec![r#"<vendorthing a="1">kept</vendorthing>"#.to_string()]);
        assert_eq!(f.xattrs[0].value, "32");
        assert!(ix.find("bad\nname%").is_some());
        let Some(Node::File(l)) = ix.find("link") else { panic!() };
        assert_eq!(l.symlink.as_deref(), Some("fotky & videa/léto 🦎.jpg"));
        assert_eq!(ix.extra.len(), 1);
        assert!(ix.policy.as_deref().unwrap().contains("*.txt"));
        assert_eq!(ix.totals(), (3, 1_300_000));
        // written and read again: the same
        let again = Index::parse(&ix.to_xml()).unwrap();
        assert_eq!(again, ix);
    }

    #[test]
    fn changes() {
        let mut ix = Index::new("u", "c", "vol");
        let now = Time::now();
        let file = |name: &str| {
            Node::File(File {
                name: name.into(),
                length: 1,
                readonly: false,
                open_for_write: false,
                times: Times::all(now),
                uid: 0,
                xattrs: vec![],
                extents: vec![],
                symlink: None,
                extra: vec![],
            })
        };
        assert!(ix.put("a\\b", file("x.txt"), now).unwrap().is_none());
        assert!(ix.put("A\\B", file("X.TXT"), now).unwrap().is_some(), "replaced, ignoring case");
        assert_eq!(ix.dir("a\\b").unwrap().children.len(), 1);
        ix.rename("a\\b\\X.TXT", "a\\y.txt", now).unwrap();
        assert!(ix.find("a\\y.txt").is_some() && ix.find("a\\b\\X.TXT").is_none());
        assert!(ix.rename("a", "a\\b\\c", now).is_err());
        assert!(ix.put("a\\y.txt", file("z"), now).is_err(), "a file in the way");
        assert!(ix.remove("a\\b", now).is_some());
        assert_eq!(ix.root.children.len(), 1);
        assert!(ix.highest_file_uid >= 3);
    }
}
