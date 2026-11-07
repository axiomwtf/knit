# Knit Version Control Specification
**Version:** 0.1.0
**Ownership:** Knit is owned by AXIOM ![Axiom Logo](https://axiomwtf.github.io/axiom.svg)
**Status:** Specification draft.
**License:** MPL-2.0
**Last updated:** 2026-11-06
---

## 1. Overview

Knit is a version control system and a from-scratch alternative to Git, written in Rust. It is not a wrapper and does not depend on Git anywhere in its implementation. Git is used only as an external test checker (`git fsck`) during development.

The whole repository lives in a single `.knit` file. There is no `.git`-style sidecar mess, in line with the Axiom "no sidecars" rule. There is no staging area: `knit tie` snapshots the working tree directly.

Every knot is signed with Ed25519, matching the vfsx and Poly signing scheme.

**Knits** is the bridge to the Git world. The client pushes over SSH and Knits turns knit history into a valid Git packfile and sends it to GitHub, with no `git push` involved. `knit pickup` and `knit fork` do the reverse.

---

## 2. Vocabulary

Knit uses knitting-based names for its own concepts. Anything that touches the Git side, including the export format and Knits, must say "Git commit" explicitly so the two never blur.

| Git term | Knit term |
|---|---|
| repository | yarnball |
| commit | knot |
| branch | strand |
| merge | join |
| conflict | snag |
| remote | Knits |

`knit tie` is the command that ties a knot (`knit knot` and `knit save` are aliases). `knit frog` is the undo command: it adds a knot that reverses an earlier one and never rewrites history.

---

## 3. The `.knit` File

- A yarnball is one `.knit` file containing the entire history and working-tree state.
- No sidecar files: no index file, no lock file on disk, no separate object store.
- Crash-safe writes are required.
- A single-writer lock is required.
- The "no sidecars" rule means the file-stat cache used to make `knit tie` fast must live inside the `.knit` file itself.
- Storage format is TBD: either a custom append-only file with an internal index (vfsx-style) or embedded SQLite. Content-defined chunking, deduplication, and zstd compression are under consideration and reuse vfsx ideas.
- The file only ever grows. Compaction is allowed but must rewrite storage only, never knot IDs or history.

**Ignore file:** `.noyarn` marks paths the yarnball ignores, using gitignore-compatible syntax. The name comes from the material model: a yarnball is the repo, yarn is what gets knitted, and a path marked `.noyarn` is a dead zone with no yarn to knit. `.noyarn` lives in the working tree, is itself committable, and supports per-directory files plus a global ignore in the Knits config dir.

---

## 4. Knots

A knot is knit's equivalent of a Git commit, but with similar fields, not identical ones.

Fields:

| Field | Description |
|---|---|
| tree | Reference to the tree object for the snapshot |
| parents | Parent knot IDs (more than one for joins) |
| author | Author identity |
| timestamp | Creation time |
| message | Commit message, stored without the Knits footer |
| signature | Ed25519 signature over the knot |
| git_id | Original Git object ID, set only for imported commits |

A knot is **native** when `git_id` is empty. A knot is **imported** when `git_id` is set. No separate flag is needed.

Every knot is signed with Ed25519 at creation. The signature is knit-native only: it is verified on pickup and inspect, but it cannot produce a GitHub "Verified" badge (see Section 7).

Imported commits are stored exactly as they came from Git, with their original Git ID kept.

---

## 5. Immutability

Knit history is immutable. There is no rewriting of history.

Removed operations: amend, rebase, squash-in-place, `reset --hard` on history, and force push. A knot's ID and signature never change once tied. Strands can only move forward, so pushing is always a fast-forward.

Consequences:

- **Undoing** cannot remove a knot, only add a new one that reverses it. The command is `knit frog`.
- **Messy local work:** a half-finished knot stays forever. Squashing a whole strand into one new knot at join time is allowed, because the strand itself stays intact.
- **Typos in messages** cannot be fixed. An annotation may attach a note to a knot without changing it.
- **Deleting strands:** hiding a strand from listings is allowed. The knots stay in the file.
- **GitHub force pushes:** knit treats upstream divergence as separate strands and tells the user, never overwriting either side.

---

## 6. Emergency Removal Hatch

Immutability has exactly one exception: a narrow, human-run emergency hatch.

Policy:

- Removal is requested through a GitHub issue, with sensitive details handled over DM. The issue itself must carry no secrets.
- Ownership must be verified before any purge, for example by a message signed by a key the repo already trusts or by control of the linked GitHub repository.
- "Emergency" is defined narrowly: leaked credentials, illegal content, and valid legal orders. Typos, embarrassing messages, and changes of mind are refused. Rotating a leaked credential always comes first.
- A purge rewrites the `.knit` file without the targeted bytes and replaces it atomically, and removes the data from the Knits copy.
- Hashes and the knot chain stay, so signatures still verify. Missing content is marked with a signed tombstone recording the date and a reason code.
- On the Git side, Knits cannot rebuild the original Git blob, so export uses a fixed placeholder and Git IDs change from that point on. This is a rewrite on the Git side only.
- The hatch only reaches what it controls: the user's `.knit` file and the Knits copy. Other clones, laptops, and backups keep the data until their owners run the same purge. Docs must say "removed from the file," never "unrecoverable."
- Who may request a purge (owner only, or also third parties such as copyright holders or courts) is TBD.

---

## 7. Knits and Git Export

Knits turns knit history into real Git objects and pushes them to GitHub over SSH. It does not invoke `git`.

**Footer rule:**

- Knits appends one fixed footer to the commit message of every **native** knot at export time.
- Imported commits are exported byte for byte and never receive a footer.
- If an imported commit already ends with the footer, it is recognized and never receives a second one.
- If a native knot is pushed and later picked up again, it returns as an imported knot with the footer in its message and stays as is.
- The footer is a frozen constant. Changing one byte later would change the Git ID of every native commit ever pushed.
- `knit rows` may hide a trailing footer when displaying a message. This is display-only and does not change stored data.

**Footer constant:** the footer is frozen forever. Appended to the knot's stored message at export time, its exact bytes are a blank line, the trailer line below, and a trailing newline:

```
Made-with: Knit (axiomwtf.github.io/knit)
```

The trailer form follows `git interpret-trailers` convention and is machine-readable for `knit rows` hiding and import detection. This constant never changes.

**Signatures:**

- The Ed25519 signature covers knit-native data. Knits adds the footer after the fact, so a Git signature over the exported bytes is impossible without Knits holding the user's private key.
- Therefore no knit signature appears as GitHub "Verified." This is intended, not a defect.

**Mapping:**

- For native knots, the client can compute the Git commit ID locally: translate knit fields to Git's layout, append the fixed footer, and hash. Knits produces the same bytes, so the two always agree.
- For imported commits, the original Git ID is stored in `git_id` and must survive round trips exactly.
- Git's awkward cases must be preserved on import and export: file modes, symlinks, submodules, merge commits, odd encodings, and tags.

**Transport:**

- All Knits communication is SSH only. There is no HTTPS API, no HTTP transport, and no unauthenticated channel. Push, pickup, gather, and fork all go through the SSH connection.
- The default Knits service is `knot@axiom.local`, run by Axiom. It has no interface of its own: it is CLI only over SSH, and its only job is translating knit history into Git and pushing it to GitHub. It does not host a browsable repo UI. (`*.local` only resolves on local networks, so a public hostname for the default service is TBD.)
- Pushing to GitHub as the user requires credentials on the Knits side, likely via SSH agent forwarding. Knits is therefore not fully stateless.
- Auth model is SSH keys only, matching the Ed25519 identity used for knot signatures where possible.
- Auth model details and hosted domain are TBD.

---

## 8. Knits Service Framework

Knits is an open framework: any developer or team can run their own Knits hub for their repos, the same way anyone can host a Git server. Axiom runs a default hub at `knot@axiom.local`, but nothing in the protocol requires it.

**What a hub is:**

- An SSH endpoint that accepts knit clients (`knot@<host>`), authenticates by SSH key, and stores bare yarnballs.
- A bare yarnball is a `.knit` file on the server, the same format as a local repo. The file format must therefore be safe for concurrent readers and network transfer.
- The hub speaks a knit-native exchange protocol over the SSH channel. It does not speak git protocol and does not require `git` on either side.
- A hub may additionally offer a web UI (repo list, rows, diffs, snags) but the UI is optional; SSH is the only required interface. The default Axiom hub at `knot@axiom.local` deliberately has no UI at all: it is a CLI-only Git translation bridge.

**What the framework defines:**

- The SSH transport and auth model.
- The exchange protocol for uploading and downloading knot objects.
- The bare yarnball storage layout.
- The mapping rules that turn knots into Git objects for GitHub export.

**What the framework does not define:**

- Web UI design, accounts beyond SSH keys, billing, or moderation.
- Whether a hub also exports to GitHub. A hub that only stores native knots is valid.

Reference implementation status: TBD. A minimal `knits serve` daemon is planned after the local core and the `.knit` format are stable.

---

## 9. Commands

The CLI uses knitting vocabulary throughout. Commands deliberately absent: `add`, `reset`, `rebase`, `amend`, and anything else that rewrites knots.

| Git equivalent | Knit command | Purpose |
|---|---|---|
| `init` | `knit cast-on` | Create a new yarnball |
| `commit` | `knit tie` | Snapshot the working tree as a new knot (`knit knot`, `knit save` aliases) |
| `status` | `knit count` | Show working-tree changes since the last knot |
| `log` | `knit rows` | List knots on the current strand |
| `diff` | `knit shear` | Show changes between knots or working tree |
| `show` | `knit pick <knot>` | Inspect one knot's message and diff |
| `branch` | `knit strand` | List, create, and manage strands |
| `switch` / `checkout` | `knit switch <strand>` | Change the active strand |
| `merge` | `knit join` | Join a strand back; conflicts are snags |
| `revert` | `knit frog <knot>` | Add a knot reversing an earlier one |
| `stash` | `knit stash` | Set work aside without a knot |
| `tag` | `knit mark` | Mark a knot with a name |
| `remote` | `knit hub` | Manage Knits remotes (`add`, `list`, `remove`) |
| `clone` | `knit pickup` | Clone via Knits or git import |
| `fetch` | `knit gather` | Pull new objects down |
| `pull` | `knit haul` | Gather plus join |
| `push` | `knit send` | Push to Knits |
| `blame` | `knit track <file>` | Trace which knots touched a file |
| `cherry-pick` | `knit lift <knot>` | Lift one knot onto the current strand |
| `worktree` | `knit loom` | Separate working surface on another strand |
| `gc` | `knit block` | Compact storage; never rewrites history |
| `clean` | `knit sweep` | Remove untracked files |
| key management | `knit key` | Generate and export Ed25519 keys |

Naming clash to resolve: `knit fork` has meant both a strand fork and a GitHub fork. These need separate commands. `knit pickup` is now the clone command, so `knit fork` is free to mean the GitHub fork once a strand fork gets its own name.

### 8.1 `knit tie` arguments

```
knit tie -knot "title" -yarn "description"
```

- `-knot` is the short title, required, single line.
- `-yarn` is the longer description, optional, repeatable; repeated values join with a blank line.
- With no flags, `knit tie` opens `$EDITOR` with both fields.
- `-knot` rejects newline escapes. `-yarn` supports the argument substitution syntax below.
- `knit tie` always snapshots the full working tree. There are no partial or per-path commits; everything not covered by `.noyarn` goes into the knot.

### 8.2 Argument substitution syntax

Argument values support a small substitution syntax:

| Syntax | Expands to |
|---|---|
| `\n` | newline |
| `$(line)` | newline |
| `$(newline[content])` | newline followed by `content` |

This applies to `-yarn` and any other free-text argument. It does not apply to `-knot`.

---

## 10. Milestones

1. **Spec:** this document, the file format, object model, and Git mapping.
2. **Local core:** `cast-on`, `tie`, `count`, `rows`, `shear`, `pick`, and `strand`/`switch`.
3. **Merge:** diff algorithm, three-way join, snag handling.
4. **Git import:** read Git packfiles including delta resolution, so `knit pickup` works.
5. **Git export:** packfile writer (the hardest part).
6. **Knits framework:** SSH transport, exchange protocol, bare yarnball storage, and a minimal reference `knits serve` daemon that third parties can implement against.
7. **Signing and verification polish.**

Import comes before export on purpose: real repos then serve as the test. Pickup, import, export, and check that every object ID matches.

---

## 11. Open Questions

- File format: custom append-only vs embedded SQLite.
- Strand fork vs GitHub fork command names (`knit pickup` is the clone command).
- Purge mechanics: exact hashing coverage, placeholder format for Git export.
- Who may request an emergency purge.
- Knits exchange protocol details and reference implementation scope.
- Whether conflicts are stored as first-class data in history or only shown in the working tree.
- Large-file strategy: content-defined chunking details.
- Strand fork vs GitHub fork command names (`knit fork` naming split; `knit pickup` is the clone command).

---


---

## Appendix: The .knit file format

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

A TREE payload may instead be a **delta** ("same parts"): only the entries
that differ from a base tree are stored. The writer picks the smaller of the
two encodings, and reuses the base tree id outright when nothing changed.

```
u32           0xFFFFFFFF       (delta marker; no real entry count can be this)
obj id        base_tree_id     (may itself be a delta; chains resolve recursively)
u32           change_count
repeat change_count times, sorted by path:
  knit string  path
  u32          mode            (0 = deleted)
  obj id       blob_id         (omitted when mode is 0)
```

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

An INDEX payload may also be a **delta** against the cache left by the
previous INDEX record, storing only changed entries. Deltas apply in frame
order, so readers resolve them while loading.

```
u32           0xFFFFFFFE       (delta marker)
u32           change_count
repeat change_count times, sorted by path:
  knit string  path
  u8           op              (0 = remove, 1 = put)
  if op == 1:
    u64        mtime_secs
    u32        mtime_nanos
    u64        size
    u32        mode
    obj id     blob_id
```


*End of Specification*
