//! Mark already-downloaded assets as downloaded in the game's `Persistent/meta`.
//!
//! The game trusts table `a.s` (0 = not downloaded, 1 = downloaded), not the
//! files on disk. `meta` is a SQLite3 Multiple Ciphers database, which neither
//! SQLite crate can open, so this calls the `sqlite3mc_x64.dll` that UmaViewer
//! ships — the same DLL and key the game uses.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_READONLY: c_int = 1;
const SQLITE_READWRITE: c_int = 2;

const GAME_EXE: &str = "UmamusumePrettyDerby.exe";
const DLL_NAME: &str = "sqlite3mc_x64.dll";
const MANIFEST_TYPES: &str = "('manifest','manifest2','manifest3')";
// A rollback journal is only hot while it keeps this magic; SQLite zeroes it
// on commit, so leftover residue with a zeroed header is harmless.
const HOT_JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

// UmaViewer Config.cs: GlobalDBKey XOR DBBaseKey (13-byte cycle).
const GLOBAL_DB_KEY: [u8; 33] = [
    0x36, 0x23, 0x6b, 0x4c, 0x2a, 0x39, 0x21, 0x75, 0x52, 0x26, 0x32, 0x76, 0x25, 0x50, 0x3f,
    0x35, 0x5d, 0x77, 0x58, 0x6d, 0x40, 0x71, 0x38, 0x5e, 0x4c, 0x31, 0x28, 0x74, 0x29, 0x59,
    0x37, 0x24, 0x53,
];
const DB_BASE_KEY: [u8; 13] = [
    0xf1, 0x70, 0xce, 0xa4, 0xdf, 0xce, 0xa3, 0xe1, 0xa5, 0xd8, 0xc7, 0x0b, 0xd1,
];

fn derived_key() -> [u8; 33] {
    let mut key = GLOBAL_DB_KEY;
    for (i, b) in key.iter_mut().enumerate() {
        *b ^= DB_BASE_KEY[i % DB_BASE_KEY.len()];
    }
    key
}

#[derive(Debug, Clone)]
pub struct IndexReport {
    pub assets: i64,
    pub missing: usize,
    /// Up to 10 `"filename expected=… actual=…"` lines for display.
    pub missing_samples: Vec<String>,
    pub marked: usize,
    pub indexed: i64,
    pub quick_check: Option<String>,
    pub backup: Option<PathBuf>,
}

impl IndexReport {
    pub fn is_fully_indexed(&self) -> bool {
        self.indexed == self.assets
    }
}

/// True when the game process is running. Errors if the check itself fails.
pub fn game_running() -> Result<bool> {
    let filter = format!("IMAGENAME eq {GAME_EXE}");
    let out = Command::new("tasklist")
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .output()
        .context("run tasklist")?;
    if !out.status.success() {
        bail!("tasklist failed");
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .to_lowercase()
        .contains(&GAME_EXE.to_lowercase()))
}

/// Search next to the executable and up the tree (dev layout lives in
/// `uma-downloader/target/release`, UmaViewer at the repo root).
fn find_library() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current_exe")?;
    let mut dir = exe.parent();
    while let Some(d) = dir {
        for candidate in [
            d.join(DLL_NAME),
            d.join("UmaViewer-master")
                .join("Assets")
                .join("Plugins")
                .join(DLL_NAME),
        ] {
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        dir = d.parent();
    }
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd
            .join("UmaViewer-master")
            .join("Assets")
            .join("Plugins")
            .join(DLL_NAME);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!(
        "{DLL_NAME} not found next to the executable, in the working directory, \
         or in UmaViewer-master/Assets/Plugins/"
    )
}

fn hot_journal(path: &Path) -> Result<bool> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(bytes.len() >= 8 && bytes[..8] == HOT_JOURNAL_MAGIC),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

type OpenV2 = unsafe extern "C" fn(*const c_char, *mut *mut c_void, c_int, *const c_char) -> c_int;
type Close = unsafe extern "C" fn(*mut c_void) -> c_int;
type Errmsg = unsafe extern "C" fn(*mut c_void) -> *const c_char;
type Free = unsafe extern "C" fn(*mut c_void);
type McConfig = unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> c_int;
type Key = unsafe extern "C" fn(*mut c_void, *const c_void, c_int) -> c_int;
type Exec =
    unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void, *mut c_void, *mut *mut c_char) -> c_int;
type PrepareV2 =
    unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut *mut c_void, *const c_char) -> c_int;
type Step = unsafe extern "C" fn(*mut c_void) -> c_int;
type Finalize = unsafe extern "C" fn(*mut c_void) -> c_int;
type ColumnInt64 = unsafe extern "C" fn(*mut c_void, c_int) -> i64;
type ColumnText = unsafe extern "C" fn(*mut c_void, c_int) -> *const c_char;
type BindInt64 = unsafe extern "C" fn(*mut c_void, c_int, i64) -> c_int;
type Reset = unsafe extern "C" fn(*mut c_void) -> c_int;
type Changes = unsafe extern "C" fn(*mut c_void) -> c_int;

struct Api {
    _lib: libloading::Library,
    open_v2: OpenV2,
    close: Close,
    errmsg: Errmsg,
    free: Free,
    mc_config: McConfig,
    key: Key,
    exec: Exec,
    prepare_v2: PrepareV2,
    step: Step,
    finalize: Finalize,
    column_int64: ColumnInt64,
    column_text: ColumnText,
    bind_int64: BindInt64,
    reset: Reset,
    changes: Changes,
}

impl Api {
    fn load(path: &Path) -> Result<Api> {
        unsafe {
            let lib = libloading::Library::new(path)
                .with_context(|| format!("load {}", path.display()))?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get(concat!($name, "\0").as_bytes())
                        .with_context(|| format!("symbol {}", $name))?
                };
            }
            Ok(Api {
                open_v2: sym!("sqlite3_open_v2"),
                close: sym!("sqlite3_close"),
                errmsg: sym!("sqlite3_errmsg"),
                free: sym!("sqlite3_free"),
                mc_config: sym!("sqlite3mc_config"),
                key: sym!("sqlite3_key"),
                exec: sym!("sqlite3_exec"),
                prepare_v2: sym!("sqlite3_prepare_v2"),
                step: sym!("sqlite3_step"),
                finalize: sym!("sqlite3_finalize"),
                column_int64: sym!("sqlite3_column_int64"),
                column_text: sym!("sqlite3_column_text"),
                bind_int64: sym!("sqlite3_bind_int64"),
                reset: sym!("sqlite3_reset"),
                changes: sym!("sqlite3_changes"),
                _lib: lib,
            })
        }
    }
}

struct Db<'a> {
    api: &'a Api,
    h: *mut c_void,
}

impl<'a> Db<'a> {
    fn open(api: &'a Api, path: &Path, write: bool) -> Result<Db<'a>> {
        let c_path = CString::new(path.to_string_lossy().as_bytes())?;
        let mut h: *mut c_void = null_mut();
        let flags = if write { SQLITE_READWRITE } else { SQLITE_READONLY };
        let rc = unsafe { (api.open_v2)(c_path.as_ptr(), &mut h, flags, null()) };
        let db = Db { api, h };
        if rc != SQLITE_OK {
            let msg = db.errmsg();
            unsafe { (api.close)(h) };
            bail!("open {}: {msg}", path.display());
        }
        // Cipher must be selected before the key; the call returns the new
        // value, so only a negative result is a failure.
        let cipher = CString::new("cipher")?;
        if unsafe { (api.mc_config)(h, cipher.as_ptr(), 3) } < 0 {
            bail!("sqlite3mc_config(cipher=3) failed");
        }
        let key = derived_key();
        let rc = unsafe { (api.key)(h, key.as_ptr() as *const c_void, key.len() as c_int) };
        if rc != SQLITE_OK {
            bail!("sqlite3_key: {}", db.errmsg());
        }
        db.exec("PRAGMA busy_timeout=5000")?;
        Ok(db)
    }

    fn errmsg(&self) -> String {
        unsafe {
            let p = (self.api.errmsg)(self.h);
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }

    fn exec(&self, sql: &str) -> Result<()> {
        let c_sql = CString::new(sql)?;
        let mut err: *mut c_char = null_mut();
        let rc = unsafe {
            (self.api.exec)(
                self.h,
                c_sql.as_ptr(),
                null_mut(),
                null_mut(),
                &mut err,
            )
        };
        let msg = if err.is_null() {
            self.errmsg()
        } else {
            let msg = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
            unsafe { (self.api.free)(err as *mut c_void) };
            msg
        };
        if rc != SQLITE_OK {
            bail!("exec '{sql}': {msg}");
        }
        Ok(())
    }

    fn prepare(&self, sql: &str) -> Result<*mut c_void> {
        let c_sql = CString::new(sql)?;
        let mut stmt: *mut c_void = null_mut();
        let rc = unsafe {
            (self.api.prepare_v2)(self.h, c_sql.as_ptr(), -1, &mut stmt, null())
        };
        if rc != SQLITE_OK {
            bail!("prepare '{sql}': {}", self.errmsg());
        }
        Ok(stmt)
    }

    fn scalar_int(&self, sql: &str) -> Result<i64> {
        let stmt = self.prepare(sql)?;
        let rc = unsafe { (self.api.step)(stmt) };
        let value = if rc == SQLITE_ROW {
            unsafe { (self.api.column_int64)(stmt, 0) }
        } else {
            0
        };
        let msg = if rc == SQLITE_ROW { None } else { Some(self.errmsg()) };
        unsafe { (self.api.finalize)(stmt) };
        match msg {
            None => Ok(value),
            Some(msg) => bail!("query '{sql}': {msg}"),
        }
    }

    fn scalar_text(&self, sql: &str) -> Result<String> {
        let stmt = self.prepare(sql)?;
        let rc = unsafe { (self.api.step)(stmt) };
        let value = if rc == SQLITE_ROW {
            let p = unsafe { (self.api.column_text)(stmt, 0) };
            if p.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
            }
        } else {
            String::new()
        };
        unsafe { (self.api.finalize)(stmt) };
        if rc == SQLITE_ROW {
            Ok(value)
        } else {
            bail!("query '{sql}': {}", self.errmsg())
        }
    }

    /// Rows still marked not downloaded: `(i, filename, expected size)`.
    fn s0_rows(&self) -> Result<Vec<(i64, String, u64)>> {
        let sql = format!("SELECT i, h, l FROM a WHERE s = 0 AND m NOT IN {MANIFEST_TYPES}");
        let stmt = self.prepare(&sql)?;
        let mut rows = Vec::new();
        loop {
            let rc = unsafe { (self.api.step)(stmt) };
            if rc == SQLITE_DONE {
                break;
            }
            if rc != SQLITE_ROW {
                let msg = self.errmsg();
                unsafe { (self.api.finalize)(stmt) };
                bail!("scan rows: {msg}");
            }
            let i = unsafe { (self.api.column_int64)(stmt, 0) };
            let h = unsafe {
                let p = (self.api.column_text)(stmt, 1);
                if p.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            };
            let size = unsafe { (self.api.column_int64)(stmt, 2) } as u64;
            rows.push((i, h, size));
        }
        unsafe { (self.api.finalize)(stmt) };
        Ok(rows)
    }

    /// One transaction; `h` has no index, so the update must use the integer
    /// primary key `i` (a `WHERE h = ?` scan makes this quadratic).
    fn mark(&self, ids: &[i64]) -> Result<usize> {
        self.exec("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<usize> {
            let stmt = self.prepare("UPDATE a SET s = 1 WHERE i = ?1 AND s = 0")?;
            let mut changed = 0usize;
            for &id in ids {
                let rc = unsafe { (self.api.bind_int64)(stmt, 1, id) };
                if rc != SQLITE_OK {
                    unsafe { (self.api.finalize)(stmt) };
                    bail!("bind failed");
                }
                let rc = unsafe { (self.api.step)(stmt) };
                if rc != SQLITE_DONE {
                    let msg = self.errmsg();
                    unsafe { (self.api.finalize)(stmt) };
                    bail!("update: {msg}");
                }
                changed += unsafe { (self.api.changes)(self.h) }.max(0) as usize;
                unsafe {
                    (self.api.reset)(stmt);
                    // sqlite3_clear_bindings is not needed: the next bind
                    // replaces the value.
                }
            }
            unsafe { (self.api.finalize)(stmt) };
            Ok(changed)
        })();
        match result {
            Ok(changed) => {
                if let Err(e) = self.exec("COMMIT") {
                    let _ = self.exec("ROLLBACK");
                    return Err(e);
                }
                Ok(changed)
            }
            Err(e) => {
                let _ = self.exec("ROLLBACK");
                Err(e)
            }
        }
    }
}

impl Drop for Db<'_> {
    fn drop(&mut self) {
        if !self.h.is_null() {
            unsafe { (self.api.close)(self.h) };
        }
    }
}

/// Scan (and with `apply`, write) the game's meta database. `dat_dir` is the
/// Persistent directory: parent of `dat/` and `meta`.
pub fn index_meta(dat_dir: &Path, apply: bool) -> Result<IndexReport> {
    let db_path = dat_dir.join("meta");
    if !db_path.is_file() {
        bail!("{} not found", db_path.display());
    }
    if apply {
        if game_running()? {
            bail!("close {GAME_EXE} before writing the database");
        }
        let journal = dat_dir.join("meta-journal");
        if hot_journal(&journal)? {
            bail!(
                "{} is a hot journal; start and quit the game once, then retry",
                journal.display()
            );
        }
    }

    let dat = dat_dir.join("dat");
    let api = Api::load(&find_library()?)?;
    let db = Db::open(&api, &db_path, apply)?;

    let assets = db.scalar_int("SELECT count(*) FROM a")?
        - db.scalar_int(&format!("SELECT count(*) FROM a WHERE m IN {MANIFEST_TYPES}"))?;
    let already = db.scalar_int(&format!(
        "SELECT count(*) FROM a WHERE s = 1 AND m NOT IN {MANIFEST_TYPES}"
    ))?;

    let mut markable = Vec::new();
    let mut missing = 0usize;
    let mut missing_samples = Vec::new();
    for (id, h, size) in db.s0_rows()? {
        let actual = std::fs::metadata(dat.join(&h[..2.min(h.len())]).join(&h))
            .ok()
            .map(|m| m.len());
        if actual == Some(size) {
            markable.push(id);
        } else {
            missing += 1;
            if missing_samples.len() < 10 {
                missing_samples.push(format!(
                    "{h} expected={size} actual={}",
                    actual.map_or_else(|| "absent".to_string(), |a| a.to_string())
                ));
            }
        }
    }

    let mut marked = 0;
    let mut quick_check = None;
    let mut backup = None;
    if apply && !markable.is_empty() {
        if game_running()? {
            bail!("game started during the scan; nothing was written");
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = dat_dir.join(format!("meta.bak-{stamp}"));
        std::fs::copy(&db_path, &path)
            .with_context(|| format!("backup to {}", path.display()))?;
        if std::fs::metadata(&path)?.len() != std::fs::metadata(&db_path)?.len() {
            bail!("backup size mismatch; nothing was written");
        }
        backup = Some(path);
        marked = db.mark(&markable)?;
        quick_check = Some(db.scalar_text("PRAGMA quick_check")?);
        let indexed = db.scalar_int(&format!(
            "SELECT count(*) FROM a WHERE s = 1 AND m NOT IN {MANIFEST_TYPES}"
        ))?;
        return Ok(IndexReport {
            assets,
            missing,
            missing_samples,
            marked,
            indexed,
            quick_check,
            backup,
        });
    }

    Ok(IndexReport {
        assets,
        missing,
        missing_samples,
        marked,
        indexed: already,
        quick_check,
        backup,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pins both key constants (UmaViewer Config.cs) to the verified result; a
    // typo in either would silently make every open fail with NOTADB.
    #[test]
    fn derived_key_matches_verified_bytes() {
        assert_eq!(
            derived_key(),
            [
                0xc7, 0x53, 0xa5, 0xe8, 0xf5, 0xf7, 0x82, 0x94, 0xf7, 0xfe, 0xf5, 0x7d, 0xf4,
                0xa1, 0x4f, 0xfb, 0xf9, 0xa8, 0x96, 0xce, 0xa1, 0xd4, 0xe0, 0x99, 0x47, 0xe0,
                0xd9, 0x04, 0xe7, 0xfd, 0xe8, 0xea, 0xf0,
            ]
        );
    }
}
