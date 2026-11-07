//! Working-tree scanning, .noyarn ignores, stat cache, checkout.

use crate::object::{self, CacheEntry, ObjectId, Tree, TreeEntry};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

const MODE_FILE: u32 = 0o100644;
const MODE_EXE: u32 = 0o100755;
const MODE_LINK: u32 = 0o120000;

fn git_mode(md: &fs::Metadata) -> Option<u32> {
    let ft = md.file_type();
    if ft.is_symlink() {
        Some(MODE_LINK)
    } else if ft.is_file() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if md.permissions().mode() & 0o111 != 0 {
                Some(MODE_EXE)
            } else {
                Some(MODE_FILE)
            }
        }
        #[cfg(not(unix))]
        {
            Some(MODE_FILE)
        }
    } else {
        None // dirs handled separately, special files skipped
    }
}

fn global_ignore_matcher(root: &Path) -> Option<ignore::gitignore::Gitignore> {
    let path = dirs::config_dir()?.join("knit").join("ignore");
    if !path.exists() {
        return None;
    }
    let mut b = ignore::gitignore::GitignoreBuilder::new(root);
    b.add(path);
    b.build().ok()
}

/// Walk the working tree and produce a snapshot tree plus a fresh stat cache.
/// `cache` is the previous INDEX contents; entries that still match are not
/// re-hashed. Returns (tree, new_cache, blobs_to_append).
pub fn scan(
    root: &Path,
    cache: &BTreeMap<String, CacheEntry>,
) -> Result<(Tree, BTreeMap<String, CacheEntry>, Vec<(ObjectId, Vec<u8>)>)> {
    let mut tree = Tree::new();
    let mut new_cache = BTreeMap::new();
    let mut blobs: Vec<(ObjectId, Vec<u8>)> = Vec::new();
    let global = global_ignore_matcher(root);

    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false) // dotfiles are tracked; .knit/.git excluded manually
        .follow_links(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .add_custom_ignore_filename(".noyarn");

    for result in builder.build() {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path == root {
            continue;
        }
        let rel = match path.strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        // The yarnball itself and any .git dir are never part of a snapshot.
        if rel_str == ".knit" || rel_str == ".git" || rel_str.starts_with(".git/") {
            continue;
        }
        if let Some(g) = &global {
            if g.matched(rel, entry.file_type().map(|t| t.is_dir()).unwrap_or(false)).is_ignore() {
                continue;
            }
        }

        let md = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let Some(mode) = git_mode(&md) else { continue };

        // Stat-cache fast path for non-symlink entries.
        let mut blob_id = None;
        if mode != MODE_LINK {
            let (secs, nanos) = mtime_parts(&md);
            if let Some(c) = cache.get(&rel_str) {
                if c.mtime_secs == secs
                    && c.mtime_nanos == nanos
                    && c.size == md.len()
                    && c.mode == mode
                {
                    blob_id = Some(c.blob_id);
                }
            }
        }

        let content = if mode == MODE_LINK {
            fs::read_link(path)
                .map(|t| t.as_os_str().as_encoded_bytes().to_vec())
                .unwrap_or_default()
        } else {
            match fs::read(path) {
                Ok(b) => b,
                Err(_) => continue,
            }
        };

        let id = match blob_id {
            Some(id) => id,
            None => {
                let id = object::object_id(&content);
                blobs.push((id, content));
                id
            }
        };

        tree.insert(
            rel_str.clone(),
            TreeEntry { path: rel_str.clone(), mode, blob_id: id },
        );

        if mode != MODE_LINK {
            let (secs, nanos) = mtime_parts(&md);
            new_cache.insert(
                rel_str,
                CacheEntry { mtime_secs: secs, mtime_nanos: nanos, size: md.len(), mode, blob_id: id },
            );
        }
    }

    Ok((tree, new_cache, blobs))
}

fn mtime_parts(md: &fs::Metadata) -> (u64, u32) {
    use std::os::unix::fs::MetadataExt;
    (md.mtime() as u64, md.mtime_nsec() as u32)
}

/// Write all files of `tree` into `root`, deleting paths in `old` that are
/// missing from `tree`. Leaves ignored/untracked files alone by only
/// deleting paths that existed in `old`.
pub fn materialize(
    root: &Path,
    store: &crate::store::Yarnball,
    tree: &Tree,
    old: Option<&Tree>,
) -> Result<()> {
    if let Some(old) = old {
        for (path, e) in old {
            if !tree.contains_key(path) {
                let p = root.join(path);
                if p.symlink_metadata().is_ok() {
                    if e.mode == MODE_LINK || p.is_dir() {
                        let _ = fs::remove_dir_all(&p);
                    } else {
                        let _ = fs::remove_file(&p);
                    }
                }
                // prune now-empty parent dirs
                let mut parent = p.parent().map(|x| x.to_path_buf());
                while let Some(d) = parent {
                    if d == root {
                        break;
                    }
                    if fs::remove_dir(&d).is_err() {
                        break;
                    }
                    parent = d.parent().map(|x| x.to_path_buf());
                }
            }
        }
    }

    for (path, e) in tree {
        let dest = root.join(path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        if e.mode == MODE_LINK {
            let target = store.blob_bytes(&e.blob_id)
                .context("missing blob for symlink")?;
            let target_os = String::from_utf8_lossy(&target);
            if dest.symlink_metadata().is_ok() {
                let _ = fs::remove_file(&dest);
            }
            std::os::unix::fs::symlink(target_os.as_ref(), &dest)?;
            continue;
        }
        let bytes = store.blob_bytes(&e.blob_id)
            .with_context(|| format!("missing blob {} for {}", hex::encode(e.blob_id), path))?;
        let needs_write = match fs::read(&dest) {
            Ok(existing) => object::object_id(&existing) != e.blob_id,
            Err(_) => true,
        };
        if needs_write {
            let mut f = fs::File::create(&dest)?;
            f.write_all(&bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perm = if e.mode == MODE_EXE { 0o755 } else { 0o644 };
                let _ = f.set_permissions(fs::Permissions::from_mode(perm));
            }
        }
    }
    Ok(())
}

