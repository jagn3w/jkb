#!/usr/bin/env python3
"""Can two kernels share one SQLite WAL database through this bind mount?

Run the SAME script on both sides of the container boundary, at the same time:

    host:       python3 .container/sqlite-share-probe.py host   ~/.jkb/walprobe
    container:  python3 .container/sqlite-share-probe.py container   ~/.jkb/walprobe

The directory argument must be the SAME directory as seen from each side, on the bind mount under
test. The two rendezvous through files in it, so start order does not matter (each waits up

to 30 minutes for the other). Nothing here touches jkb.db: every file it uses lives in
<dir>/run-<id>/. `selftest <dir>` runs both roles on one kernel, where every phase must pass.

SQLite's WAL mode needs two operating-system facilities to be shared by every process using the
database: POSIX advisory locks (fcntl) on the database and its -shm file, and a MAP_SHARED mmap of
the -shm file (the wal-index). SQLite's documentation says WAL requires all processes to be on the
same host for exactly this reason. So the probe measures the two primitives directly, in both
directions, and then runs the real thing:

  phase 1  locks   one side holds an fcntl write lock; the other asks F_GETLK and tries F_SETLK.
  phase 2  mmap    one side increments a counter in a MAP_SHARED mapping; the other samples its
                   own mapping of the same file and reports how far behind it reads.
  phase 3  sqlite  both sides run concurrent writers (BEGIN IMMEDIATE, WAL, synchronous=NORMAL,
                   busy_timeout=5000 -- jkb's pragmas, crates/jkb-core/src/db.rs) plus a
                   checkpointer, then the container side verifies: integrity_check, every
                   committed row present exactly once, nothing present that was not committed.

Prints a verdict per phase; full results land in ./run-<id>/results-<side>.json.
"""
import fcntl
import json
import mmap
import os
import random
import sqlite3
import struct
import sys
import time

HERE = os.path.abspath(os.path.expanduser(sys.argv[2])) if len(sys.argv) > 2 else os.path.dirname(os.path.abspath(__file__))
WAIT = 1800  # seconds to wait for the other side
STRESS_SECONDS = 45
WRITERS = 4

LOCK_ARGS = 'hhqqi4x' if sys.platform.startswith('linux') else 'qqihh'


def flock_struct(ltype, start, length):
    if sys.platform.startswith('linux'):
        return struct.pack('hhqqi4x', ltype, os.SEEK_SET, start, length, 0)
    # Darwin: struct flock { off_t l_start; off_t l_len; pid_t l_pid; short l_type; short l_whence; }
    return struct.pack('qqihh', start, length, 0, ltype, os.SEEK_SET)


def flock_unpack(buf):
    if sys.platform.startswith('linux'):
        t, _w, s, l, pid = struct.unpack('hhqqi4x', buf)
    else:
        s, l, pid, t, _w = struct.unpack('qqihh', buf)
    return t, s, l, pid


def wait_for(path, timeout=WAIT, poll=0.02):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if os.path.exists(path):
            return True
        # virtiofs caches negative lookups; listing the directory refreshes it.
        try:
            os.listdir(os.path.dirname(path))
        except OSError:
            pass
        time.sleep(poll)
    return False


def touch(path, body=''):
    tmp = path + '.tmp'
    with open(tmp, 'w') as f:
        f.write(body)
    os.replace(tmp, path)


def say(side, msg):
    print(f'[{side} {time.strftime("%H:%M:%S")}] {msg}', flush=True)


# --- phase 1: fcntl locks ---------------------------------------------------------------------

def lock_hold(run, side, tag):
    fd = os.open(os.path.join(run, 'lockfile'), os.O_RDWR | os.O_CREAT, 0o600)
    fcntl.fcntl(fd, fcntl.F_SETLK, flock_struct(fcntl.F_WRLCK, 0, 16))
    touch(os.path.join(run, f'{tag}.held'))
    wait_for(os.path.join(run, f'{tag}.observed'))
    os.close(fd)
    touch(os.path.join(run, f'{tag}.released'))


def lock_observe(run, side, tag):
    wait_for(os.path.join(run, f'{tag}.held'))
    time.sleep(0.5)
    fd = os.open(os.path.join(run, 'lockfile'), os.O_RDWR)
    t, s, l, pid = flock_unpack(fcntl.fcntl(fd, fcntl.F_GETLK, flock_struct(fcntl.F_WRLCK, 0, 16)))
    getlk = {fcntl.F_UNLCK: 'unlocked', fcntl.F_RDLCK: 'shared', fcntl.F_WRLCK: 'exclusive'}.get(t, str(t))
    try:
        fcntl.fcntl(fd, fcntl.F_SETLK, flock_struct(fcntl.F_WRLCK, 0, 16))
        setlk = 'ACQUIRED (the other side\'s lock is invisible here)'
        fcntl.fcntl(fd, fcntl.F_SETLK, flock_struct(fcntl.F_UNLCK, 0, 16))
    except OSError as e:
        setlk = f'refused ({e.strerror})'
    os.close(fd)
    touch(os.path.join(run, f'{tag}.observed'))
    return {'getlk': getlk, 'getlk_pid': pid, 'setlk': setlk,
            'visible': getlk != 'unlocked' and setlk.startswith('refused')}


# --- phase 2: MAP_SHARED coherence ------------------------------------------------------------

def mmap_write(run, side, tag):
    path = os.path.join(run, 'mapfile')
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    os.ftruncate(fd, 4096)
    m = mmap.mmap(fd, 4096, mmap.MAP_SHARED)
    touch(os.path.join(run, f'{tag}.writing'))
    end = time.monotonic() + 3.0
    n = 0
    while time.monotonic() < end:
        n += 1
        m[0:8] = struct.pack('<Q', n)
        time.sleep(0.001)
    touch(os.path.join(run, f'{tag}.final'), str(n))
    wait_for(os.path.join(run, f'{tag}.sampled'))
    m.close()
    os.close(fd)
    return {'final': n}


def mmap_read(run, side, tag):
    wait_for(os.path.join(run, f'{tag}.writing'))
    fd = os.open(os.path.join(run, 'mapfile'), os.O_RDWR)
    m = mmap.mmap(fd, 4096, mmap.MAP_SHARED)
    samples = []
    while not os.path.exists(os.path.join(run, f'{tag}.final')):
        samples.append(struct.unpack('<Q', m[0:8])[0])
        time.sleep(0.01)
        os.listdir(run)
    time.sleep(0.2)
    last_seen = struct.unpack('<Q', m[0:8])[0]
    final = int(open(os.path.join(run, f'{tag}.final')).read())
    distinct = len(set(samples))
    m.close()
    os.close(fd)
    touch(os.path.join(run, f'{tag}.sampled'))
    return {'final_written': final, 'final_seen_after_200ms': last_seen,
            'distinct_values_seen_while_writing': distinct, 'samples': len(samples),
            'coherent': last_seen == final and distinct > 10}


# --- phase 3: SQLite stress -------------------------------------------------------------------

def connect(db):
    c = sqlite3.connect(db, timeout=5.0, isolation_level=None)
    c.execute('PRAGMA journal_mode=WAL')
    c.execute('PRAGMA synchronous=NORMAL')
    c.execute('PRAGMA busy_timeout=5000')
    return c


def writer(db, side, w, stop_at, out):
    c = connect(db)
    committed, errors = 0, []
    while time.monotonic() < stop_at:
        try:
            c.execute('BEGIN IMMEDIATE')
            c.execute('INSERT INTO rows(side, writer, n, pad) VALUES (?,?,?,?)',
                      (side, w, committed, os.urandom(random.randint(50, 3000))))
            c.execute('COMMIT')
            committed += 1
        except sqlite3.Error as e:
            errors.append(str(e))
            try:
                c.execute('ROLLBACK')
            except sqlite3.Error:
                pass
            if len(errors) > 200:
                break
    with open(out, 'w') as f:
        json.dump({'committed': committed, 'errors': errors[:20], 'error_count': len(errors)}, f)


def checkpointer(db, stop_at, out):
    c = connect(db)
    results = {}
    while time.monotonic() < stop_at:
        try:
            r = c.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone()
            results[str(r)] = results.get(str(r), 0) + 1
        except sqlite3.Error as e:
            results['error: ' + str(e)] = results.get('error: ' + str(e), 0) + 1
        time.sleep(0.25)
    with open(out, 'w') as f:
        json.dump(results, f)


def stress(run, side):
    db = os.path.join(run, 'stress.db')
    wait_for(os.path.join(run, 'db.created'))
    wait_for(os.path.join(run, 'stress.go'))
    stop_at = time.monotonic() + STRESS_SECONDS
    pids = []
    for w in range(WRITERS):
        pid = os.fork()
        if pid == 0:
            writer(db, side, w, stop_at, os.path.join(run, f'w-{side}-{w}.json'))
            os._exit(0)
        pids.append(pid)
    pid = os.fork()
    if pid == 0:
        checkpointer(db, stop_at, os.path.join(run, f'ckpt-{side}.json'))
        os._exit(0)
    pids.append(pid)
    for p in pids:
        os.waitpid(p, 0)
    touch(os.path.join(run, f'stress-{side}.done'))


def verify(run):
    db = os.path.join(run, 'stress.db')
    try:
        c = connect(db)
        integrity = [r[0] for r in c.execute('PRAGMA integrity_check').fetchall()]
    except sqlite3.DatabaseError as e:
        # Corruption is a RESULT here, not a crash: the first cross-kernel run died on this line
        # and lost the container side's lock and mmap measurements with it.
        return {'ok': False, 'integrity_check': [f'unreadable: {e}'], 'sides': {}}
    report = {'integrity_check': integrity[:10], 'sides': {}}
    ok = integrity == ['ok']
    for side in ('host', 'container'):
        for w in range(WRITERS):
            p = os.path.join(run, f'w-{side}-{w}.json')
            if not os.path.exists(p):
                report['sides'][f'{side}-{w}'] = 'NO RESULT FILE'
                ok = False
                continue
            claimed = json.load(open(p))
            rows = [r[0] for r in c.execute(
                'SELECT n FROM rows WHERE side=? AND writer=? ORDER BY n', (side, w))]
            want = list(range(claimed['committed']))
            missing = sorted(set(want) - set(rows))
            extra = sorted(set(rows) - set(want))
            dups = len(rows) - len(set(rows))
            report['sides'][f'{side}-{w}'] = {
                'committed': claimed['committed'], 'present': len(rows), 'missing': len(missing),
                'extra': len(extra), 'duplicates': dups, 'errors': claimed['error_count'],
                'sample_errors': claimed['errors'][:3]}
            if missing or extra or dups:
                ok = False
    report['ok'] = ok
    return report


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in ('host', 'container', 'selftest'):
        sys.exit('usage: sqlite-share-probe.py host|container|selftest <shared-dir>')
    side = sys.argv[1]
    if side == 'selftest':
        return selftest()
    other = 'container' if side == 'host' else 'host'
    os.makedirs(HERE, exist_ok=True)
    if side == 'container':
        try:
            os.remove(os.path.join(HERE, 'current'))  # a previous run's pointer must not be followed
        except OSError:
            pass
    touch(os.path.join(HERE, f'{side}.ready'), str(os.getpid()))
    say(side, f'waiting for the {other} side (run the other side with the same directory)')
    if not wait_for(os.path.join(HERE, f'{other}.ready')):
        sys.exit('the other side never arrived')
    # The container side owns the run directory, so both agree on one without clocks.
    if side == 'container':
        run = os.path.join(HERE, f'run-{int(time.time())}')
        os.makedirs(run)
        touch(os.path.join(HERE, 'current'), run.rsplit('/', 1)[1])
    else:
        wait_for(os.path.join(HERE, 'current'))
        time.sleep(0.2)
        run = os.path.join(HERE, open(os.path.join(HERE, 'current')).read().strip())
        wait_for(run)
    say(side, f'run directory {run}')
    results = {'side': side, 'platform': sys.platform, 'sqlite': sqlite3.sqlite_version}

    # phase 1, both directions: container holds first, then host holds.
    say(side, 'phase 1: fcntl locks')
    if side == 'container':
        lock_hold(run, side, 'lock-a')
        results['lock_seen_from_container'] = lock_observe(run, side, 'lock-b')
    else:
        results['lock_seen_from_host'] = lock_observe(run, side, 'lock-a')
        wait_for(os.path.join(run, 'lock-a.released'))
        lock_hold(run, side, 'lock-b')

    say(side, 'phase 2: MAP_SHARED mmap')
    if side == 'container':
        wait_for(os.path.join(run, 'lock-b.released'))
        mmap_write(run, side, 'map-a')
        results['mmap_seen_from_container'] = mmap_read(run, side, 'map-b')
    else:
        results['mmap_seen_from_host'] = mmap_read(run, side, 'map-a')
        mmap_write(run, side, 'map-b')

    say(side, f'phase 3: sqlite stress, {WRITERS} writers + checkpointer per side, {STRESS_SECONDS}s')
    if side == 'container':
        c = connect(os.path.join(run, 'stress.db'))
        c.execute('CREATE TABLE rows(seq INTEGER PRIMARY KEY AUTOINCREMENT, side TEXT, writer INT,'
                  ' n INT, pad BLOB, UNIQUE(side, writer, n))')
        c.close()
        touch(os.path.join(run, 'db.created'))
        wait_for(os.path.join(run, 'host.stress.ready'))
        touch(os.path.join(run, 'stress.go'))
    else:
        touch(os.path.join(run, 'host.stress.ready'))
    stress(run, side)
    if side == 'container':
        say(side, 'waiting for the host writers to finish')
        wait_for(os.path.join(run, 'stress-host.done'), timeout=300)
        time.sleep(1)
        results['stress'] = verify(run)
    touch(os.path.join(run, f'results-{side}.json'), json.dumps(results, indent=2))
    print(json.dumps(results, indent=2))
    for f in (f'{side}.ready',):
        try:
            os.remove(os.path.join(HERE, f))
        except OSError:
            pass


def selftest():
    """Both roles on ONE kernel: every phase must pass here, or the probe itself is broken."""
    pid = os.fork()
    if pid == 0:
        sys.argv = [sys.argv[0], 'host'] + sys.argv[2:]
        main()
        os._exit(0)
    sys.argv = [sys.argv[0], 'container'] + sys.argv[2:]
    main()
    os.waitpid(pid, 0)


if __name__ == '__main__':
    main()
