# knit

Knit is a version control system and a from-scratch alternative to Git, written in Rust. The whole repository lives in a single `.knit` file. There is no staging area: `knit tie` snapshots the working tree directly. Every knot is signed with Ed25519.

Owned by AXIOM. Specification: `knitspec.md` (parent dir) and `FORMAT.md` (file format).

## Status

Early development. Working: `cast-on`, `tie`, `count`, `rows`, `shear`, `pick`, `strand`, `switch`, `join`, `frog`, `lift`, `pickup` (git import), `mark`, `stash`, `sweep`, `track`, `loom`, `block`, `hub`, `send`, `gather`, `haul`, `key`. The `knits` binary serves bare yarnballs over stdio for ssh remotes.

## Usage

```sh
knit key gen                  # one-time: Ed25519 identity in ~/.config/knit/
knit cast-on [dir]            # create a yarnball (.knit file)
knit tie -knot "title" -yarn "description"   # snapshot the working tree
knit count                    # working-tree changes since last knot
knit rows                     # list knots on the current strand
knit shear                    # diff working tree vs HEAD
knit pick <knot>              # inspect a knot (verifies signature)
knit strand [name]            # list strands / create one at HEAD
knit switch <strand>          # change the active strand
knit join <strand>            # three-way merge; conflicts become snags
knit frog <knot>              # revert a knot (adds a reversing knot)
knit lift <knot>              # cherry-pick one knot onto this strand
knit pickup <path>            # import a git repo (worktree, .git, or bare)
knit mark [name] [knot]       # list marks / mark a knot (-d deletes)
knit stash [label]            # set work aside; `stash list`, `stash pop`
knit sweep                    # remove untracked files
knit track <file>             # knots that touched a file
knit loom <strand> <dir>      # materialize a strand into a separate dir (read-only)
knit block                    # compact the yarnball, keeping reachable objects
knit hub add|list|remove      # manage Knits remotes
knit send <hub>               # push the yarnball
knit gather <hub>             # pull new objects and strands
knit haul <hub>               # gather, then join the remote head strand
```

Hubs accept `file:///dir`, `/dir`, or `user@host[:dir]` (ssh, remote side runs
`knits`, which stores yarnballs under `KNITS_DIR` or `~/.knits`). The remote
repo name is the local directory name.

`.noyarn` files mark ignored paths (gitignore syntax). `~/.config/knit/ignore` is the global ignore.

## Layout

- `src/format.rs` - `.knit` file: header, frames, flock, crash-tolerant reads
- `src/object.rs` - canonical blob/tree/knot encodings, blake3 IDs
- `src/store.rs` - yarnball state: objects, strands, head, stat cache
- `src/worktree.rs` - scanning, ignores, stat cache, checkout
- `src/diff.rs` - tree diffs and line diffs
- `src/keys.rs` - Ed25519 identity
- `src/cli.rs` - commands
- `FORMAT.md` - `.knit` file format spec
