//! A small streaming XML reader and writer helpers, enough for LTFS labels
//! and indexes: elements, attributes, text, entities, CDATA; comments,
//! processing instructions and the doctype are skipped. Elements the caller
//! doesn't know can be taken over raw ([`Reader::raw_element`]) and written
//! back as they were.

use std::borrow::Cow;

/// One step through the document.
#[derive(Debug, PartialEq, Eq)]
pub enum Tok<'a> {
    /// `<name attr="v">` (or `<name/>` when `empty`); `at` is where `<` is.
    Start { name: &'a str, attrs: Vec<(&'a str, String)>, empty: bool, at: usize },
    /// `</name>`; `end` is just after `>`.
    End { name: &'a str, end: usize },
    /// Text between tags, entities decoded.
    Text(Cow<'a, str>),
    Eof,
}

pub struct Reader<'a> {
    s: &'a str,
    pos: usize,
    /// The end tag of an empty element, given next.
    pending_end: Option<(&'a str, usize)>,
}

pub type Result<T> = std::result::Result<T, String>;

impl<'a> Reader<'a> {
    pub fn new(s: &'a str) -> Reader<'a> {
        let s = s.strip_prefix('\u{feff}').unwrap_or(s);
        Reader { s, pos: 0, pending_end: None }
    }

    fn err<T>(&self, what: &str) -> Result<T> {
        let line = self.s[..self.pos.min(self.s.len())].matches('\n').count() + 1;
        Err(format!("XML: {what} (line {line})"))
    }

    pub fn next(&mut self) -> Result<Tok<'a>> {
        if let Some((name, end)) = self.pending_end.take() {
            return Ok(Tok::End { name, end });
        }
        loop {
            let rest = &self.s[self.pos..];
            if rest.is_empty() {
                return Ok(Tok::Eof);
            }
            if !rest.starts_with('<') {
                let n = rest.find('<').unwrap_or(rest.len());
                let raw = &rest[..n];
                self.pos += n;
                return Ok(Tok::Text(decode(raw).map_err(|e| format!("XML: {e}"))?));
            }
            if rest.starts_with("<!--") {
                let Some(n) = rest.find("-->") else { return self.err("unterminated comment") };
                self.pos += n + 3;
                continue;
            }
            if rest.starts_with("<![CDATA[") {
                let Some(n) = rest.find("]]>") else { return self.err("unterminated CDATA") };
                let text = &rest[9..n];
                self.pos += n + 3;
                return Ok(Tok::Text(Cow::Borrowed(text)));
            }
            if rest.starts_with("<?") {
                let Some(n) = rest.find("?>") else { return self.err("unterminated processing instruction") };
                self.pos += n + 2;
                continue;
            }
            if rest.starts_with("<!") {
                let Some(n) = rest.find('>') else { return self.err("unterminated declaration") };
                self.pos += n + 1;
                continue;
            }
            if let Some(r) = rest.strip_prefix("</") {
                let Some(n) = r.find('>') else { return self.err("unterminated end tag") };
                let name = r[..n].trim();
                self.pos += 2 + n + 1;
                return Ok(Tok::End { name, end: self.pos });
            }
            // a start tag
            let at = self.pos;
            let Some(close) = find_tag_end(rest) else { return self.err("unterminated start tag") };
            let inner = &rest[1..close];
            let (inner, empty) = match inner.strip_suffix('/') {
                Some(i) => (i, true),
                None => (inner, false),
            };
            let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
            let name = &inner[..name_end];
            if name.is_empty() {
                return self.err("a tag without a name");
            }
            let attrs = parse_attrs(&inner[name_end..]).map_err(|e| format!("XML: {e} in <{name}>"))?;
            self.pos += close + 1;
            if empty {
                self.pending_end = Some((name, self.pos));
            }
            return Ok(Tok::Start { name, attrs, empty, at });
        }
    }

    /// The text of the element whose start tag was just read, up to its end
    /// tag (which is consumed). Fails on elements inside.
    pub fn text(&mut self, name: &str) -> Result<String> {
        let mut out = String::new();
        loop {
            match self.next()? {
                Tok::Text(t) => out.push_str(&t),
                Tok::End { name: n, .. } if n == name => return Ok(out),
                Tok::End { name: n, .. } => return self.err(&format!("</{n}> where </{name}> was expected")),
                Tok::Start { name: n, .. } => return self.err(&format!("<{n}> inside <{name}>")),
                Tok::Eof => return self.err(&format!("the document ends inside <{name}>")),
            }
        }
    }

    /// The element whose start tag (at `at`) was just read, as it is in the
    /// source, up to and with its end tag (consumed).
    pub fn raw_element(&mut self, name: &str, at: usize) -> Result<String> {
        let mut depth = 1;
        loop {
            match self.next()? {
                Tok::Start { name: n, .. } if n == name => depth += 1,
                Tok::End { name: n, end } if n == name => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(self.s[at..end].to_owned());
                    }
                }
                Tok::Eof => return self.err(&format!("the document ends inside <{name}>")),
                _ => {}
            }
        }
    }

    /// Skip what is left of the element `name` (its start tag just read).
    pub fn skip(&mut self, name: &str) -> Result<()> {
        let mut depth = 1;
        loop {
            match self.next()? {
                Tok::Start { name: n, .. } if n == name => depth += 1,
                Tok::End { name: n, .. } if n == name => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                Tok::Eof => return self.err(&format!("the document ends inside <{name}>")),
                _ => {}
            }
        }
    }
}

/// Where the `>` closing a start tag is (not inside a quoted value).
fn find_tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

fn parse_attrs(mut s: &str) -> std::result::Result<Vec<(&str, String)>, String> {
    let mut out = Vec::new();
    loop {
        s = s.trim_start();
        if s.is_empty() {
            return Ok(out);
        }
        let eq = s.find('=').ok_or("an attribute without a value")?;
        let name = s[..eq].trim();
        let rest = s[eq + 1..].trim_start();
        let q = rest.chars().next().filter(|c| *c == '"' || *c == '\'').ok_or("an unquoted attribute")?;
        let end = rest[1..].find(q).ok_or("an unterminated attribute")?;
        out.push((name, decode(&rest[1..1 + end])?.into_owned()));
        s = &rest[end + 2..];
    }
}

/// Decode entities.
pub fn decode(s: &str) -> std::result::Result<Cow<'_, str>, String> {
    if !s.contains('&') {
        return Ok(Cow::Borrowed(s));
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let r = &rest[i + 1..];
        let end = r.find(';').ok_or("an unterminated entity")?;
        let ent = &r[..end];
        let c = match ent {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ if ent.starts_with("#x") || ent.starts_with("#X") => u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32).ok_or("a bad character reference")?,
            _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32).ok_or("a bad character reference")?,
            _ => return Err(format!("an unknown entity &{ent};")),
        };
        out.push(c);
        rest = &r[end + 1..];
    }
    out.push_str(rest);
    Ok(Cow::Owned(out))
}

/// Escape text for an element's content or an attribute value.
pub fn escape(s: &str) -> Cow<'_, str> {
    if !s.contains(['<', '>', '&', '"', '\r']) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Writes indented XML.
pub struct Writer {
    pub out: String,
    depth: usize,
}

impl Writer {
    pub fn new() -> Writer {
        Writer { out: String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"), depth: 0 }
    }

    fn indent(&mut self) {
        for _ in 0..self.depth {
            self.out.push_str("    ");
        }
    }

    /// `<name attrs>`, then a new level.
    pub fn open(&mut self, name: &str, attrs: &[(&str, &str)]) {
        self.indent();
        self.out.push('<');
        self.out.push_str(name);
        for (k, v) in attrs {
            self.out.push(' ');
            self.out.push_str(k);
            self.out.push_str("=\"");
            self.out.push_str(&escape(v));
            self.out.push('"');
        }
        self.out.push_str(">\n");
        self.depth += 1;
    }

    pub fn close(&mut self, name: &str) {
        self.depth -= 1;
        self.indent();
        self.out.push_str("</");
        self.out.push_str(name);
        self.out.push_str(">\n");
    }

    /// `<name attrs>text</name>`.
    pub fn leaf_with(&mut self, name: &str, attrs: &[(&str, &str)], text: &str) {
        self.indent();
        self.out.push('<');
        self.out.push_str(name);
        for (k, v) in attrs {
            self.out.push(' ');
            self.out.push_str(k);
            self.out.push_str("=\"");
            self.out.push_str(&escape(v));
            self.out.push('"');
        }
        self.out.push('>');
        self.out.push_str(&escape(text));
        self.out.push_str("</");
        self.out.push_str(name);
        self.out.push_str(">\n");
    }

    pub fn leaf(&mut self, name: &str, text: &str) {
        self.leaf_with(name, &[], text);
    }

    /// An element kept as it was read.
    pub fn raw(&mut self, xml: &str) {
        self.indent();
        self.out.push_str(xml.trim());
        self.out.push('\n');
    }
}

impl Default for Writer {
    fn default() -> Writer {
        Writer::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_ltfs_writes() {
        let doc = "<?xml version=\"1.0\"?>\n<!-- c --><a x='1' y=\"a&amp;b\"><b>t&lt;1 &#x10D;</b><c/><d><e>raw</e></d><![CDATA[<z>]]></a>";
        let mut r = Reader::new(doc);
        assert_eq!(r.next().unwrap(), Tok::Text(Cow::Borrowed("\n")));
        let Tok::Start { name: "a", attrs, .. } = r.next().unwrap() else { panic!() };
        assert_eq!(attrs, vec![("x", "1".to_string()), ("y", "a&b".to_string())]);
        assert!(matches!(r.next().unwrap(), Tok::Start { name: "b", .. }));
        assert_eq!(r.text("b").unwrap(), "t<1 č");
        assert!(matches!(r.next().unwrap(), Tok::Start { name: "c", empty: true, .. }));
        assert!(matches!(r.next().unwrap(), Tok::End { name: "c", .. }));
        let Tok::Start { name: "d", at, .. } = r.next().unwrap() else { panic!() };
        assert_eq!(r.raw_element("d", at).unwrap(), "<d><e>raw</e></d>");
        assert_eq!(r.next().unwrap(), Tok::Text(Cow::Borrowed("<z>")));
        assert!(matches!(r.next().unwrap(), Tok::End { name: "a", .. }));
        assert_eq!(r.next().unwrap(), Tok::Eof);
    }
}
