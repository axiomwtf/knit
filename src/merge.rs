//! Three-way tree merge for `knit join`, `knit frog`, `knit lift`.

use crate::object::{ObjectId, Tree, TreeEntry};
use std::collections::{HashMap, VecDeque};

#[derive(Debug)]
pub struct Conflict {
    pub path: String,
    pub base: Option<ObjectId>,
    pub ours: Option<ObjectId>,
    pub theirs: Option<ObjectId>,
}

pub struct MergeOutcome {
    /// Merged entries. Conflicted paths are absent; callers write markers.
    pub tree: Tree,
    pub conflicts: Vec<Conflict>,
}

fn same(a: Option<&TreeEntry>, b: Option<&TreeEntry>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.blob_id == y.blob_id && x.mode == y.mode,
        _ => false,
    }
}

/// Merge `ours` and `theirs` against `base`. All trees are flat path maps.
pub fn merge_trees(base: Option<&Tree>, ours: &Tree, theirs: &Tree) -> MergeOutcome {
    let base = base.cloned().unwrap_or_default();
    let mut paths: Vec<&String> = ours.keys().chain(theirs.keys()).chain(base.keys()).collect();
    paths.sort();
    paths.dedup();

    let mut tree = Tree::new();
    let mut conflicts = Vec::new();

    for path in paths {
        let b = base.get(path);
        let o = ours.get(path);
        let t = theirs.get(path);

        if same(o, t) {
            if let Some(e) = o {
                tree.insert(path.clone(), e.clone());
            }
            continue;
        }
        if same(o, b) {
            // Only theirs changed.
            if let Some(e) = t {
                tree.insert(path.clone(), e.clone());
            }
            continue;
        }
        if same(t, b) {
            // Only ours changed.
            if let Some(e) = o {
                tree.insert(path.clone(), e.clone());
            }
            continue;
        }
        // Both sides changed differently: snag.
        conflicts.push(Conflict {
            path: path.clone(),
            base: b.map(|e| e.blob_id),
            ours: o.map(|e| e.blob_id),
            theirs: t.map(|e| e.blob_id),
        });
    }

    MergeOutcome { tree, conflicts }
}

/// Render conflict-marker content for a snaged path.
pub fn conflict_content(
    store: &crate::store::Yarnball,
    c: &Conflict,
) -> Vec<u8> {
    let side = |id: Option<ObjectId>| -> Vec<u8> {
        match id {
            Some(id) => store.blob_bytes(&id).unwrap_or_default(),
            None => Vec::new(),
        }
    };
    let ours = side(c.ours);
    let theirs = side(c.theirs);
    let mut out = Vec::new();
    out.extend_from_slice(b"<<<<<<< ours\n");
    out.extend_from_slice(&ours);
    if !ours.ends_with(b"\n") && !ours.is_empty() {
        out.push(b'\n');
    }
    out.extend_from_slice(b"=======\n");
    out.extend_from_slice(&theirs);
    if !theirs.ends_with(b"\n") && !theirs.is_empty() {
        out.push(b'\n');
    }
    out.extend_from_slice(b">>>>>>> theirs\n");
    out
}

/// BFS distances from `start` over all parents.
pub fn ancestor_depths(
    y: &crate::store::Yarnball,
    start: ObjectId,
) -> HashMap<ObjectId, usize> {
    let mut depth = HashMap::new();
    let mut queue = VecDeque::new();
    depth.insert(start, 0);
    queue.push_back(start);
    while let Some(id) = queue.pop_front() {
        let d = depth[&id];
        if let Ok(knot) = y.knot(&id) {
            for p in knot.parents {
                if !depth.contains_key(&p) {
                    depth.insert(p, d + 1);
                    queue.push_back(p);
                }
            }
        }
    }
    depth
}

/// Nearest common ancestor of two knots: BFS from `b` until hitting a knot
/// that is an ancestor of `a`. Returns None if unrelated.
pub fn merge_base(
    y: &crate::store::Yarnball,
    a: ObjectId,
    b: ObjectId,
) -> Option<ObjectId> {
    if a == b {
        return Some(a);
    }
    let a_ancestors = ancestor_depths(y, a);
    let mut queue = VecDeque::new();
    let mut seen = std::collections::HashSet::new();
    queue.push_back((b, 0usize));
    seen.insert(b);
    while let Some((id, _)) = queue.pop_front() {
        if a_ancestors.contains_key(&id) {
            return Some(id);
        }
        if let Ok(knot) = y.knot(&id) {
            for p in knot.parents {
                if seen.insert(p) {
                    queue.push_back((p, 0));
                }
            }
        }
    }
    None
}
