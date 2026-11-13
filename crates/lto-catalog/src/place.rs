//! Where the plugin keeps its settings and its catalog, so that `ltocat`
//! finds the same files.
//!
//! The plugin saves in `sal_plugin_api::data_dir(<plugin folder>)`: its own
//! folder while RustySalamander is portable, or
//! `%APPDATA%\RustySalamander\plugins\ltocatalog` once the program was
//! switched to multi-user mode (`multiuser.toml` next to the program).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sal_plugin_api::ui::{decode, encode};

/// The plugin's folder name below the program's `plugins`.
pub const PLUGIN_FOLDER: &str = "ltocatalog";
/// The program, by which its folder is recognised.
pub const PROGRAM_EXE: &str = "rustysalamander.exe";
/// Next to the program: settings live in each user's `%APPDATA%`.
pub const MULTI_USER_FILE: &str = "multiuser.toml";

/// The plugin's settings (Plugins → Library settings…).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// The changer's device; empty: found.
    pub changer: String,
    /// Tape drives, comma-separated; empty: all.
    pub drives: String,
    /// The catalog file; empty: [`default_catalog`].
    pub catalog: String,
}

impl Settings {
    /// The settings in `file`, or the defaults.
    pub fn load(file: &Path) -> Settings {
        let mut s = Settings::default();
        let Ok(text) = fs::read_to_string(file) else { return s };
        let Some(parts) = decode(&text) else { return s };
        for kv in parts.chunks(2) {
            if let [k, v] = kv {
                match k.as_str() {
                    "changer" => s.changer = v.clone(),
                    "drives" => s.drives = v.clone(),
                    "catalog" => s.catalog = v.clone(),
                    _ => {}
                }
            }
        }
        s
    }

    pub fn save(&self, file: &Path) -> io::Result<()> {
        let parts = ["changer", &self.changer, "drives", &self.drives, "catalog", &self.catalog];
        if let Some(d) = file.parent() {
            fs::create_dir_all(d)?;
        }
        fs::write(file, encode(&parts))
    }

    /// The catalog: the one chosen, or the default in the data folder `data`.
    pub fn catalog_path(&self, data: &Path) -> PathBuf {
        if self.catalog.trim().is_empty() {
            default_catalog(data)
        } else {
            PathBuf::from(self.catalog.trim())
        }
    }
}

/// The settings file in the data folder `data`.
pub fn settings_file(data: &Path) -> PathBuf {
    data.join("settings")
}

/// The catalog used unless another is chosen: in the data folder `data`.
pub fn default_catalog(data: &Path) -> PathBuf {
    data.join("catalog.sqlite")
}

/// The plugin's data folder for the RustySalamander installed in
/// `program` (the folder with `rustysalamander.exe`), found as the plugin
/// finds it when that program runs it.
pub fn data_dir_of_program(program: &Path) -> PathBuf {
    if program.join(MULTI_USER_FILE).is_file() {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        base.join("RustySalamander").join("plugins").join(PLUGIN_FOLDER)
    } else {
        program.join("plugins").join(PLUGIN_FOLDER)
    }
}

/// Whether Windows keeps `name` for a device (`NUL`, `con.txt`, `COM1`…).
fn device_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or("").trim_end_matches(' ').to_ascii_uppercase();
    matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (base.len() == 4 && (base.starts_with("COM") || base.starts_with("LPT")) && base.as_bytes()[3].is_ascii_digit())
}

/// A catalog file as typed (Library settings, `ltocat --catalog`): a whole
/// path to a file in a folder that exists, never a device. SQLite writes to
/// what it opens, so `\\.\PhysicalDrive0` or `NUL` must never get there.
pub fn check_catalog_path(typed: &str) -> Result<PathBuf, String> {
    let s = typed.trim().trim_matches('"').trim();
    if s.is_empty() {
        return Err("no catalog file was given".into());
    }
    if s.chars().any(|c| (c as u32) < 32 || c == '\u{7f}') {
        return Err("the catalog's path must not contain control characters".into());
    }
    let b = s.as_bytes();
    let ordinary = |rest: &[u8]| rest.len() >= 3 && rest[0].is_ascii_alphabetic() && rest[1..3] == *b":\\" || rest.len() >= 4 && rest[..4].eq_ignore_ascii_case(b"UNC\\");
    if b.starts_with(br"\\.\") || b.starts_with(br"\??\") || b.starts_with(br"\\?\") && !ordinary(&b[4..]) {
        return Err(format!("{s} is a device, not a file"));
    }
    let p = PathBuf::from(s);
    if !p.is_absolute() {
        return Err(format!("type the whole path of the catalog, such as D:\\Tapes\\catalog.sqlite (not {s})"));
    }
    for c in p.components() {
        if let std::path::Component::Normal(n) = c {
            let n = n.to_string_lossy();
            if let Some(c) = n.chars().find(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*')) {
                return Err(format!("a name in the catalog's path must not contain {c}"));
            }
            if device_name(&n) {
                return Err(format!("\"{n}\" is a name Windows keeps for a device"));
            }
        }
    }
    if p.is_dir() {
        return Err(format!("{} is a folder: name the catalog file in it", p.display()));
    }
    match p.parent() {
        Some(dir) if dir.is_dir() => Ok(p),
        Some(dir) => Err(format!("the folder {} doesn't exist", dir.display())),
        None => Err(format!("{s} has no folder")),
    }
}

/// Whether `dir` holds RustySalamander.
pub fn is_program_dir(dir: &Path) -> bool {
    dir.join(PROGRAM_EXE).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_folder_follows_the_program() {
        let t = std::env::temp_dir().join(format!("lto-place-{}", std::process::id()));
        let program = t.join("Rusty Salamander 🦎");
        fs::create_dir_all(&program).unwrap();
        assert!(!is_program_dir(&program));
        fs::write(program.join(PROGRAM_EXE), b"").unwrap();
        assert!(is_program_dir(&program));
        // portable: the plugin's own folder
        let data = data_dir_of_program(&program);
        assert_eq!(data, program.join("plugins").join("ltocatalog"));
        assert_eq!(default_catalog(&data), data.join("catalog.sqlite"));
        // multi-user: the user's %APPDATA%
        fs::write(program.join(MULTI_USER_FILE), "mode = \"multi-user\"\n").unwrap();
        let data = data_dir_of_program(&program);
        assert!(data.ends_with(Path::new("RustySalamander").join("plugins").join("ltocatalog")), "{}", data.display());
        assert!(!data.starts_with(&program));
        // settings saved by the plugin are read back, a chosen catalog wins
        let s = Settings { changer: "scsi:1:0:3:1".into(), drives: String::new(), catalog: r"\\nas\tapes\catalog.sqlite".into() };
        let file = settings_file(&t.join("data"));
        s.save(&file).unwrap();
        let back = Settings::load(&file);
        assert_eq!(back, s);
        assert_eq!(back.catalog_path(&t), PathBuf::from(r"\\nas\tapes\catalog.sqlite"));
        assert_eq!(Settings::default().catalog_path(&t), t.join("catalog.sqlite"));
        // a catalog is a file in a folder that exists, never a device
        let ok = t.join("Pásky 🦎.sqlite");
        assert_eq!(check_catalog_path(&format!("  \"{}\" ", ok.display())).unwrap(), ok);
        for bad in [
            String::new(),
            "   ".into(),
            r"\\.\PhysicalDrive0".into(),
            r"\\.\C:".into(),
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\x".into(),
            r"\??\C:\x".into(),
            "NUL".into(),
            r"C:\x\nul.sqlite".into(),
            r"C:\x\COM1".into(),
            "catalog.sqlite".into(),
            "C:catalog.sqlite".into(),
            format!("{}\\a|b", t.display()),
            format!("{}\\x\r\ny", t.display()),
            t.display().to_string(),
            format!("{}\\missing\\c.sqlite", t.display()),
            "a".repeat(100_000),
        ] {
            assert!(check_catalog_path(&bad).is_err(), "{bad:?}");
        }
        let _ = fs::remove_dir_all(&t);
    }
}
