# Umamusume Downloader

A parallel, resumable asset downloader and database sync tool for
**Umamusume: Pretty Derby (Global / Steam)**, written in Rust.

It downloads the game's asset bundle files directly from the CDN, much faster
than the in-game downloader, and then marks them as downloaded in the game's
local `meta` database so the game accepts them without fetching them again.

> **AI-generated code**
>
> This project was written by an AI assistant (Claude, by Anthropic) under
> human direction. The format research, testing and verification were done
> against a real game installation. Treat the code accordingly.

> **Inspired by UmaViewer**
>
> This tool exists because of the reverse-engineering work in
> [UmaViewer](https://github.com/katboi01/UmaViewer) (katboi01). Its manifest
> parsing, database schema and decryption-key constants were used as a
> **reference for the file formats**.
>
> **No UmaViewer source code was copied.** All Rust and Python code in this
> repository is original. Two byte-array key constants and the documented
> format knowledge originate from UmaViewer; the SQLite3 Multiple Ciphers DLL
> (`vendor/sqlite3mc_x64.dll`) is a third-party library redistributed from
> UmaViewer's plugins folder — see [Credits](#credits).

---

## Why this exists

The game's own "download all data" is serial, fragile and often restarts
after a connection drop or a game update. This tool:

- downloads assets **in parallel** (configurable worker count);
- **resumes** interrupted files with HTTP `Range` requests (`.part` files);
- can run **headless** (CLI) or with a native **GUI**;
- **syncs the game's database** afterwards, so the game sees the files as
  already downloaded and stops asking for them.

The full Windows asset set is roughly **191,000 files / 33 GB**; only the
delta is downloaded when the game receives content updates.

## Features

- **Parallel, resumable downloads** with retry/backoff and stall detection.
- **Uses the game's own cached manifests** from
  `Persistent/dat/` — no hardcoded resource version that goes stale.
- **Native GUI** (egui/eframe): dashboard, per-category selection, live
  per-file progress, pause/resume/cancel, and a game-status panel.
- **Native database indexer** (no Python needed) that marks downloaded files
  as present in the game's encrypted `meta` database, with backup and
  `PRAGMA quick_check` verification.
- **"Up to date" indicator**: how many files are on disk, how many rows are
  indexed, and when the game last refreshed its manifests.
- CLI for scripting.

## How it works

### 1. Manifests

The game keeps its current manifest tree cached in
`<Game>/UmamusumePrettyDerby_Data/Persistent/dat/`:

```
root.manifest          -> platform manifests (Windows, iOS, Android)
platform manifest      -> ~37 category manifests (assetbundles, story, chara, ...)
category manifest      -> asset entries: name, size, 16-byte hash
```

All manifests are LZ4-frame compressed. The **asset filename is derived from
its content**:

```
filename = base32_nopad(SHA1(hash[0:8] + size_big_endian_u64 + name))
```

The file is stored at `dat/<first two chars>/<filename>`. This tool reads the
manifests from the game's own cache, so it always matches the version the game
is running.

Only the *root* manifest URL is version-specific
(`${CDN}/<version>/manifests/...`); platform/category manifests and assets use
content-hash URLs, so they never go stale.

### 2. Downloading

Each asset is fetched from

```
https://assets-umamusume-en.akamaized.net/dl/vertical/resources/Windows/<category>/<XX>/<filename>
```

Files that already exist on disk with the expected size are skipped. Partial
downloads live in `<dest>.part` and resume with a `Range` request.

### 3. Indexing (why the game ignored the files before)

`Persistent/meta` is a **SQLite3 Multiple Ciphers** database. Table `a` holds
one row per asset; the game uses column **`s`** (`0` = not downloaded,
`1` = downloaded) and trusts it over the filesystem. Files downloaded by this
tool are therefore invisible to the game until their rows are marked.

The indexer:

1. finds every row with `s = 0` (excluding manifest rows);
2. checks whether `dat/<XX>/<filename>` exists with the expected size;
3. copies `meta` to `meta.bak-<epoch>` (size-verified);
4. updates the matching rows **by integer primary key** `i` in one transaction
   (`h` has no index — updating by `h` is quadratic and takes forever);
5. runs `PRAGMA quick_check` and re-counts the indexed rows.

It refuses to write while `UmamusumePrettyDerby.exe` is running or while a hot
rollback journal (`meta-journal`) is present.

## Requirements

- **Windows** 10/11 (the code targets Windows paths and processes; the game is
  Windows/Steam here).
- **Umamusume: Pretty Derby (Global, Steam)** installed.
- **Rust** (stable toolchain) to build.
- Write access to the game directory — run elevated or point `--dat-dir`
  elsewhere (the game won't see files outside its own directory, but the CLI
  can still be used).
- The bundled `sqlite3mc_x64.dll` (in `vendor/`) is copied next to the
  executable automatically at build time.

## Building

```powershell
cd uma-downloader
cargo build --release
```

The result is a self-contained folder:

```
target/release/
  uma-downloader.exe      # GUI + CLI
  sqlite3mc_x64.dll       # database driver (vendored)
  uma_index_meta.py       # optional Python reference indexer
```

The build script copies the DLL and the Python script next to the executable
automatically.

## GUI usage

Launch `uma-downloader.exe` with **no arguments**:

```powershell
.\target\release\uma-downloader.exe
```

### First run

1. The tool reads the game's cached manifests and scans `dat/`.
2. It shows the current state: game data id + sync age, files on disk, and how
   many database rows are indexed.
3. Close the game before indexing (the indexer refuses to write while it runs).

### Layout

- **Left panel — Settings**: game `Persistent` directory, download worker count
  (1–64), and the category list with `All` / `None` shortcuts.
- **Status banner**: the headline result — "Game data is up to date" (green),
  "N files missing" (yellow), or a database error (red).
- **Summary tiles**: game data identity, files on disk, database index state.
- **Progress card**: overall progress, file counts, speed and ETA.
- **Downloading now**: live per-file progress for active workers; fills the
  window.

### Buttons

| Button | What it does |
|--------|--------------|
| **Start** | Downloads the checked categories (skips files already present). |
| **Update all** | Downloads every category — use after a game update. |
| **Pause / Resume** | Pauses in-flight workers; resumes where they stopped. |
| **Cancel** | Stops the run (`.part` files remain and resume later). |
| **Reload manifests** | Re-reads the manifest tree from the game cache / CDN. |
| **Fix game DB** | Runs the database indexer manually (game must be closed). |

After every completed download the indexer runs automatically, so the game
accepts the new files immediately.

### Typical update flow

1. Start the game once and quit at the title screen — it refreshes
   `Persistent/dat/` manifests and inserts the new rows into `meta`.
2. Close the game.
3. Open the downloader → **Update all**.
4. Wait for the green "Game data is up to date" banner.
5. Start the game — it should not download anything.

## CLI usage

Passing any CLI flag runs headless instead of opening the GUI.

```powershell
# List categories and totals (no network)
uma-downloader.exe --list

# Show what would be downloaded
uma-downloader.exe --dry-run

# Sync everything with 16 workers
uma-downloader.exe --workers 16

# Sync selected categories only
uma-downloader.exe --category story,storyevent,chara

# Use a different game data directory
uma-downloader.exe --dat-dir "D:\Games\Umamusume\UmamusumePrettyDerby_Data\Persistent"

# Fetch a specific resource version's root manifest from the CDN
uma-downloader.exe --resource-version 10008100

# Mark size-matching files as downloaded in the game database (game closed)
uma-downloader.exe --index
```

| Flag | Description |
|------|-------------|
| `--dat-dir <PATH>` | `Persistent` directory (parent of `dat/` and `meta`). |
| `--category <LIST>` | Comma-separated categories; default is all. |
| `--workers <N>` | Concurrent downloads (default 8). |
| `--list` | Print categories, counts and sizes. |
| `--dry-run` | Plan only; print counts without downloading. |
| `--resource-version <V>` | Override the cached root manifest version. |
| `--index` | Run the database indexer and exit. |
| `--gui` | Force the GUI. |

Exit code is `1` when any download failed.

## Python reference indexer

`uma_index_meta.py` is the original standalone indexer, kept as a reference and
fallback (requires Python 3). It is **dry-run by default**:

```powershell
python uma_index_meta.py            # report only
python uma_index_meta.py --apply    # write (game must be closed)
```

The native `--index` / GUI indexer is preferred and needs no Python.

## How "up to date" is determined

The game stores no readable resource version (`meta`'s `i` table only holds a
schema version, and old third-party version APIs are dead). The tool therefore
uses:

- the **cached root manifest filename** as the data identity, plus the time the
  game last refreshed it;
- **files on disk** = count of manifest entries whose file exists at the exact
  expected size;
- **indexed** = count of asset rows with `s = 1` in `meta`.

"Up to date" (green) means all manifest files exist and all asset rows are
indexed.

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `no cached root manifest in .../dat` | Launch the game once (it syncs manifests), or pass `--resource-version`. |
| `sqlite3mc_x64.dll not found` | Keep the DLL next to the executable (it is copied there by the build). |
| `close UmamusumePrettyDerby.exe before writing the database` | Quit the game completely, then retry. |
| `... is a hot journal; start and quit the game once` | Start the game and quit at the title screen, then retry. |
| Permission denied writing files | Run elevated, or choose a writable `--dat-dir`. |
| Downloads are slow / failing | Lower `--workers`, rerun — partial files resume. |
| Antivirus flags the exe | It is an unsigned locally built binary; build it yourself if in doubt. |

## Notes and limitations

- Content is validated by **size only** — filenames are content-addressed by
  the game itself, so a file with the right name and size is the right file.
- A future game update could change the manifest format, the database cipher,
  the key constants or the CDN layout; that would require a code change.
- Only the **Global (Steam) Windows** client has been tested.
- Downloading is tied to the account-free CDN; the game's own login is not
  touched.

## Credits

- **UmaViewer** — https://github.com/katboi01/UmaViewer — reference for
  manifest and database formats and the key constants. No code copied.
- **SQLite3 Multiple Ciphers** — https://github.com/utelle/SQLite3MultipleCiphers
  — MIT license; `vendor/sqlite3mc_x64.dll` is redistributed with this project.
- Rust crates: `eframe`/`egui`, `tokio`, `reqwest`, `lz4_flex`, `sha1`,
  `clap`, `indicatif`, `futures`, `libloading`, `data-encoding`.

## Disclaimer

Unofficial personal-use tool. Not affiliated with or endorsed by Cygames.
Use at your own risk; back up `Persistent/meta` before writing (the indexer
does this automatically).
