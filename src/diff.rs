//! Tree diffs and line diffs for `knit count`, `knit shear`, `knit pick`.

use crate::object::{ObjectId, Tree};
use anyhow::Result;
use similar::TextDiff;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug)]
pub struct TreeChange {
    pub path: String,
    pub status: Status,
    pub old_blob: Option<ObjectId>,
    pub new_blob: Option<ObjectId>,
}

/// Diff two trees (or a tree against the empty tree for "everything added").
pub fn tree_diff(old: Option<&Tree>, new: &Tree) -> Vec<TreeChange> {
    let mut changes = Vec::new();
    let old = old.cloned().unwrap_or_default();
    for (path, e) in new {
        match old.get(path) {
            None => changes.push(TreeChange {
                path: path.clone(),
                status: Status::Added,
                old_blob: None,
                new_blob: Some(e.blob_id),
            }),
            Some(o) if o.blob_id != e.blob_id || o.mode != e.mode => changes.push(TreeChange {
                path: path.clone(),
                status: Status::Modified,
                old_blob: Some(o.blob_id),
                new_blob: Some(e.blob_id),
            }),
            _ => {}
        }
    }
    for (path, e) in &old {
        if !new.contains_key(path) {
            changes.push(TreeChange {
                path: path.clone(),
                status: Status::Deleted,
                old_blob: Some(e.blob_id),
                new_blob: None,
            });
        }
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
}

/// Unified line diff between two byte blobs, rendered like `git diff`.
pub fn blob_diff(path: &str, old: &[u8], new: &[u8]) -> String {
    let old_text = String::from_utf8_lossy(old);
    let new_text = String::from_utf8_lossy(new);
    let diff = TextDiff::from_lines(&old_text, &new_text);
    let mut out = String::new();
    out.push_str(&format!("diff --knit a/{path} b/{path}\n"));
    out.push_str(&diff.unified_diff().header("a/", "b/").to_string());
    out
}

/// Diff for a single tree change, fetching blobs from the store.
pub fn change_diff(
    store: &crate::store::Yarnball,
    change: &TreeChange,
) -> Result<String> {
    let old = match change.old_blob {
        Some(id) => store.blob_bytes(&id).unwrap_or_default(),
        None => Vec::new(),
    };
    let new = match change.new_blob {
        Some(id) => store.blob_bytes(&id).unwrap_or_default(),
        None => Vec::new(),
    };
    // Binary sniff: skip diff if either side has NUL bytes.
    if old.contains(&0) || new.contains(&0) {
        return Ok(format!(
            "Binary files differ for {}\n",
            change.path
        ));
    }
    Ok(blob_diff(&change.path, &old, &new))
}

/// Hide a trailing Knits footer in a stored message for display.
pub const FOOTER: &str = "Made-with: Knit (axiomwtf.github.io/knit)";

pub fn hide_footer(message: &str) -> &str {
    let trimmed = message.trim_end_matches('\n');
    if trimmed.ends_with(FOOTER) {
        trimmed[..trimmed.len() - FOOTER.len()].trim_end()
    } else {
        message
    }
}
