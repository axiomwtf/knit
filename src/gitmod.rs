//! Minimal git object reader: loose objects and packfiles with delta
//! resolution, plus `knit pickup` import. Never shells out to git.

use crate::format::{self, KIND_HEAD, KIND_INDEX, KIND_KNOT, KIND_REF, KIND_TREE};
use crate::keys::KeyStorage;
use crate::object::{self, Knot, ObjectId, Tree, TreeEntry};
use crate::store::Yarnball;
use crate::worktree;
use anyhow::{bail, Context, Result};
use flate2::read::ZlibDecoder;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const OBJ_COMMIT: u8 = 1;
pub const OBJ_TREE: u8 = 2;
pub const OBJ_BLOB: u8 = 3;
pub const OBJ_TAG: u8 = 4;

pub type GitSha = [u8; 20];

fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    ZlibDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}

pub fn sha_hex(sha: &GitSha) -> String {
    hex::encode(sha)
}

// ---------- pack idx (v2) ----------

struct PackIdx {
    shas: Vec<GitSha>,
    offsets32: Vec<u32>,
    offsets64: Vec<u64>,
}

impl PackIdx {
    fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < 8 + 256 * 4 || &data[..4] != b"\xfftOc" {
            bail!("bad pack idx magic");
        }
        let version = u32::from_be_bytes(data[4..8].try_into().unwrap());
        if version != 2 {
            bail!("unsupported pack idx version {version}");
        }
        let fanout_start = 8;
        let fanout_end = fanout_start + 256 * 4;
        let n = u32::from_be_bytes(data[fanout_end - 4..fanout_end].try_into().unwrap()) as usize;
        let shas_end = fanout_end + n * 20;
        if data.len() < shas_end {
            bail!("truncated pack idx");
        }
        let mut shas = Vec::with_capacity(n);
        for i in 0..n {
            shas.push(data[fanout_end + i * 20..fanout_end + (i + 1) * 20].try_into().unwrap());
        }
        let mut pos = shas_end + n * 4; // skip crc table
        if data.len() < pos + n * 4 {
            bail!("truncated pack idx offsets");
        }
        let mut offsets32 = Vec::with_capacity(n);
        for i in 0..n {
            offsets32.push(u32::from_be_bytes(data[pos + i * 4..pos + i * 4 + 4].try_into().unwrap()));
        }
        pos += n * 4;
        let mut offsets64 = Vec::new();
        if offsets32.iter().any(|o| o & 0x8000_0000 != 0) {
            let avail = (data.len().saturating_sub(pos + 40)) / 8;
            for i in 0..avail {
                offsets64.push(u64::from_be_bytes(data[pos + i * 8..pos + i * 8 + 8].try_into().unwrap()));
            }
        }
        Ok(Self { shas, offsets32, offsets64 })
    }

    fn find(&self, sha: &GitSha) -> Option<u64> {
        let mut lo = 0usize;
        let mut hi = self.shas.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.shas[mid].cmp(sha) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let o = self.offsets32[mid];
                    return Some(if o & 0x8000_0000 != 0 {
                        self.offsets64[(o & 0x7fff_ffff) as usize]
                    } else {
                        o as u64
                    });
                }
            }
        }
        None
    }
}

// ---------- pack entries ----------

fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v: u64 = 0;
    let mut shift = 0;
    loop {
        let b = *buf.get(*pos).context("varint overrun")?;
        *pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        if shift > 63 {
            bail!("varint too long");
        }
    }
}

/// OFS_DELTA offset: first byte contributes its low 7 bits directly; each
/// following byte does n = ((n+1)<<7)|(b&0x7f). Relative to entry start.
fn read_ofs_offset(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let b = *buf.get(*pos).context("ofs offset overrun")?;
    *pos += 1;
    let mut n = (b & 0x7f) as u64;
    let mut c = b;
    while c & 0x80 != 0 {
        c = *buf.get(*pos).context("ofs offset overrun")?;
        *pos += 1;
        n = ((n + 1) << 7) | (c & 0x7f) as u64;
    }
    Ok(n)
}

fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    if std::env::var("KNIT_DEBUG").is_ok() {
        eprintln!("apply_delta: base={} delta={}", base.len(), delta.len());
    }
    let mut pos = 0usize;
    let _src_size = read_varint(delta, &mut pos)?;
    let dst_size = read_varint(delta, &mut pos)? as usize;
    let mut out = Vec::with_capacity(dst_size);
    while pos < delta.len() {
        let cmd = delta[pos];
        pos += 1;
        if cmd & 0x80 != 0 {
            let mut offset = 0usize;
            let mut len = 0usize;
            for i in 0..4 {
                if cmd & (1 << i) != 0 {
                    offset |= (*delta.get(pos).context("delta copy overrun")? as usize) << (8 * i);
                    pos += 1;
                }
            }
            for i in 0..3 {
                if cmd & (0x10 << i) != 0 {
                    len |= (*delta.get(pos).context("delta copy overrun")? as usize) << (8 * i);
                    pos += 1;
                }
            }
            if len == 0 {
                len = 0x10000;
            }
            let end = offset + len;
            if end > base.len() {
                bail!("delta copy out of range");
            }
            out.extend_from_slice(&base[offset..end]);
        } else {
            // Literal insert: the command byte's value is the byte count.
            let n = cmd as usize;
            if pos + n > delta.len() {
                bail!("delta insert out of range");
            }
            out.extend_from_slice(&delta[pos..pos + n]);
            pos += n;
        }
    }
    Ok(out)
}

// ---------- repo ----------

struct Pack {
    data: Vec<u8>,
    idx: PackIdx,
}

impl Pack {
    fn offset_of(&self, sha: &GitSha) -> Option<usize> {
        self.idx.find(sha).map(|o| o as usize)
    }

    /// Resolve the object starting at pack offset `pos`, following deltas.
    fn object_at(&self, pos: usize, depth: usize) -> Result<(u8, Vec<u8>)> {
        if depth > 64 {
            bail!("delta chain too deep");
        }
        let data = &self.data;
        let b0 = *data.get(pos).context("pack entry overrun")?;
        let typ = (b0 >> 4) & 0x7;
        if std::env::var("KNIT_DEBUG").is_ok() {
            eprintln!("pack entry at {pos}: b0={b0:#04x} typ={typ}");
        }
        if typ == 0 || typ == 5 {
            bail!("bad pack entry type {typ}");
        }
        let mut p = pos + 1;
        // size varint continuation
        let mut c = b0;
        while c & 0x80 != 0 {
            c = *data.get(p).context("pack header overrun")?;
            p += 1;
        }

        match typ {
            6 => {
                // OFS_DELTA: offset then zlib delta
                let ofs = read_ofs_offset(data, &mut p)? as usize;
                let base_pos = pos.checked_sub(ofs).context("ofs delta before start")?;
                let (base_type, base_data) = self.object_at(base_pos, depth + 1)?;
                let delta = inflate(&data[p..])?;
                Ok((base_type, apply_delta(&base_data, &delta)?))
            }
            7 => {
                // REF_DELTA: 20-byte base sha then zlib delta
                if p + 20 > data.len() {
                    bail!("ref delta header overrun");
                }
                let base_sha: GitSha = data[p..p + 20].try_into().unwrap();
                p += 20;
                let base_off = self
                    .idx
                    .find(&base_sha)
                    .context("ref delta base not in pack")? as usize;
                let (base_type, base_data) = self.object_at(base_off, depth + 1)?;
                let delta = inflate(&data[p..])?;
                Ok((base_type, apply_delta(&base_data, &delta)?))
            }
            _ => {
                let raw = inflate(&data[p..])?;
                Ok((typ, raw))
            }
        }
    }
}

pub struct GitRepo {
    pub git_dir: PathBuf,
    packs: Vec<Pack>,
    cache: std::cell::RefCell<HashMap<GitSha, (u8, Vec<u8>)>>,
}

impl GitRepo {
    pub fn open(path: &Path) -> Result<Self> {
        let git_dir = if path.join(".git").is_dir() {
            path.join(".git")
        } else if path.join(".git").is_file() {
            // worktree gitfile: "gitdir: <path>"
            let content = fs::read_to_string(path.join(".git"))?;
            let target = content.trim().strip_prefix("gitdir:").context("bad gitfile")?.trim();
            let p = PathBuf::from(target);
            if p.is_absolute() { p } else { path.join(p) }
        } else if path.join("HEAD").exists() && path.join("objects").is_dir() {
            path.to_path_buf() // bare repo or .git dir itself
        } else {
            bail!("{} does not look like a git repository", path.display());
        };

        let mut packs = Vec::new();
        let pack_dir = git_dir.join("objects").join("pack");
        if pack_dir.is_dir() {
            for e in fs::read_dir(&pack_dir)? {
                let p = e?.path();
                if p.extension().map(|x| x == "idx").unwrap_or(false) {
                    let data = fs::read(&p)?;
                    let idx = PackIdx::parse(&data).with_context(|| format!("bad idx {}", p.display()))?;
                    let pack_path = p.with_extension("pack");
                    let pdata = fs::read(&pack_path)
                        .with_context(|| format!("missing pack for {}", p.display()))?;
                    packs.push(Pack { data: pdata, idx });
                }
            }
        }
        Ok(Self { git_dir, packs, cache: std::cell::RefCell::new(HashMap::new()) })
    }

    /// Read a git object by sha. Returns (type, raw content).
    pub fn read_object(&self, sha: &GitSha) -> Result<(u8, Vec<u8>)> {
        if let Some(v) = self.cache.borrow().get(sha) {
            return Ok(v.clone());
        }
        // loose object
        let hex = sha_hex(sha);
        let loose = self.git_dir.join("objects").join(&hex[..2]).join(&hex[2..]);
        let result = if loose.exists() {
            let raw = inflate(&fs::read(&loose)?)?;
            let nul = raw.iter().position(|b| *b == 0).context("bad loose object header")?;
            let header = std::str::from_utf8(&raw[..nul])?;
            let mut parts = header.split(' ');
            let typ = match parts.next() {
                Some("commit") => OBJ_COMMIT,
                Some("tree") => OBJ_TREE,
                Some("blob") => OBJ_BLOB,
                Some("tag") => OBJ_TAG,
                other => bail!("unknown loose object type {other:?}"),
            };
            (typ, raw[nul + 1..].to_vec())
        } else {
            let mut found = None;
            for pack in &self.packs {
                if let Some(off) = pack.offset_of(sha) {
                    found = Some(pack.object_at(off, 0)?);
                    break;
                }
            }
            found.context("object not found")?
        };
        self.cache.borrow_mut().insert(*sha, result.clone());
        Ok(result)
    }

    pub fn read_object_hex(&self, hex: &str) -> Result<(u8, Vec<u8>)> {
        let sha: GitSha = hex::decode(hex.trim())?
            .try_into()
            .map_err(|v: Vec<u8>| anyhow::anyhow!("bad sha length {}", v.len()))?;
        self.read_object(&sha)
    }

    /// refs/heads/* -> (branch name, sha hex). Includes packed-refs.
    pub fn branches(&self) -> Result<Vec<(String, String)>> {
        let mut out: HashMap<String, String> = HashMap::new();
        let packed = self.git_dir.join("packed-refs");
        if packed.exists() {
            for line in fs::read_to_string(&packed)?.lines() {
                if line.starts_with('#') || line.trim().is_empty() {
                    continue;
                }
                let mut it = line.split(' ');
                if let (Some(sha), Some(r)) = (it.next(), it.next()) {
                    if let Some(name) = r.strip_prefix("refs/heads/") {
                        out.insert(name.to_string(), sha.to_string());
                    }
                }
            }
        }
        let heads = self.git_dir.join("refs").join("heads");
        if heads.is_dir() {
            for e in fs::read_dir(&heads)? {
                let p = e?.path();
                if p.is_file() {
                    if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                        let sha = fs::read_to_string(&p)?.trim().to_string();
                        out.insert(name.to_string(), sha);
                    }
                }
            }
        }
        Ok(out.into_iter().collect())
    }

    /// refs/tags/* -> (tag name, sha hex). Includes packed-refs.
    pub fn tags(&self) -> Result<Vec<(String, String)>> {
        let mut out: HashMap<String, String> = HashMap::new();
        let packed = self.git_dir.join("packed-refs");
        if packed.exists() {
            for line in fs::read_to_string(&packed)?.lines() {
                if line.starts_with('#') || line.trim().is_empty() {
                    continue;
                }
                let mut it = line.split(' ');
                if let (Some(sha), Some(r)) = (it.next(), it.next()) {
                    if let Some(name) = r.strip_prefix("refs/tags/") {
                        out.insert(name.to_string(), sha.to_string());
                    }
                }
            }
        }
        let tags = self.git_dir.join("refs").join("tags");
        if tags.is_dir() {
            for e in fs::read_dir(&tags)? {
                let p = e?.path();
                if p.is_file() {
                    if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                        let sha = fs::read_to_string(&p)?.trim().to_string();
                        out.insert(name.to_string(), sha);
                    }
                }
            }
        }
        Ok(out.into_iter().collect())
    }

    /// Current branch from HEAD symref, if on a branch.
    pub fn head_branch(&self) -> Option<String> {
        let head = fs::read_to_string(self.git_dir.join("HEAD")).ok()?;
        head.trim()
            .strip_prefix("ref: refs/heads/")
            .map(|s| s.to_string())
    }
}

// ---------- commit / tree parsing ----------

pub struct GitCommit {
    pub tree_sha: String,
    pub parents: Vec<String>,
    pub author: String,
    pub author_ts: Option<u64>,
    pub message: String,
}

pub fn parse_commit(data: &[u8]) -> Result<GitCommit> {
    let text = String::from_utf8_lossy(data);
    let (header, message) = text.split_once("\n\n").unwrap_or((&text, ""));
    let mut tree_sha = String::new();
    let mut parents = Vec::new();
    let mut author = String::new();
    let mut author_ts = None;
    for line in header.lines() {
        if let Some(rest) = line.strip_prefix("tree ") {
            tree_sha = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("parent ") {
            parents.push(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("author ") {
            author_ts = git_line_ts(rest);
            author = parse_author(rest);
        }
    }
    if tree_sha.is_empty() {
        bail!("commit has no tree");
    }
    Ok(GitCommit { tree_sha, parents, author, author_ts, message: message.to_string() })
}

/// Unix timestamp from the trailing fields of an ident line.
fn git_line_ts(line: &str) -> Option<u64> {
    let mut it = line.trim().rsplit(' ');
    let _tz = it.next()?;
    it.next()?.parse().ok()
}

/// "A U Thor <a@b.c> 1700000000 +0000" -> "A U Thor <a@b.c>"
fn parse_author(line: &str) -> String {
    let line = line.trim();
    // strip trailing " <ts> <tz>"
    let mut idx = line.len();
    for _ in 0..2 {
        match line[..idx].rfind(' ') {
            Some(i) => idx = i,
            None => return line.to_string(),
        }
    }
    line[..idx].trim().to_string()
}

/// One entry in a git tree object.
struct GitTreeEntry {
    mode: u32,
    name: String,
    sha: GitSha,
}

fn parse_tree_entries(data: &[u8]) -> Result<Vec<GitTreeEntry>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let sp = data[pos..].iter().position(|b| *b == b' ').map(|i| pos + i).context("bad tree entry")?;
        let mode_str = std::str::from_utf8(&data[pos..sp])?;
        let mode = u32::from_str_radix(mode_str.trim_start_matches('0'), 8)
            .or_else(|_| u32::from_str_radix(mode_str, 8))?;
        let name_start = sp + 1;
        let nul = data[name_start..].iter().position(|b| *b == 0).map(|i| name_start + i).context("bad tree entry")?;
        let name = String::from_utf8_lossy(&data[name_start..nul]).into_owned();
        let sha_start = nul + 1;
        if sha_start + 20 > data.len() {
            bail!("truncated tree entry");
        }
        let sha: GitSha = data[sha_start..sha_start + 20].try_into().unwrap();
        out.push(GitTreeEntry { mode, name, sha });
        pos = sha_start + 20;
    }
    Ok(out)
}

// ---------- import ----------

pub struct Importer<'a> {
    repo: &'a GitRepo,
    y: &'a mut Yarnball,
    kp: crate::keys::KeyPair,
    /// git tree sha -> knit tree id
    tree_cache: HashMap<GitSha, ObjectId>,
    /// git commit sha hex -> knit knot id
    commit_cache: HashMap<String, ObjectId>,
    /// git blob sha hex -> knit blob id
    blob_cache: HashMap<String, ObjectId>,
    /// object ids already queued in `records` (not yet in the yarnball)
    pending: HashSet<ObjectId>,
    pub gitlinks_skipped: usize,
}

pub struct ImportStats {
    pub commits: usize,
    pub trees: usize,
    pub blobs: usize,
    pub gitlinks_skipped: usize,
}

impl<'a> Importer<'a> {
    pub fn new(repo: &'a GitRepo, y: &'a mut Yarnball) -> Result<Self> {
        let ks = KeyStorage::new()?;
        let kp = ks.load_keypair()?;
        Ok(Self {
            repo, y, kp,
            tree_cache: HashMap::new(),
            commit_cache: HashMap::new(),
            blob_cache: HashMap::new(),
            pending: HashSet::new(),
            gitlinks_skipped: 0,
        })
    }

    /// Import all refs/heads/* as strands, set HEAD, return stats.
    pub fn import(&mut self) -> Result<ImportStats> {
        let branches = self.repo.branches()?;
        if branches.is_empty() {
            bail!("no branches found in git repo");
        }
        let head_branch = self.repo.head_branch();

        let mut records: Vec<(u8, Vec<u8>)> = Vec::new();
        let pubkey = self.kp.public_key_bytes();
        if !self.y.pubkeys.contains(&pubkey) {
            records.push((format::KIND_KEY, pubkey.to_vec()));
        }

        let mut branch_tips: Vec<(String, ObjectId)> = Vec::new();
        for (name, sha_hex) in &branches {
            let knot_id = self.import_commit_hex(sha_hex, &mut records)?;
            branch_tips.push((name.clone(), knot_id));
        }
        for (name, knot_id) in &branch_tips {
            records.push((KIND_REF, object::encode_ref(name, knot_id)));
        }
        let head_name = head_branch
            .filter(|h| branch_tips.iter().any(|(n, _)| n == h))
            .unwrap_or_else(|| branch_tips[0].0.clone());
        records.push((KIND_HEAD, object::encode_head(&head_name)));

        // Annotated (and lightweight) tags become marks.
        for (name, sha_hex) in self.repo.tags()? {
            match self.import_tag_hex(&sha_hex, &mut records) {
                Ok(knot_id) => {
                    records.push((format::KIND_MARK, object::encode_mark(true, &name, &knot_id)));
                }
                Err(e) => eprintln!("warning: skipping tag '{name}': {e}"),
            }
        }

        self.y.mutate(|f| format::append_records(f, &records))?;

        let stats = ImportStats {
            commits: self.commit_cache.len(),
            trees: self.tree_cache.len(),
            blobs: self.blob_cache.len(),
            gitlinks_skipped: self.gitlinks_skipped,
        };
        Ok(stats)
    }

    fn import_commit_hex(&mut self, sha_hex: &str, records: &mut Vec<(u8, Vec<u8>)>) -> Result<ObjectId> {
        if let Some(id) = self.commit_cache.get(sha_hex) {
            return Ok(*id);
        }
        let (typ, data) = self.repo.read_object_hex(sha_hex)?;
        if typ != OBJ_COMMIT {
            bail!("{sha_hex} is not a commit");
        }
        let commit = parse_commit(&data)?;
        let tree_id = self.import_tree_hex(&commit.tree_sha, records)?;
        let mut parents = Vec::new();
        for p in &commit.parents {
            parents.push(self.import_commit_hex(p, records)?);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let mut knot = Knot {
            tree_id,
            parents,
            author: if commit.author.is_empty() { "git".into() } else { commit.author.clone() },
            timestamp_secs: commit.author_ts.unwrap_or((now / 1_000_000_000) as u64),
            timestamp_nanos: 0,
            message: commit.message,
            git_id: sha_hex.trim().to_string(),
            signature: [0u8; 64],
        };
        knot.signature = self.kp.sign(&knot.signed_bytes());
        let id = knot.id();
        records.push((KIND_KNOT, knot.encode()));
        self.commit_cache.insert(sha_hex.trim().to_string(), id);
        Ok(id)
    }

    /// Resolve a tag ref (possibly an annotated tag object) to a knot id.
    fn import_tag_hex(&mut self, sha_hex: &str, records: &mut Vec<(u8, Vec<u8>)>) -> Result<ObjectId> {
        let (typ, data) = self.repo.read_object_hex(sha_hex)?;
        match typ {
            OBJ_COMMIT => self.import_commit_hex(sha_hex, records),
            OBJ_TAG => {
                let text = String::from_utf8_lossy(&data);
                let target = text.lines()
                    .find_map(|l| l.strip_prefix("object "))
                    .map(|s| s.trim().to_string())
                    .context("tag object has no target")?;
                self.import_tag_hex(&target, records)
            }
            other => bail!("tag points at non-commit object type {other}"),
        }
    }

    fn import_tree_hex(&mut self, sha_hex: &str, records: &mut Vec<(u8, Vec<u8>)>) -> Result<ObjectId> {
        let sha: GitSha = hex::decode(sha_hex.trim())?
            .try_into()
            .map_err(|v: Vec<u8>| anyhow::anyhow!("bad tree sha len {}", v.len()))?;
        if let Some(id) = self.tree_cache.get(&sha) {
            return Ok(*id);
        }
        let mut flat = Tree::new();
        self.collect_tree(&sha, "", &mut flat, records)?;
        let payload = object::encode_tree(&flat);
        let id = object::object_id(&payload);
        if !self.y.has_object(&id) && !self.pending.contains(&id) {
            self.pending.insert(id);
            records.push((KIND_TREE, payload));
        }
        self.tree_cache.insert(sha, id);
        Ok(id)
    }

    /// Walk a git tree object into the flat path map. Subdirectories recurse
    /// in place; no intermediate knit tree objects are created.
    fn collect_tree(
        &mut self,
        sha: &GitSha,
        prefix: &str,
        flat: &mut Tree,
        records: &mut Vec<(u8, Vec<u8>)>,
    ) -> Result<()> {
        let (typ, data) = self.repo.read_object(sha)?;
        if typ != OBJ_TREE {
            bail!("{} is not a tree", sha_hex(sha));
        }
        for e in parse_tree_entries(&data)? {
            self.walk_tree_entry(&e, prefix, flat, records)?;
        }
        Ok(())
    }

    fn walk_tree_entry(
        &mut self,
        e: &GitTreeEntry,
        prefix: &str,
        flat: &mut Tree,
        records: &mut Vec<(u8, Vec<u8>)>,
    ) -> Result<()> {
        let path = format!("{prefix}{}", e.name);
        if e.mode == 0o040000 {
            return self.collect_tree(&e.sha, &format!("{path}/"), flat, records);
        }
        if e.mode == 0o160000 {
            self.gitlinks_skipped += 1;
            return Ok(());
        }
        let mode = match e.mode {
            0o100755 => 0o100755,
            0o120000 => 0o120000,
            _ => 0o100644,
        };
        let blob_id = self.import_blob(&e.sha, records)?;
        flat.insert(path.clone(), TreeEntry { path, mode, blob_id });
        Ok(())
    }

    fn import_blob(&mut self, sha: &GitSha, records: &mut Vec<(u8, Vec<u8>)>) -> Result<ObjectId> {
        let hex = sha_hex(sha);
        if let Some(id) = self.blob_cache.get(&hex) {
            return Ok(*id);
        }
        let (typ, data) = self.repo.read_object(sha)?;
        if typ != OBJ_BLOB {
            bail!("{} is not a blob", hex);
        }
        let id = object::object_id(&data);
        if !self.y.has_object(&id) && !self.pending.contains(&id) {
            self.pending.insert(id);
            records.push((format::KIND_BLOB, data));
        }
        self.blob_cache.insert(hex, id);
        Ok(id)
    }
}

/// Import a git repo (worktree or bare/gitdir path) into the yarnball at `root`.
pub fn pickup(root: &Path, git_path: &Path) -> Result<ImportStats> {
    let repo = GitRepo::open(git_path)?;
    let mut y = Yarnball::cast_on(root)?;
    let mut importer = Importer::new(&repo, &mut y)?;
    let stats = importer.import()?;

    // Materialize the worktree and write the stat cache.
    let head_tree = match y.head_knot()? {
        Some(k) => {
            let knot = y.knot(&k)?;
            Some(y.tree(&knot.tree_id)?)
        }
        None => None,
    };
    if let Some(t) = &head_tree {
        worktree::materialize(root, &y, t, None)?;
    }
    let (_, cache, blobs) = worktree::scan(root, &std::collections::BTreeMap::new())?;
    let mut records = Vec::new();
    for (id, content) in blobs {
        if !y.has_object(&id) {
            records.push((format::KIND_BLOB, content));
        }
    }
    records.push((KIND_INDEX, object::encode_index(&cache)));
    if !records.is_empty() {
        y.mutate(|f| format::append_records(f, &records))?;
    }
    Ok(stats)
}
