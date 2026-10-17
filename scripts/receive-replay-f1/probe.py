"""UNRUN disposable mechanism source; no database path argument or native st proof.
A separate reviewed ordinary-hosted unit must supply actual caller/slot/lease,
Python/SQLite pins, queue-inclusive deadline and terminal cleanup evidence.
"""
import hashlib
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import time

CAP_SECONDS = 90
SAMPLED_DISK_LIMIT_BYTES = 256 * 1024 * 1024
# Total rows PER SIDE, not rows per table. Large single-pair case tests row scale;
# 1/8/24 pairs at the same total test schema scale without multiplying disk 24x.
CASES = ((1, 0), (1, 1024), (1, 16384), (1, 131072), (8, 131072), (24, 131072))

def probe():
    start = time.monotonic()
    output = []
    with tempfile.TemporaryDirectory(prefix="st-replay-swap-disposable-") as owned:
        root = Path(owned)
        def check():
            size = sum(p.stat().st_size for p in root.iterdir() if p.is_file())
            if size > SAMPLED_DISK_LIMIT_BYTES:
                raise RuntimeError("sampled disk budget exceeded (hard quota must be supplied by the hosted owner)")
            if time.monotonic() - start >= CAP_SECONDS:
                raise RuntimeError("wall cap exceeded")
            return size
        for case_number, (pairs, total) in enumerate(CASES):
            db = root / f"fixture-{case_number}.sqlite"
            ticks = {"writer": 0, "reader": 0}
            connections = []
            counts = [total // pairs + int(i < total % pairs) for i in range(pairs)]
            def progress(role):
                def callback():
                    ticks[role] += 1
                    return int(time.monotonic() - start >= CAP_SECONDS)
                return callback
            def connect(role):
                con = sqlite3.connect(db, isolation_level=None, cached_statements=128, timeout=0.1)
                connections.append(con)
                con.set_progress_handler(progress(role), 100)
                return con
            record = {"pairs": pairs, "total_rows_per_side": total,
                      "rows_per_pair": counts, "phases": []}
            def file_sizes():
                result = {}
                for role,suffix in (("main_db",""),("wal","-wal"),("shm","-shm")):
                    try:
                        meta = (root/(db.name+suffix)).stat()
                        result[role] = {"logical_bytes":meta.st_size,"allocated_bytes":meta.st_blocks*512}
                    except FileNotFoundError:
                        result[role] = {"logical_bytes":0,"allocated_bytes":0}
                return result
            def rows(con, sql, params=()):
                return con.execute(sql, params).fetchall()
            def attempt(con, sql, params=()):
                try:
                    return {"rows": rows(con, sql, params)}
                except sqlite3.Error as error:
                    return {"error": str(error), "sqlite_code": error.sqlite_errorcode,
                            "sqlite_name": error.sqlite_errorname}
            def schema(con):
                return rows(con, "SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name")
            def transaction(label, callback):
                before = dict(ticks); files_before = file_sizes(); t = time.monotonic()
                try:
                    w.execute("BEGIN IMMEDIATE")
                    callback()
                    w.execute("COMMIT")
                except BaseException as error:
                    record["phases"].append({"label": label, "seconds_to_error": time.monotonic()-t,
                                              "error": str(error), "sqlite_code": getattr(error,"sqlite_errorcode",None)})
                    if w.in_transaction:
                        w.execute("ROLLBACK")
                    # Genuine error/cap is retained STOP, never retried or bypassed.
                    raise
                elapsed = time.monotonic()-t
                files_after = file_sizes()
                record["phases"].append({"label": label, "seconds_including_commit": elapsed,
                    "files_before":files_before,"files_after":files_after,
                    "file_growth":{role:{field:files_after[role][field]-files_before[role][field] for field in files_before[role]} for role in files_before},
                    "progress_callbacks_100_ops": {k:ticks[k]-before[k] for k in ticks}, "owned_bytes": check()})
            try:
                w = connect("writer")
                w.execute("PRAGMA journal_mode=WAL")
                w.execute("PRAGMA synchronous=FULL")
                w.execute("PRAGMA foreign_keys=ON")
                w.execute("CREATE TABLE observed(name TEXT NOT NULL)")
                for i in range(pairs):
                    w.executescript(f"""
                        CREATE TABLE live_{i}(id INTEGER PRIMARY KEY,v TEXT NOT NULL);
                        CREATE TABLE shadow_{i}(id INTEGER PRIMARY KEY,v TEXT NOT NULL);
                        CREATE INDEX live_lookup_{i} ON live_{i}(v);
                        CREATE INDEX shadow_lookup_{i} ON shadow_{i}(v);
                        CREATE TABLE dependent_{i}(id INTEGER PRIMARY KEY,ref INTEGER REFERENCES live_{i}(id));
                        CREATE VIEW live_view_{i} AS SELECT id,v FROM live_{i};
                        CREATE TRIGGER live_insert_{i} AFTER INSERT ON live_{i} BEGIN
                          INSERT INTO observed VALUES('old-{i}');
                        END;
                        CREATE TRIGGER shadow_insert_{i} AFTER INSERT ON shadow_{i} BEGIN
                          INSERT INTO observed VALUES('new-{i}');
                        END;
                    """)
                def fill():
                    for i,n in enumerate(counts):
                        w.executemany(f"INSERT INTO live_{i} VALUES(?,?)", ((j,"o"*128) for j in range(n)))
                        w.executemany(f"INSERT INTO shadow_{i} VALUES(?,?)", ((j,"n"*128) for j in range(n)))
                transaction("setup-seeding-not-swap", fill)
                record["schema_before"] = schema(w)
                record["pragmas"] = {k:rows(w,"PRAGMA "+k) for k in
                    ("journal_mode","synchronous","foreign_keys","wal_autocheckpoint","legacy_alter_table","page_size","schema_version")}
                record["index_name_collision"] = attempt(w,"CREATE INDEX live_lookup_0 ON shadow_0(v)")
                if "error" not in record["index_name_collision"]:
                    raise AssertionError("global index-name collision was not refused")
                # Second physical connection: warm its cached statement, then keep
                # both a read transaction and a partly consumed cursor across swap.
                r = connect("reader")
                r.execute("PRAGMA foreign_keys=ON")
                cached = "SELECT id,substr(v,1,1) FROM live_0 WHERE id=?"
                record["reader_cached_before"] = attempt(r,cached,(0,))
                r.execute("BEGIN")
                record["reader_snapshot_count_before"] = rows(r,"SELECT COUNT(*) FROM live_0")
                cursor = r.execute("SELECT id,substr(v,1,1) FROM live_0 ORDER BY id LIMIT 2")
                record["reader_cursor_first"] = cursor.fetchone()
                def swap():
                    for i in range(pairs):
                        w.execute(f"ALTER TABLE live_{i} RENAME TO retired_{i}")
                        w.execute(f"ALTER TABLE shadow_{i} RENAME TO live_{i}")
                transaction("name-only-swap-including-commit-with-reader-open", swap)
                record["schema_after"] = schema(w)
                record["writer_after"] = attempt(w,cached,(0,))
                record["reader_cached_inside_old_snapshot"] = attempt(r,cached,(0,))
                record["reader_cursor_remaining"] = cursor.fetchall()
                record["writer_indexed_by_before_repair"] = attempt(w,
                    "SELECT id FROM live_0 INDEXED BY live_lookup_0 WHERE v=? LIMIT 1",("n"*128,))
                # Native semantic probes: do ALTER rewrites leave FK/view on the
                # logical live table or on its retired physical predecessor?
                for i,n in enumerate(counts):
                    w.execute(f"INSERT INTO live_{i} VALUES(?,?)",(n+1,"candidate"))
                    w.execute(f"INSERT INTO retired_{i} VALUES(?,?)",(n+2,"retired"))
                    w.execute(f"INSERT INTO dependent_{i} VALUES(1,?)",(n+2,))
                record["foreign_key_check"] = rows(w,"PRAGMA foreign_key_check")
                record["fk_targets_after"] = {str(i):rows(w,f"PRAGMA foreign_key_list(dependent_{i})") for i in range(pairs)}
                record["view_after"] = rows(w,"SELECT id,substr(v,1,1) FROM live_view_0 ORDER BY id LIMIT 3")
                record["trigger_counts"] = rows(w,"SELECT name,COUNT(*) FROM observed GROUP BY name ORDER BY name")
                def repair_indexes():
                    for i in range(pairs):
                        w.execute(f"DROP INDEX live_lookup_{i}")
                        w.execute(f"DROP INDEX shadow_lookup_{i}")
                        w.execute(f"CREATE INDEX live_lookup_{i} ON live_{i}(v)")
                # Measure populated index repair separately WHILE the old reader
                # remains open. SQLite busy/locked is evidence, not an auto retry.
                transaction("populated-index-repair-including-commit-with-reader-open", repair_indexes)
                record["reader_after_index_repair_old_snapshot"] = attempt(r,cached,(0,))
                cursor.close();r.execute("COMMIT")
                record["reader_cached_after_snapshot_end"] = attempt(r,cached,(0,))
                record["reader_indexed_by_after_snapshot_end"] = attempt(r,
                    "SELECT id FROM live_0 INDEXED BY live_lookup_0 WHERE v='candidate' LIMIT 1")
                def retirement():
                    for i in range(pairs):
                        w.execute(f"DROP VIEW live_view_{i}")
                        w.execute(f"DROP TABLE dependent_{i}")
                        w.execute(f"DROP TABLE retired_{i}")
                transaction("populated-retirement-including-commit", retirement)
                old_expected = [] if counts[0] == 0 else [(0,"o")]
                new_expected = [] if counts[0] == 0 else [(0,"n")]
                record["discriminators"] = {
                    "reader_old_snapshot_before":record["reader_cached_before"].get("rows") == old_expected,
                    "reader_old_snapshot_after_swap":record["reader_cached_inside_old_snapshot"].get("rows") == old_expected,
                    "reader_old_snapshot_after_index_repair":record["reader_after_index_repair_old_snapshot"].get("rows") == old_expected,
                    "fresh_writer_new_table":record["writer_after"].get("rows") == new_expected,
                    "reader_new_table_after_snapshot_end":record["reader_cached_after_snapshot_end"].get("rows") == new_expected,
                    "existing_indexed_by_survives_name_only": "rows" in record["writer_indexed_by_before_repair"],
                    "index_repair_restores_lookup":record["reader_indexed_by_after_snapshot_end"].get("rows") == [(counts[0]+1,)]
                }
                record["cache_observation_limit"] = "Python may transparently reprepare; no raw SQLITE_SCHEMA count or rusqlite cache proof is inferred"
            except BaseException as error:
                record["terminal_error"] = str(error)
                print(json.dumps({"failed_case": record}, sort_keys=True), file=sys.stderr)
                raise
            finally:
                cleanup_start = time.monotonic()
                record["cleanup_sizes_before_close"] = file_sizes()
                # Closing the reader first releases its retained snapshot/cursor.
                close_errors = []
                for con in reversed(connections):
                    try:
                        con.close()
                    except sqlite3.Error as error:
                        close_errors.append(str(error))
                if close_errors:
                    print(json.dumps({"cleanup_errors": close_errors}), file=sys.stderr)
                    raise RuntimeError("owned connection cleanup failed")
                record["cleanup_sizes_after_physical_close"] = file_sizes()
                record["cleanup_seconds_to_physical_close"] = time.monotonic()-cleanup_start
                for suffix in ("", "-wal", "-shm"):
                    (root / (db.name+suffix)).unlink(missing_ok=True)
                record["cleanup_sizes_after_owned_removal"] = file_sizes()
                record["cleanup_seconds_including_removal"] = time.monotonic()-cleanup_start
                print(json.dumps({"terminal_cleanup":{k:v for k,v in record.items() if k.startswith("cleanup_")}},sort_keys=True))
            record["cleanup"] = "both connections closed; this case's owned DB/WAL/SHM removed"
            output.append(record)
            print(json.dumps({"completed_case":record},sort_keys=True))
            check()
    return {"status":"mechanism observations only", "sqlite_version":sqlite3.sqlite_version,
            "source_sha256":hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            "wall_seconds":time.monotonic()-start, "cases":output,
            "cleanup":"owned temporary directory removed; no existing DB accepted"}

if __name__ == "__main__":
    print(json.dumps(probe(),sort_keys=True))
