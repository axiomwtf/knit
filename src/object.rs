//! Canonical object encodings and blake3 object IDs.
//! See knitspec.md (appendix) for the byte layouts.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;

pub type ObjectId = [u8; 32];

pub fn object_id(payload: &[u8]) -> ObjectId {
    *blake3::hash(payload).as_bytes()
}

// ---------- primitives ----------

pub fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn put_id(out: &mut Vec<u8>, id: &ObjectId) {
    out.extend_from_slice(id);
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            bail!("truncated object payload");
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn id(&mut self) -> Result<ObjectId> {
        Ok(self.take(32)?.try_into().unwrap())
    }
    fn string(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        let bytes = self.take(n)?;
        String::from_utf8(bytes.to_vec()).context("invalid UTF-8 string in object")
    }
    fn done(&self) -> Result<()> {
        if self.pos != self.buf.len() {
            bail!("trailing bytes in object payload");
        }
        Ok(())
    }
}

// ---------- tree ----------

/// One file entry in a tree snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: String,
    pub mode: u32,
    pub blob_id: ObjectId,
}

/// A tree is a flat map of path -> entry. BTreeMap keeps canonical order.
pub type Tree = BTreeMap<String, TreeEntry>;

pub fn encode_tree(tree: &Tree) -> Vec<u8> {
    let mut out = Vec::new();
    put_u32(&mut out, tree.len() as u32);
    for (path, e) in tree {
        put_str(&mut out, path);
        put_u32(&mut out, e.mode);
        put_id(&mut out, &e.blob_id);
    }
    out
}

pub fn decode_tree(payload: &[u8]) -> Result<Tree> {
    let mut r = Reader { buf: payload, pos: 0 };
    let n = r.u32()?;
    let mut tree = Tree::new();
    for _ in 0..n {
        let path = r.string()?;
        let mode = r.u32()?;
        let blob_id = r.id()?;
        tree.insert(path.clone(), TreeEntry { path, mode, blob_id });
    }
    r.done()?;
    Ok(tree)
}

// ---------- tree deltas ("same parts") ----------
//
// A TREE payload may be a delta: only the entries that differ from a base
// tree are stored. The base is another tree object id, which may itself be
// a delta, so chains resolve recursively. The first u32 is TREE_DELTA
// (0xFFFFFFFF), a value no real entry count can have.

pub const TREE_DELTA: u32 = 0xFFFF_FFFF;

/// One changed path: mode 0 means deleted.
pub type TreeDelta = Vec<(String, u32, ObjectId)>;

/// If `payload` is a delta, return its base tree id.
pub fn tree_delta_base(payload: &[u8]) -> Option<ObjectId> {
    if payload.len() >= 36 && u32::from_le_bytes(payload[..4].try_into().unwrap()) == TREE_DELTA {
        Some(ObjectId::try_from(&payload[4..36]).unwrap())
    } else {
        None
    }
}

/// Encode `new` as a delta against `base` (resolved trees). Returns the
/// payload only if it is smaller than the full encoding.
pub fn encode_tree_delta(base_id: &ObjectId, base: &Tree, new: &Tree) -> Option<Vec<u8>> {
    // path -> new entry, or None for deletion. BTreeMap keeps canonical order.
    let mut changes: BTreeMap<&str, Option<&TreeEntry>> = BTreeMap::new();
    for (p, e) in new {
        if base.get(p) != Some(e) {
            changes.insert(p, Some(e));
        }
    }
    for p in base.keys() {
        if !new.contains_key(p) {
            changes.insert(p.as_str(), None);
        }
    }
    if changes.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    put_u32(&mut out, TREE_DELTA);
    put_id(&mut out, base_id);
    put_u32(&mut out, changes.len() as u32);
    for (path, entry) in changes {
        put_str(&mut out, path);
        match entry {
            Some(e) => {
                put_u32(&mut out, e.mode);
                put_id(&mut out, &e.blob_id);
            }
            None => put_u32(&mut out, 0),
        }
    }
    if out.len() >= encode_tree(new).len() {
        return None;
    }
    Some(out)
}

/// Decode a delta payload into (base_id, changes).
pub fn decode_tree_delta(payload: &[u8]) -> Result<(ObjectId, TreeDelta)> {
    let mut r = Reader { buf: payload, pos: 0 };
    let marker = r.u32()?;
    if marker != TREE_DELTA {
        bail!("not a tree delta");
    }
    let base_id = r.id()?;
    let n = r.u32()?;
    let mut changes = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let path = r.string()?;
        let mode = r.u32()?;
        let blob_id = if mode == 0 { ObjectId::default() } else { r.id()? };
        changes.push((path, mode, blob_id));
    }
    r.done()?;
    Ok((base_id, changes))
}

// ---------- knot ----------

#[derive(Debug, Clone)]
pub struct Knot {
    pub tree_id: ObjectId,
    pub parents: Vec<ObjectId>,
    pub author: String,
    pub timestamp_secs: u64,
    pub timestamp_nanos: u32,
    pub message: String,
    pub git_id: String,
    pub signature: [u8; 64],
}

impl Knot {
    pub fn id(&self) -> ObjectId {
        object_id(&self.encode())
    }

    /// Canonical payload: everything except we keep signature inline at the
    /// end. Signing covers all bytes before the signature field.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_id(&mut out, &self.tree_id);
        put_u32(&mut out, self.parents.len() as u32);
        for p in &self.parents {
            put_id(&mut out, p);
        }
        put_str(&mut out, &self.author);
        put_u64(&mut out, self.timestamp_secs);
        put_u32(&mut out, self.timestamp_nanos);
        put_str(&mut out, &self.message);
        put_str(&mut out, &self.git_id);
        out.extend_from_slice(&self.signature);
        out
    }

    /// Bytes covered by the Ed25519 signature (everything before signature).
    pub fn signed_bytes(&self) -> Vec<u8> {
        let full = self.encode();
        full[..full.len() - 64].to_vec()
    }

    pub fn decode(payload: &[u8]) -> Result<Knot> {
        if payload.len() < 64 {
            bail!("knot payload too short");
        }
        let (body, sig) = payload.split_at(payload.len() - 64);
        let mut r = Reader { buf: body, pos: 0 };
        let tree_id = r.id()?;
        let n = r.u32()?;
        let mut parents = Vec::with_capacity(n as usize);
        for _ in 0..n {
            parents.push(r.id()?);
        }
        let author = r.string()?;
        let timestamp_secs = r.u64()?;
        let timestamp_nanos = r.u32()?;
        let message = r.string()?;
        let git_id = r.string()?;
        r.done()?;
        Ok(Knot {
            tree_id,
            parents,
            author,
            timestamp_secs,
            timestamp_nanos,
            message,
            git_id,
            signature: sig.try_into().unwrap(),
        })
    }

    pub fn is_native(&self) -> bool {
        self.git_id.is_empty()
    }
}

// ---------- ref / head / index ----------

pub fn encode_ref(strand: &str, knot_id: &ObjectId) -> Vec<u8> {
    let mut out = Vec::new();
    put_str(&mut out, strand);
    put_id(&mut out, knot_id);
    out
}

pub fn decode_ref(payload: &[u8]) -> Result<(String, ObjectId)> {
    let mut r = Reader { buf: payload, pos: 0 };
    let strand = r.string()?;
    let knot_id = r.id()?;
    r.done()?;
    Ok((strand, knot_id))
}

pub fn encode_head(strand: &str) -> Vec<u8> {
    let mut out = Vec::new();
    put_str(&mut out, strand);
    out
}

pub fn decode_head(payload: &[u8]) -> Result<String> {
    let mut r = Reader { buf: payload, pos: 0 };
    let s = r.string()?;
    r.done()?;
    Ok(s)
}

// ---------- index (stat cache) ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub mtime_secs: u64,
    pub mtime_nanos: u32,
    pub size: u64,
    pub mode: u32,
    pub blob_id: ObjectId,
}

pub fn encode_index(cache: &BTreeMap<String, CacheEntry>) -> Vec<u8> {
    let mut out = Vec::new();
    put_u32(&mut out, cache.len() as u32);
    for (path, e) in cache {
        put_str(&mut out, path);
        put_u64(&mut out, e.mtime_secs);
        put_u32(&mut out, e.mtime_nanos);
        put_u64(&mut out, e.size);
        put_u32(&mut out, e.mode);
        put_id(&mut out, &e.blob_id);
    }
    out
}

pub fn decode_index(payload: &[u8]) -> Result<BTreeMap<String, CacheEntry>> {
    let mut r = Reader { buf: payload, pos: 0 };
    let n = r.u32()?;
    let mut map = BTreeMap::new();
    for _ in 0..n {
        let path = r.string()?;
        let e = CacheEntry {
            mtime_secs: r.u64()?,
            mtime_nanos: r.u32()?,
            size: r.u64()?,
            mode: r.u32()?,
            blob_id: r.id()?,
        };
        map.insert(path, e);
    }
    r.done()?;
    Ok(map)
}

// ---------- index deltas ("same parts") ----------
//
// An INDEX payload may be a delta against the cache state left by the
// previous INDEX record: only entries that changed are stored. The first
// u32 is INDEX_DELTA (0xFFFFFFFE), applied by the loader in frame order.

pub const INDEX_DELTA: u32 = 0xFFFF_FFFE;

/// (path, entry or None for removal), canonical order.
pub type IndexDelta = Vec<(String, Option<CacheEntry>)>;

pub fn is_index_delta(payload: &[u8]) -> bool {
    payload.len() >= 4 && u32::from_le_bytes(payload[..4].try_into().unwrap()) == INDEX_DELTA
}

/// Encode `new` as a delta against `prev`. Returns the payload only if it
/// is smaller than the full encoding.
pub fn encode_index_delta(prev: &BTreeMap<String, CacheEntry>, new: &BTreeMap<String, CacheEntry>) -> Option<Vec<u8>> {
    let mut changes: BTreeMap<&str, Option<&CacheEntry>> = BTreeMap::new();
    for (p, e) in new {
        if prev.get(p) != Some(e) {
            changes.insert(p, Some(e));
        }
    }
    for p in prev.keys() {
        if !new.contains_key(p) {
            changes.insert(p.as_str(), None);
        }
    }
    // An empty change set still encodes (marker + count 0): tiny, and keeps
    // the "one INDEX record per mutation" invariant.
    let mut out = Vec::new();
    put_u32(&mut out, INDEX_DELTA);
    put_u32(&mut out, changes.len() as u32);
    for (path, entry) in changes {
        put_str(&mut out, path);
        match entry {
            Some(e) => {
                out.push(1);
                put_u64(&mut out, e.mtime_secs);
                put_u32(&mut out, e.mtime_nanos);
                put_u64(&mut out, e.size);
                put_u32(&mut out, e.mode);
                put_id(&mut out, &e.blob_id);
            }
            None => out.push(0),
        }
    }
    if out.len() >= encode_index(new).len() {
        return None;
    }
    Some(out)
}

/// Apply a delta payload onto `cache` in place.
pub fn apply_index_delta(payload: &[u8], cache: &mut BTreeMap<String, CacheEntry>) -> Result<()> {
    let mut r = Reader { buf: payload, pos: 0 };
    let marker = r.u32()?;
    if marker != INDEX_DELTA {
        bail!("not an index delta");
    }
    let n = r.u32()?;
    for _ in 0..n {
        let path = r.string()?;
        match r.take(1)?[0] {
            0 => {
                cache.remove(&path);
            }
            _ => {
                let e = CacheEntry {
                    mtime_secs: r.u64()?,
                    mtime_nanos: r.u32()?,
                    size: r.u64()?,
                    mode: r.u32()?,
                    blob_id: r.id()?,
                };
                cache.insert(path, e);
            }
        }
    }
    r.done()?;
    Ok(())
}

// ---------- join (pending merge state) ----------

/// Latest-wins record for an in-progress join/frog/lift. state 0 = pending,
/// 1 = resolved by the next knot.
#[derive(Debug, Clone)]
pub struct JoinState {
    pub resolved: bool,
    /// Extra parents the next knot gets beyond HEAD (the other side(s)).
    pub extra_parents: Vec<ObjectId>,
    /// What the pending op was, for display ("join feature", "frog abc123").
    pub label: String,
}

pub fn encode_join(state: &JoinState) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(if state.resolved { 1 } else { 0 });
    put_u32(&mut out, state.extra_parents.len() as u32);
    for id in &state.extra_parents {
        put_id(&mut out, id);
    }
    put_str(&mut out, &state.label);
    out
}

pub fn decode_join(payload: &[u8]) -> Result<JoinState> {
    let mut r = Reader { buf: payload, pos: 0 };
    let resolved = match r.take(1)?[0] {
        0 => false,
        _ => true,
    };
    let n = r.u32()?;
    let mut extra_parents = Vec::with_capacity(n as usize);
    for _ in 0..n {
        extra_parents.push(r.id()?);
    }
    let label = r.string()?;
    r.done()?;
    Ok(JoinState { resolved, extra_parents, label })
}

/// Short knot ID for display: first 10 hex chars, like git's short hashes.
pub fn short_id(id: &ObjectId) -> String {
    hex::encode(&id[..5])
}

// ---------- mark (0x07) ----------

/// op: 0 = add, 1 = delete. knot_id is zeroed for deletes.
pub fn encode_mark(add: bool, name: &str, knot_id: &ObjectId) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(if add { 0 } else { 1 });
    put_str(&mut out, name);
    put_id(&mut out, knot_id);
    out
}

pub fn decode_mark(payload: &[u8]) -> Result<(bool, String, ObjectId)> {
    let mut r = Reader { buf: payload, pos: 0 };
    let add = match r.take(1)?[0] {
        0 => true,
        _ => false,
    };
    let name = r.string()?;
    let knot_id = r.id()?;
    r.done()?;
    Ok((add, name, knot_id))
}

// ---------- stash (0x08) ----------

#[derive(Debug, Clone)]
pub struct StashEntry {
    pub tree_id: ObjectId,
    pub label: String,
    pub timestamp_secs: u64,
}

impl StashEntry {
    /// Stable id for drop records: blake3 of canonical payload.
    pub fn stash_id(&self) -> ObjectId {
        object_id(&self.encode())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_id(&mut out, &self.tree_id);
        put_str(&mut out, &self.label);
        put_u64(&mut out, self.timestamp_secs);
        out
    }

    pub fn decode(payload: &[u8]) -> Result<StashEntry> {
        let mut r = Reader { buf: payload, pos: 0 };
        let e = StashEntry {
            tree_id: r.id()?,
            label: r.string()?,
            timestamp_secs: r.u64()?,
        };
        r.done()?;
        Ok(e)
    }
}

/// Marks a stash entry as dropped. Latest-wins per stash id.
pub fn encode_stash_drop(stash_id: &ObjectId) -> Vec<u8> {
    let mut out = Vec::new();
    put_id(&mut out, stash_id);
    out
}

pub fn decode_stash_drop(payload: &[u8]) -> Result<ObjectId> {
    let mut r = Reader { buf: payload, pos: 0 };
    let id = r.id()?;
    r.done()?;
    Ok(id)
}

// ---------- hub (0x0C) ----------

/// url empty = remove the remote.
pub fn encode_hub(name: &str, url: &str) -> Vec<u8> {
    let mut out = Vec::new();
    put_str(&mut out, name);
    put_str(&mut out, url);
    out
}

pub fn decode_hub(payload: &[u8]) -> Result<(String, String)> {
    let mut r = Reader { buf: payload, pos: 0 };
    let name = r.string()?;
    let url = r.string()?;
    r.done()?;
    Ok((name, url))
}
