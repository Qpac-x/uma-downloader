#!/usr/bin/env python3
"""Mark already-downloaded assets as downloaded in the game's Persistent/meta.

The game trusts table a.s (0 = not downloaded, 1 = downloaded), not the files
on disk. This sets s=1 for asset rows whose dat file exists at the expected
size. meta is a SQLite3 Multiple Ciphers database, so it is opened through the
bundled sqlite3mc DLL (Python's built-in sqlite3 cannot read it).

Dry run by default; pass --apply to write (after backing up meta).
Close the game first.
"""

import argparse
import ctypes
import os
import shutil
import struct
import subprocess
import sys
import time
from ctypes import POINTER, byref, c_char_p, c_int, c_int64, c_void_p

DEFAULT_DAT_DIR = r"C:\Program Files (x86)\Steam\steamapps\common\UmamusumePrettyDerby\UmamusumePrettyDerby_Data\Persistent"
_HERE = os.path.dirname(os.path.abspath(__file__))
# Bundled next to this script when copied into target/<profile>; otherwise the
# dev layout under the repo root.
DEFAULT_DLL = os.path.join(_HERE, "sqlite3mc_x64.dll")
if not os.path.exists(DEFAULT_DLL):
    DEFAULT_DLL = os.path.join(_HERE, "UmaViewer-master", "Assets", "Plugins", "sqlite3mc_x64.dll")
GAME_EXE = "UmamusumePrettyDerby.exe"

# UmaViewer Config.cs: GlobalDBKey XOR DBBaseKey (13-byte cycle).
GLOBAL_DB_KEY = bytes.fromhex(
    "36236b4c2a3921755226327625503f355d77586d4071385e4c3128742959372453"
)
DB_BASE_KEY = bytes.fromhex("f170cea4dfcea3e1a5d8c70bd1")

MANIFEST_TYPES = "('manifest','manifest2','manifest3')"

SQLITE_OK = 0
SQLITE_ROW = 100
SQLITE_DONE = 101
SQLITE_READONLY = 1
SQLITE_READWRITE = 2
SQLITE_NULL = 5
SQLITE_TRANSIENT = c_void_p(-1)

# A rollback journal is only hot (needs replay) while its header keeps this
# magic; SQLite zeroes it on commit, so a leftover file with a zeroed header is
# harmless residue.
HOT_JOURNAL_MAGIC = bytes.fromhex("d9d505f920a163d7")


class DbError(Exception):
    pass


def refuse(msg):
    print(f"refused: {msg}", file=sys.stderr)
    sys.exit(2)


def load_dll(path):
    d = ctypes.CDLL(path)
    d.sqlite3_open_v2.argtypes = [c_char_p, POINTER(c_void_p), c_int, c_void_p]
    d.sqlite3_open_v2.restype = c_int
    d.sqlite3_close.argtypes = [c_void_p]
    d.sqlite3_close.restype = c_int
    d.sqlite3_errmsg.argtypes = [c_void_p]
    d.sqlite3_errmsg.restype = c_char_p
    d.sqlite3mc_config.argtypes = [c_void_p, c_char_p, c_int]
    d.sqlite3mc_config.restype = c_int
    d.sqlite3_key.argtypes = [c_void_p, c_char_p, c_int]
    d.sqlite3_key.restype = c_int
    d.sqlite3_exec.argtypes = [c_void_p, c_char_p, c_void_p, c_void_p, POINTER(c_char_p)]
    d.sqlite3_exec.restype = c_int
    d.sqlite3_free.argtypes = [c_void_p]
    d.sqlite3_prepare_v2.argtypes = [c_void_p, c_char_p, c_int, POINTER(c_void_p), c_void_p]
    d.sqlite3_prepare_v2.restype = c_int
    d.sqlite3_step.argtypes = [c_void_p]
    d.sqlite3_step.restype = c_int
    d.sqlite3_finalize.argtypes = [c_void_p]
    d.sqlite3_finalize.restype = c_int
    d.sqlite3_column_text.argtypes = [c_void_p, c_int]
    d.sqlite3_column_text.restype = c_char_p
    d.sqlite3_column_int64.argtypes = [c_void_p, c_int]
    d.sqlite3_column_int64.restype = c_int64
    d.sqlite3_column_type.argtypes = [c_void_p, c_int]
    d.sqlite3_column_type.restype = c_int
    d.sqlite3_bind_text.argtypes = [c_void_p, c_int, c_char_p, c_int, c_void_p]
    d.sqlite3_bind_text.restype = c_int
    d.sqlite3_bind_int64.argtypes = [c_void_p, c_int, c_int64]
    d.sqlite3_bind_int64.restype = c_int
    d.sqlite3_reset.argtypes = [c_void_p]
    d.sqlite3_reset.restype = c_int
    d.sqlite3_clear_bindings.argtypes = [c_void_p]
    d.sqlite3_clear_bindings.restype = c_int
    d.sqlite3_changes.argtypes = [c_void_p]
    d.sqlite3_changes.restype = c_int
    return d


class MetaDb:
    def __init__(self, dll, path, write):
        self.d = dll
        self.h = c_void_p()
        flags = SQLITE_READWRITE if write else SQLITE_READONLY
        rc = dll.sqlite3_open_v2(os.fsencode(path), byref(self.h), flags, None)
        if rc != SQLITE_OK:
            raise DbError(f"open {path}: rc={rc}: {self.errmsg()}")
        # Cipher must be selected before the key. sqlite3mc_config returns the
        # new value (3), not an error code; only a negative result is fatal.
        if dll.sqlite3mc_config(self.h, b"cipher", 3) < 0:
            raise DbError("sqlite3mc_config(cipher=3) failed")
        key = bytes(g ^ DB_BASE_KEY[i % 13] for i, g in enumerate(GLOBAL_DB_KEY))
        rc = dll.sqlite3_key(self.h, key, len(key))
        if rc != SQLITE_OK:
            raise DbError(f"sqlite3_key: rc={rc}: {self.errmsg()}")
        self.exec("PRAGMA busy_timeout=5000")

    def errmsg(self):
        return (self.d.sqlite3_errmsg(self.h) or b"").decode("utf-8", "replace")

    def _prepare(self, sql):
        stmt = c_void_p()
        rc = self.d.sqlite3_prepare_v2(self.h, sql.encode(), -1, byref(stmt), None)
        if rc != SQLITE_OK:
            raise DbError(f"prepare '{sql}': rc={rc}: {self.errmsg()}")
        return stmt

    def exec(self, sql):
        err = c_char_p()
        rc = self.d.sqlite3_exec(self.h, sql.encode(), None, None, byref(err))
        msg = err.value.decode("utf-8", "replace") if err.value else self.errmsg()
        if err.value:
            self.d.sqlite3_free(err)
        if rc != SQLITE_OK:
            raise DbError(f"exec '{sql}': rc={rc}: {msg}")

    def _scalar(self, sql, text):
        stmt = self._prepare(sql)
        try:
            rc = self.d.sqlite3_step(stmt)
            if rc != SQLITE_ROW:
                raise DbError(f"step '{sql}': rc={rc}: {self.errmsg()}")
            if self.d.sqlite3_column_type(stmt, 0) == SQLITE_NULL:
                return None
            if text:
                return self.d.sqlite3_column_text(stmt, 0).decode("utf-8", "replace")
            return self.d.sqlite3_column_int64(stmt, 0)
        finally:
            self.d.sqlite3_finalize(stmt)

    def scalar_int(self, sql):
        return self._scalar(sql, False)

    def scalar_text(self, sql):
        return self._scalar(sql, True)

    def asset_rows_s0(self):
        # `i` (the integer primary key) for the update: `h` has no index, so
        # `WHERE h = ?` scans every s=0 row on each update (quadratic).
        stmt = self._prepare(
            f"SELECT i, h, l FROM a WHERE s = 0 AND m NOT IN {MANIFEST_TYPES}"
        )
        rows = []
        try:
            while True:
                rc = self.d.sqlite3_step(stmt)
                if rc == SQLITE_ROW:
                    h = self.d.sqlite3_column_text(stmt, 1).decode("ascii")
                    rows.append(
                        (self.d.sqlite3_column_int64(stmt, 0), h,
                         self.d.sqlite3_column_int64(stmt, 2))
                    )
                elif rc == SQLITE_DONE:
                    break
                else:
                    raise DbError(f"scan: rc={rc}: {self.errmsg()}")
        finally:
            self.d.sqlite3_finalize(stmt)
        return rows

    def mark(self, ids):
        self.exec("BEGIN IMMEDIATE")
        try:
            stmt = self._prepare("UPDATE a SET s = 1 WHERE i = ?1 AND s = 0")
            changed = 0
            try:
                for i in ids:
                    if self.d.sqlite3_bind_int64(stmt, 1, i) != SQLITE_OK:
                        raise DbError("bind failed")
                    rc = self.d.sqlite3_step(stmt)
                    if rc != SQLITE_DONE:
                        raise DbError(f"update: rc={rc}: {self.errmsg()}")
                    changed += self.d.sqlite3_changes(self.h)
                    self.d.sqlite3_reset(stmt)
                    self.d.sqlite3_clear_bindings(stmt)
            finally:
                self.d.sqlite3_finalize(stmt)
            self.exec("COMMIT")
            return changed
        except Exception:
            try:
                self.exec("ROLLBACK")
            except Exception:
                pass
            raise

    def close(self):
        if self.h:
            self.d.sqlite3_close(self.h)
            self.h = c_void_p()


def game_running():
    """True/False, or None when the check itself failed."""
    try:
        out = subprocess.run(
            ["tasklist", "/FI", f"IMAGENAME eq {GAME_EXE}", "/FO", "CSV", "/NH"],
            capture_output=True, text=True, timeout=15,
        )
    except OSError:
        return None
    if out.returncode != 0:
        return None
    return GAME_EXE.lower() in out.stdout.lower()


def main():
    ap = argparse.ArgumentParser(
        description="Mark size-matching assets as downloaded in Persistent/meta."
    )
    ap.add_argument("--dat-dir", default=DEFAULT_DAT_DIR,
                    help="Persistent directory (parent of dat/ and meta)")
    ap.add_argument("--db", default=None, help="meta path (default: <dat-dir>/meta)")
    ap.add_argument("--dll", default=DEFAULT_DLL, help="sqlite3mc_x64.dll path")
    ap.add_argument("--apply", action="store_true", help="write (default: dry run)")
    args = ap.parse_args()

    db_path = args.db or os.path.join(args.dat_dir, "meta")

    if struct.calcsize("P") != 8:
        refuse("64-bit Python required (sqlite3mc_x64.dll)")
    for label, path in (("meta", db_path), ("sqlite3mc DLL", args.dll)):
        if not os.path.isfile(path):
            refuse(f"{label} not found: {path}")

    running = game_running()
    if running is None:
        refuse("could not check whether the game is running (tasklist failed)")
    if running:
        refuse("close UmamusumePrettyDerby.exe first")

    journal = db_path + "-journal"
    if os.path.exists(journal):
        with open(journal, "rb") as f:
            if f.read(8) == HOT_JOURNAL_MAGIC:
                refuse(f"{journal} is hot; start and quit the game once first")
    if os.path.exists(db_path + "-wal"):
        refuse(f"{db_path}-wal exists; start and quit the game once first")

    if args.apply:
        free = shutil.disk_usage(os.path.dirname(db_path)).free
        if free < 250 * 1024 * 1024:
            refuse(f"only {free // (1024 * 1024)} MB free; need 250 MB for the backup")

    dll = load_dll(args.dll)
    db = MetaDb(dll, db_path, args.apply)
    try:
        total = db.scalar_int("SELECT count(*) FROM a")
        manifests = db.scalar_int(
            f"SELECT count(*) FROM a WHERE m IN {MANIFEST_TYPES}"
        )
        already = db.scalar_int(
            f"SELECT count(*) FROM a WHERE s = 1 AND m NOT IN {MANIFEST_TYPES}"
        )

        dat_dir = os.path.join(args.dat_dir, "dat")
        markable = []
        missing = []
        for i, h, size in db.asset_rows_s0():
            try:
                actual = os.path.getsize(os.path.join(dat_dir, h[:2], h))
            except OSError:
                actual = None
            if actual == size:
                markable.append(i)
            else:
                missing.append((h, size, actual))

        print(f"rows          {total}")
        print(f"  manifests   {manifests}")
        print(f"  assets      {total - manifests}")
        print(f"    already s=1  {already}")
        print(f"    markable     {len(markable)}")
        print(f"    missing/bad  {len(missing)}")
        for h, want, got in missing[:10]:
            print(f"      {h}  expected={want}  actual={got if got is not None else 'absent'}")
        if len(missing) > 10:
            print(f"      ... {len(missing) - 10} more")

        if not args.apply:
            print("dry run; pass --apply to write")
            return 0

        if game_running() is not False:
            refuse("game started during the scan; nothing was written")

        backup = f"{db_path}.bak-{time.strftime('%Y%m%d-%H%M%S')}"
        shutil.copy2(db_path, backup)
        if os.path.getsize(backup) != os.path.getsize(db_path):
            raise DbError("backup size mismatch; nothing was written")
        print(f"backup        {backup}")

        started = time.time()
        changed = db.mark(markable)
        print(f"marked        {changed} rows in {time.time() - started:.1f}s")

        now = db.scalar_int(
            f"SELECT count(*) FROM a WHERE s = 1 AND m NOT IN {MANIFEST_TYPES}"
        )
        print(f"assets s=1    {now}")
        check = db.scalar_text("PRAGMA quick_check")
        print(f"quick_check   {check}")
        return 0 if check == "ok" else 1
    finally:
        db.close()


if __name__ == "__main__":
    try:
        sys.exit(main())
    except DbError as e:
        print(f"error: {e}", file=sys.stderr)
        sys.exit(1)
