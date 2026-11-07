//! Yarnball state: objects, strands, head, stat cache.

use crate::format::{self, KIND_HEAD, KIND_HUB, KIND_INDEX, KIND_JOIN, KIND_KNOT, KIND_MARK, KIND_REF, KIND_STASH, KIND_STASH_DROP, KIND_TREE};
use crate::object::{self, CacheEntry, JoinState, Knot, ObjectId, StashEntry, Tree};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
struct Loc {
    offset: u64,
    len: u64,
}

pub struct Yarnball {
    pub path: PathBuf,
    /// object id -> location of its payload frame
    objects: BTreeMap<ObjectId, Loc>,
    /// strand name -> knot id (latest REF wins)
    pub strands: BTreeMap<String, ObjectId>,
    /// active strand
    pub head: String,
    /// stat cache from latest INDEX record
    pub cache: BTreeMap<String, CacheEntry>,
    /// all knot object ids (for prefix resolution)
    pub knots: BTreeSet<ObjectId>,
    /// Ed25519 public keys seen in KEY records
    pub pubkeys: BTreeSet<[u8; 32]>,
    /// latest JOIN record; pending if !resolved
    pub join: Option<JoinState>,
    /// mark name -> knot id (latest op per name wins)
    pub marks: BTreeMap<String, ObjectId>,
    /// live stash entries (STASH records minus STASH_DROP)
    pub stashes: Vec<StashEntry>,
    /// hub name -> url (empty url removes)
    pub hubs: BTreeMap<String, String>,
}

impl Yarnball {
    /// Load state from an existing `.knit` file.
    pub fn load(path: &Path) -> Result<Self> {
        let frames = format::read_frames(path)?;
        let mut objects = BTreeMap::new();
        let mut strands = BTreeMap::new();
        let mut head = "main".to_string();
        let mut cache = BTreeMap::new();
        let mut knots = BTreeSet::new();
        let mut pubkeys = BTreeSet::new();
        let mut join = None;
        let mut marks = BTreeMap::new();
        let mut stashes: Vec<StashEntry> = Vec::new();
        let mut stash_drops: BTreeSet<ObjectId> = BTreeSet::new();
        let mut hubs = BTreeMap::new();

        for f in &frames {
            match f.kind {
                format::KIND_BLOB | KIND_TREE | KIND_KNOT => {
                    let id = object::object_id(&f.payload);
                    objects.insert(
                        id,
                        Loc { offset: f.payload_offset(), len: f.payload.len() as u64 },
                    );
                    if f.kind == KIND_KNOT {
                        knots.insert(id);
                    }
                }
                KIND_REF => {
                    let (name, knot) = object::decode_ref(&f.payload)?;
                    strands.insert(name, knot);
                }
                KIND_HEAD => {
                    head = object::decode_head(&f.payload)?;
                }
                KIND_INDEX => {
                    cache = object::decode_index(&f.payload)?;
                }
                format::KIND_KEY => {
                    if f.payload.len() == 32 {
                        pubkeys.insert(f.payload[..].try_into().unwrap());
                    }
                }
                KIND_JOIN => {
                    join = object::decode_join(&f.payload).ok();
                }
                KIND_MARK => {
                    if let Ok((add, name, id)) = object::decode_mark(&f.payload) {
                        if add {
                            marks.insert(name, id);
                        } else {
                            marks.remove(&name);
                        }
                    }
                }
                KIND_STASH => {
                    if let Ok(e) = StashEntry::decode(&f.payload) {
                        stashes.push(e);
                    }
                }
                KIND_STASH_DROP => {
                    if let Ok(id) = object::decode_stash_drop(&f.payload) {
                        stash_drops.insert(id);
                    }
                }
                KIND_HUB => {
                    if let Ok((name, url)) = object::decode_hub(&f.payload) {
                        if url.is_empty() {
                            hubs.remove(&name);
                        } else {
                            hubs.insert(name, url);
                        }
                    }
                }
                _ => {}
            }
        }

        stashes.retain(|s| !stash_drops.contains(&s.stash_id()));
        Ok(Self { path: path.to_path_buf(), objects, strands, head, cache, knots, pubkeys, join, marks, stashes, hubs })
    }

    /// Create a new yarnball: `.knit` file with HEAD=main.
    pub fn cast_on(dir: &Path) -> Result<Self> {
        let path = format::yarnball_path(dir);
        if path.exists() {
            bail!("A yarnball already exists at {}", path.display());
        }
        fs::create_dir_all(dir)?;
        format::create(&path)?;
        let mut file = format::open_write(&path)?;
        format::append_records(&mut file, &[(KIND_HEAD, object::encode_head("main"))])?;
        drop(file);
        Self::load(&path)
    }

    pub fn open(dir: &Path) -> Result<Self> {
        let path = format::yarnball_path(dir);
        if !path.exists() {
            bail!("No yarnball here. Run `knit cast-on` first.");
        }
        Self::load(&path)
    }

    pub fn has_object(&self, id: &ObjectId) -> bool {
        self.objects.contains_key(id)
    }

    /// Read an object's payload bytes.
    pub fn object_bytes(&self, id: &ObjectId) -> Result<Vec<u8>> {
        let loc = self.objects.get(id).context("object not found")?;
        let mut f = format::open_read(&self.path)?;
        f.seek(SeekFrom::Start(loc.offset))?;
        let mut buf = vec![0u8; loc.len as usize];
        f.read_exact(&mut buf)?;
        Ok(buf)
    }

    pub fn blob_bytes(&self, id: &ObjectId) -> Result<Vec<u8>> {
        self.object_bytes(id)
    }

    pub fn tree(&self, tree_id: &ObjectId) -> Result<Tree> {
        let payload = self.object_bytes(tree_id).context("tree object missing")?;
        object::decode_tree(&payload)
    }

    pub fn knot(&self, knot_id: &ObjectId) -> Result<Knot> {
        let payload = self.object_bytes(knot_id).context("knot object missing")?;
        Knot::decode(&payload)
    }

    pub fn head_knot(&self) -> Result<Option<ObjectId>> {
        Ok(self.strands.get(&self.head).copied())
    }

    /// Run a mutation under the exclusive lock, then reload state so object
    /// offsets and latest-wins records are fresh. `f` gets the locked file and
    /// should call `format::append_records`.
    pub fn mutate<R>(&mut self, f: impl FnOnce(&mut File) -> Result<R>) -> Result<R> {
        let mut file = format::open_write(&self.path)?;
        let r = f(&mut file)?;
        drop(file);
        *self = Self::load(&self.path)?;
        Ok(r)
    }
}
