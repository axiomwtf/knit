# .knit File Format

Version 1. A yarnball is one `.knit` file containing the entire history and
working-tree state. No sidecars: no index file, no lock file, no object store.

## Layout

```
┌──────────────────────────────┐
│ Header (32 bytes, fixed)     │
├──────────────────────────────┤
│ Record frame                 │
│ Record frame                 │
│ ...                          │
└──────────────────────────────┘
```

The file only ever grows. Records are appended; nothing is rewritten in place.
Compaction (`knit block`) may rewrite storage later but never knot IDs or
history.

## Header (32 bytes, little-endian)

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 4 | `magic` | `b"KNIT"` |
| 4 | 4 | `version` | Format version (current: 1) |
| 8 | 8 | `flags` | Reserved, must be 0 |
| 16 | 16 | `reserved` | Zero padding |

## Record frame

Every record is one frame:

| Field | Size | Description |
|-------|------|-------------|
| `payload_len` | u32 LE | Length of `payload` in bytes |
| `kind` | u8 | Record type (see below) |
| `payload` | `payload_len` bytes | Record body |
| `crc32` | u32 LE | crc32 of `kind` byte + `payload` |

Frame header is 5 bytes; trailer is 4 bytes.

### Crash safety

A crash can leave a partial frame at EOF. Readers MUST stop at the first frame
whose length, CRC, or bounds are invalid and treat everything before it as
the valid file. Writers append with `write_all` + `fsync` (`sync_data`).

### Single writer

A writer MUST hold an exclusive `flock(2)` on the `.knit` file for the whole
mutation. Readers hold a shared lock. The lock is on the file itself; there is
no lock file.

## Record kinds

| Kind | Name | Payload |
|------|------|---------|
| 0x01 | `BLOB` | Raw file content. Object ID = blake3(payload). |
| 0x02 | `TREE` | Canonical tree (below). Object ID = blake3(payload). |
| 0x03 | `KNOT` | Canonical knot (below). Object ID = blake3(payload). |
| 0x04 | `REF` | Strand pointer (below). |
| 0x05 | `HEAD` | Active strand name (knit string). |
| 0x06 | `INDEX` | Working-tree stat cache (below). |
| 0x07 | `MARK` | Named mark on a knot (below). Latest op per name wins. |
| 0x08 | `STASH` | Stash entry (below). |
| 0x09 | `TOMBSTONE` | Reserved: emergency-purge marker (future). |
| 0x0A | `KEY` | 32-byte Ed25519 public key. Appended the first time a key signs a knot; the set of KEY records is the yarnball's known signers. |
| 0x0B | `JOIN` | Pending/resolved join state (below). Latest record wins. |
| 0x0C | `HUB` | Knits remote (below). Empty url removes the remote. |
| 0x0D | `STASH_DROP` | Drops a stash entry by id (below). |

### JOIN payload

```
u8            state         (0 = pending, 1 = resolved)
u32           parent_count
repeat parent_count times:
  obj id      extra_parent  (knot ids beyond HEAD for the next knot)
knit string   label         (e.g. "join feature", "frog a1b2c3")
```

A pending JOIN means a `join`/`frog`/`lift` hit snags: the worktree holds
conflict markers and the next `knit tie` produces a knot whose parents are
HEAD plus `extra_parents`, after which a resolved JOIN record is appended.

### MARK payload

```
u8            op             (0 = add, 1 = delete)
knit string   name
obj id        knot_id        (zeroed on delete)
```

### STASH payload

```
obj id        tree_id        (snapshot of the worktree at stash time)
knit string   label
u64           timestamp_secs
```

The entry's id is `blake3(payload)`; `STASH_DROP` carries that id and removes
the entry. Live stashes are STASH records minus dropped ids.

### STASH_DROP payload

```
obj id        stash_id       (blake3 of the STASH payload)
```

### HUB payload

```
knit string   name
knit string   url            ("" removes; file:///dir, /dir, or user@host[:dir])
```

Object records (`BLOB`, `TREE`, `KNOT`) are content-addressed: their ID is the
blake3 of the payload. Duplicate objects MUST NOT be appended; writers check
the ID set first.

`REF`, `HEAD`, `INDEX`, and future state records are append-only logs: the
latest record of each kind/subject wins. Older records stay in the file.

## Canonical encodings

All integers little-endian. A "knit string" is `u32 length` + UTF-8 bytes.
An "object id" is 32 raw bytes (blake3 digest).

### TREE payload

```
u32 entry_count
repeat entry_count times, sorted by name (byte order):
  knit string  path        (relative, '/'-separated, no leading './')
  u32          mode        (git-style: 0o100644, 0o100755, 0o120000)
  obj id       blob_id
```

The tree is a flat map of every file in the snapshot. Nested git trees are
reconstructed at export time.

### KNOT payload

```
obj id        tree_id
u32           parent_count
repeat parent_count times:
  obj id      parent_knot_id
author        knit string    ("Name <email>")
u64           timestamp_secs
u32           timestamp_nanos
knit string   message        (stored without any Knits footer)
knit string   git_id         (hex SHA-1, empty for native knots)
64 bytes      signature      (Ed25519)
```

- The signature covers every byte before the signature field.
- `git_id` empty means the knot is native; set means imported.
- Knot ID = blake3 of the whole payload (including signature).

### REF payload

```
knit string   strand_name
obj id        knot_id
```

### HEAD payload

```
knit string   strand_name
```

### INDEX payload (stat cache)

```
u32 entry_count
repeat entry_count times, sorted by path:
  knit string  path
  u64          mtime_secs
  u32          mtime_nanos
  u64          size
  u32          mode
  obj id       blob_id
```

The cache lets `knit tie` skip re-hashing unchanged files. It is a hint only:
a cache miss or mismatch always falls back to hashing.
