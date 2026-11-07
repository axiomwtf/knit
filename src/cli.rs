//! `knit` command line. Vocabulary per knitspec.md section 9.

use crate::diff::{self, Status};
use crate::format::{self, KIND_HEAD, KIND_INDEX, KIND_KNOT, KIND_REF, KIND_TREE};
use crate::keys::KeyStorage;
use crate::merge;
use crate::object::{self, Knot, ObjectId, Tree};
use crate::store::Yarnball;
use crate::worktree;
use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "knit", version, about = "Knit version control")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Manage the Ed25519 signing identity
    Key(KeyArgs),
    /// Create a new yarnball
    #[command(name = "cast-on")]
    CastOn { dir: Option<PathBuf> },
    /// Snapshot the working tree as a new knot
    #[command(name = "tie", alias = "knot", alias = "save")]
    Tie(TieArgs),
    /// Show working-tree changes since the last knot
    Count,
    /// List knots on the current strand
    Rows { strand: Option<String> },
    /// Show changes between knots or the working tree
    Shear { args: Vec<String> },
    /// Inspect one knot's message and diff
    Pick { knot: String },
    /// Join a strand back; conflicts are snags
    #[command(name = "join")]
    Join { strand: String },
    /// Add a knot reversing an earlier one
    #[command(name = "frog")]
    Frog { knot: String },
    /// Lift one knot onto the current strand
    #[command(name = "lift")]
    Lift { knot: String },
    /// Mark a knot with a name
    #[command(name = "mark")]
    Mark(MarkArgs),
    /// Set work aside without a knot
    #[command(name = "stash")]
    Stash {
        #[command(subcommand)]
        cmd: Option<StashCmd>,
        /// Stash label
        message: Option<String>,
    },
    /// Remove untracked files
    Sweep,
    /// Trace which knots touched a file
    #[command(name = "track")]
    Track { file: String },
    /// Materialize another strand into a separate directory
    #[command(name = "loom")]
    Loom { strand: String, dir: PathBuf },
    /// Compact storage; never rewrites history
    #[command(name = "block")]
    Block,
    /// Manage Knits remotes
    Hub(HubArgs),
    /// Push to Knits
    #[command(name = "send")]
    Send { hub: String },
    /// Pull new objects down
    #[command(name = "gather")]
    Gather { hub: String },
    /// Gather plus join
    #[command(name = "haul")]
    Haul { hub: String },
    /// List, create, and manage strands
    Strand { name: Option<String> },
    /// Clone via Knits or git import
    #[command(name = "pickup")]
    Pickup { path: PathBuf },
    /// Change the active strand
    Switch { strand: String },
}

#[derive(Args)]
pub struct KeyArgs {
    #[command(subcommand)]
    pub cmd: KeyCmd,
}

#[derive(Subcommand)]
pub enum KeyCmd {
    /// Generate a new Ed25519 keypair
    Gen,
    /// Show the public key
    Show,
}

#[derive(Args)]
pub struct MarkArgs {
    /// Mark name; omitted lists marks
    name: Option<String>,
    /// Knot to mark (default HEAD)
    knot: Option<String>,
    /// Delete a mark
    #[arg(short = 'd', long = "delete")]
    delete: bool,
}

#[derive(clap::Subcommand)]
pub enum StashCmd {
    /// List stashes
    List,
    /// Apply the latest stash to the worktree
    Pop,
}

#[derive(Args)]
pub struct HubArgs {
    #[command(subcommand)]
    pub cmd: HubCmd,
}

#[derive(clap::Subcommand)]
pub enum HubCmd {
    /// Add a remote (url: file:///dir, /dir, or user@host[:dir])
    Add { name: String, url: String },
    /// List remotes
    List,
    /// Remove a remote
    Remove { name: String },
}

#[derive(Args)]
pub struct TieArgs {
    /// Short title, single line, required
    #[arg(short = 'k', long = "knot")]
    knot: Option<String>,
    /// Longer description, optional, repeatable
    #[arg(short = 'y', long = "yarn")]
    yarn: Vec<String>,
    /// Open $EDITOR with both fields
    #[arg(short = 'e', long = "edit", conflicts_with_all = ["knot", "yarn"])]
    edit: bool,
}

pub fn run(cli: Cli) -> Result<()> {
    // Loom worktrees are read-only for worktree-mutating commands.
    let loom_blocked = matches!(
        cli.cmd,
        Cmd::Tie(_) | Cmd::Join { .. } | Cmd::Frog { .. } | Cmd::Lift { .. }
            | Cmd::Switch { .. } | Cmd::Sweep | Cmd::Stash { .. }
    );
    if loom_blocked {
        if let Ok(r) = repo() {
            if r.in_loom {
                bail!("loom worktrees are read-only; run this in the main checkout");
            }
        }
    }
    match cli.cmd {
        Cmd::Key(k) => key(k.cmd),
        Cmd::CastOn { dir } => {
            let dir = dir.unwrap_or_else(|| std::env::current_dir().unwrap());
            Yarnball::cast_on(&dir)?;
            println!("cast on: {}", format::yarnball_path(&dir).display());
            Ok(())
        }
        Cmd::Tie(args) => tie(args),
        Cmd::Count => count(),
        Cmd::Rows { strand } => rows(strand.as_deref()),
        Cmd::Shear { args } => shear(&args),
        Cmd::Pick { knot } => pick(&knot),
        Cmd::Join { strand } => join(&strand),
        Cmd::Frog { knot } => frog(&knot),
        Cmd::Lift { knot } => lift(&knot),
        Cmd::Mark(a) => mark(a),
        Cmd::Stash { cmd, message } => stash(cmd.as_ref(), message.as_deref()),
        Cmd::Sweep => sweep(),
        Cmd::Track { file } => track(&file),
        Cmd::Loom { strand, dir } => loom(&strand, &dir),
        Cmd::Block => block(),
        Cmd::Hub(h) => hub(h.cmd),
        Cmd::Send { hub } => send(&hub),
        Cmd::Gather { hub } => gather(&hub, false),
        Cmd::Haul { hub } => gather(&hub, true),
        Cmd::Strand { name } => strand(name.as_deref()),
        Cmd::Switch { strand } => switch(&strand),
        Cmd::Pickup { path } => pickup(&path),
    }
}

// ---------- helpers ----------

/// A resolved repo location: `wt` is the working tree (may be a loom
/// worktree), `ball` is the directory holding the `.knit` yarnball.
pub struct Repo {
    pub wt: PathBuf,
    pub ball: PathBuf,
    pub in_loom: bool,
}

/// Resolve from cwd upward: a `.loom` file marks a linked worktree; a `.knit`
/// file marks a normal yarnball root.
fn repo() -> Result<Repo> {
    let cwd = std::env::current_dir()?;
    let mut dir = Some(cwd.as_path());
    while let Some(d) = dir {
        let loom = d.join(".loom");
        if loom.exists() {
            let text = fs::read_to_string(&loom)?;
            let root = text
                .lines()
                .find_map(|l| l.strip_prefix("root ").map(|s| s.trim().to_string()))
                .context("bad .loom file")?;
            return Ok(Repo { wt: d.to_path_buf(), ball: PathBuf::from(root), in_loom: true });
        }
        if format::yarnball_path(d).exists() {
            return Ok(Repo { wt: d.to_path_buf(), ball: d.to_path_buf(), in_loom: false });
        }
        dir = d.parent();
    }
    bail!("No yarnball found (looked for .knit or .loom upward from {})", cwd.display())
}

fn author() -> String {
    let cfg = dirs::config_dir().map(|d| d.join("knit").join("config"));
    let (mut name, mut email) = (String::new(), String::new());
    if let Some(p) = cfg {
        if let Ok(text) = fs::read_to_string(p) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    match k.trim() {
                        "name" => name = v.trim().to_string(),
                        "email" => email = v.trim().to_string(),
                        _ => {}
                    }
                }
            }
        }
    }
    if name.is_empty() {
        name = whoami::username();
    }
    if email.is_empty() {
        email = whoami::fallible::hostname().unwrap_or_else(|_| "localhost".into());
    }
    format!("{name} <{email}>")
}

/// $EDITOR flow for `knit tie` with no message flags.
fn edit_message() -> Result<(String, String)> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let dir = std::env::temp_dir();
    let file = dir.join(format!("knit-msg-{}", std::process::id()));
    fs::write(&file, "# knot title on the first line\n# description below\n")?;
    let status = std::process::Command::new(&editor).arg(&file).status()?;
    if !status.success() {
        bail!("editor failed");
    }
    let text = fs::read_to_string(&file)?;
    let _ = fs::remove_file(&file);
    let mut lines = text.lines();
    let title = lines.next().unwrap_or("").trim().to_string();
    let body: Vec<&str> = lines
        .map(|l| l.trim_end())
        .skip_while(|l| l.is_empty())
        .collect();
    Ok((title, body.join("\n")))
}

/// Argument substitution for free-text args: \n, $(line), $(newline[x]).
fn substitute(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i..];
        if let Some(inner) = after.strip_prefix("$(newline[") {
            if let Some(end) = inner.find("])") {
                out.push('\n');
                out.push_str(&inner[..end]);
                rest = &inner[end + 2..];
                continue;
            }
        } else if after.starts_with("$(line)") {
            out.push('\n');
            rest = &after[7..];
            continue;
        }
        out.push('$');
        rest = &after[1..];
    }
    out.push_str(rest);
    out.replace("\\n", "\n")
}

fn resolve_knot(y: &Yarnball, arg: &str) -> Result<ObjectId> {
    // HEAD and strand names resolve to their tip knot.
    if arg == "HEAD" {
        return y.head_knot()?.context("no knots yet");
    }
    if let Some(id) = y.strands.get(arg) {
        return Ok(*id);
    }
    // Accept full hex or an unambiguous prefix.
    if let Ok(id) = hex::decode(arg) {
        if id.len() == 32 {
            let id: ObjectId = id.try_into().unwrap();
            if y.has_object(&id) {
                return Ok(id);
            }
        }
    }
    let want = arg.to_lowercase();
    let mut matches = Vec::new();
    for &id in &y.knots {
        if hex::encode(id).starts_with(&want) {
            matches.push(id);
        }
    }
    match matches.len() {
        0 => bail!("no knot matching '{arg}'"),
        1 => Ok(matches[0]),
        _ => bail!("ambiguous knot '{arg}'"),
    }
}

fn head_tree(y: &Yarnball) -> Result<Option<Tree>> {
    match y.head_knot()? {
        Some(k) => {
            let knot = y.knot(&k)?;
            Ok(Some(y.tree(&knot.tree_id)?))
        }
        None => Ok(None),
    }
}

// ---------- commands ----------

fn key(cmd: KeyCmd) -> Result<()> {
    let ks = KeyStorage::new()?;
    match cmd {
        KeyCmd::Gen => {
            if ks.has_keypair() {
                bail!("keypair already exists at {}; remove it first to regenerate", ks.privkey_path().display());
            }
            let kp = ks.generate_keypair()?;
            println!("generated Ed25519 keypair");
            println!("  private: {}", ks.privkey_path().display());
            println!("  public:  {}", ks.pubkey_path().display());
            println!("  key id:  {}", hex::encode(blake3::hash(&kp.public_key_bytes()).as_bytes()));
        }
        KeyCmd::Show => {
            if !ks.has_keypair() {
                bail!("no keypair; run `knit key gen`");
            }
            let kp = ks.load_keypair()?;
            println!("public key: {}", hex::encode(kp.public_key_bytes()));
        }
    }
    Ok(())
}

fn tie(args: TieArgs) -> Result<()> {
    let repo = repo()?;
    let root = repo.wt;
    let ball = repo.ball;
    let (title, body) = if args.edit {
        edit_message()?
    } else {
        let title = args
            .knot
            .clone()
            .context("knit tie needs -knot \"title\" (or no flags to edit)")?;
        if title.contains('\n') {
            bail!("-knot must be a single line");
        }
        let body = args.yarn.iter().map(|s| substitute(s)).collect::<Vec<_>>().join("\n\n");
        (title, body)
    };
    if title.trim().is_empty() {
        bail!("knot title is empty");
    }
    let message = if body.is_empty() { title } else { format!("{title}\n\n{body}") };

    let mut y = Yarnball::open(&ball)?;
    let (tree, new_cache, blobs) = worktree::scan(&root, &y.cache)?;

    let head_knot = y.head_knot()?;
    let mut parents: Vec<ObjectId> = head_knot.into_iter().collect();
    // A pending join/frog/lift contributes its extra parents to this knot.
    let mut join_done = None;
    if let Some(j) = &y.join {
        if !j.resolved {
            parents.extend(j.extra_parents.iter().copied());
            join_done = Some(object::JoinState {
                resolved: true,
                extra_parents: j.extra_parents.clone(),
                label: j.label.clone(),
            });
        }
    }

    let base = base_tree(&y, &parents)?;
    let (tree_id, tree_record) = plan_tree(&y, &tree, base)?;

    let ks = KeyStorage::new()?;
    let kp = ks.load_keypair()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let mut knot = Knot {
        tree_id,
        parents,
        author: author(),
        timestamp_secs: (now / 1_000_000_000) as u64,
        timestamp_nanos: (now % 1_000_000_000) as u32,
        message,
        git_id: String::new(),
        signature: [0u8; 64],
    };
    let sig = kp.sign(&knot.signed_bytes());
    knot.signature = sig;
    let knot_id = knot.id();
    let head = y.head.clone();

    let mut records: Vec<(u8, Vec<u8>)> = Vec::new();
    for (id, content) in &blobs {
        if !y.has_object(id) {
            records.push((format::KIND_BLOB, content.clone()));
        }
    }
    if let Some(payload) = tree_record {
        records.push((KIND_TREE, payload));
    }
    let pubkey = kp.public_key_bytes();
    if !y.pubkeys.contains(&pubkey) {
        records.push((format::KIND_KEY, pubkey.to_vec()));
    }
    records.push((KIND_KNOT, knot.encode()));
    if let Some(j) = &join_done {
        records.push((format::KIND_JOIN, object::encode_join(j)));
    }
    records.push((KIND_REF, object::encode_ref(&head, &knot_id)));
    records.push((KIND_INDEX, index_payload(&y.cache, &new_cache)));

    y.mutate(|f| format::append_records(f, &records))?;
    println!("knot {} {}", object::short_id(&knot_id), diff::hide_footer(&knot.message).lines().next().unwrap_or(""));
    Ok(())
}

fn count() -> Result<()> {
    let repo = repo()?;
    let root = repo.wt;
    let ball = repo.ball;
    let y = Yarnball::open(&ball)?;
    let (tree, _, _) = worktree::scan(&root, &y.cache)?;
    let old = head_tree(&y)?;
    let changes = diff::tree_diff(old.as_ref(), &tree);
    if let Some(j) = &y.join {
        if !j.resolved {
            println!("joining: {} (resolve, then `knit tie`)", j.label);
        }
    }
    if changes.is_empty() {
        println!("clean");
        return Ok(());
    }
    let mut added = 0;
    let mut modified = 0;
    let mut deleted = 0;
    for c in &changes {
        let tag = match c.status {
            Status::Added => { added += 1; "A" }
            Status::Modified => { modified += 1; "M" }
            Status::Deleted => { deleted += 1; "D" }
        };
        println!("{tag} {}", c.path);
    }
    println!("-- {} added, {} modified, {} deleted", added, modified, deleted);
    Ok(())
}

fn rows(strand: Option<&str>) -> Result<()> {
    let repo = repo()?;
    let ball = repo.ball;
    let y = Yarnball::open(&ball)?;
    let name = strand.unwrap_or(&y.head);
    let head_id = y.head_knot()?;
    let mut cur = match y.strands.get(name) {
        Some(k) => *k,
        None => bail!("no such strand '{name}'"),
    };
    loop {
        let knot = y.knot(&cur)?;
        let marker = if Some(cur) == head_id && name == y.head { '*' } else { ' ' };
        let subj = diff::hide_footer(&knot.message).lines().next().unwrap_or("").to_string();
        let ts = knot.timestamp_secs;
        println!("{} {} {} {}", marker, object::short_id(&cur), fmt_time(ts), subj);
        match knot.parents.first() {
            Some(p) => cur = *p,
            None => break,
        }
    }
    Ok(())
}

fn fmt_time(secs: u64) -> String {
    // Local time, ISO-ish: YYYY-MM-DD HH:MM
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    // civil-from-days (Howard Hinnant's algorithm)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
}

/// Persist blobs produced by a worktree scan so diffs can read them back.
fn persist_blobs(y: &mut Yarnball, blobs: &[(ObjectId, Vec<u8>)]) -> Result<()> {
    let records: Vec<(u8, Vec<u8>)> = blobs
        .iter()
        .filter(|(id, _)| !y.has_object(id))
        .map(|(_, c)| (format::KIND_BLOB, c.clone()))
        .collect();
    if records.is_empty() {
        return Ok(());
    }
    y.mutate(|f| format::append_records(f, &records))
}

fn shear(args: &[String]) -> Result<()> {
    let repo = repo()?;
    let root = repo.wt;
    let ball = repo.ball;
    let mut y = Yarnball::open(&ball)?;

    // Determine (old_tree, new_tree): the "new" side is always the worktree
    // unless two knot args were given (handled by early return).
    let old_tree: Option<Tree> = match args.len() {
        0 => head_tree(&y)?,
        1 => {
            let k = resolve_knot(&y, &args[0])?;
            Some(y.tree(&y.knot(&k)?.tree_id)?)
        }
        2 => {
            let a = resolve_knot(&y, &args[0])?;
            let b = resolve_knot(&y, &args[1])?;
            let ta = y.tree(&y.knot(&a)?.tree_id)?;
            let tb = y.tree(&y.knot(&b)?.tree_id)?;
            return print_tree_diff(&y, Some(&ta), &tb);
        }
        _ => bail!("usage: knit shear [<knot> [<knot>]]"),
    };

    let (wt_tree, _, blobs) = worktree::scan(&root, &y.cache)?;
    persist_blobs(&mut y, &blobs)?;
    print_tree_diff(&y, old_tree.as_ref(), &wt_tree)
}

fn print_tree_diff(y: &Yarnball, old: Option<&Tree>, new: &Tree) -> Result<()> {
    let changes = diff::tree_diff(old, new);
    let mut out = String::new();
    for c in &changes {
        out.push_str(&diff::change_diff(y, c)?);
    }
    print!("{out}");
    Ok(())
}

fn pick(arg: &str) -> Result<()> {
    let repo = repo()?;
    let ball = repo.ball;
    let y = Yarnball::open(&ball)?;
    let id = resolve_knot(&y, arg)?;
    let knot = y.knot(&id)?;
    println!("knot {}", object::short_id(&id));
    println!("author: {}", knot.author);
    println!("date:   {}", fmt_time(knot.timestamp_secs));
    if !knot.is_native() {
        println!("git_id: {}", knot.git_id);
    }
    println!("parents: {}", knot.parents.iter().map(object::short_id).collect::<Vec<_>>().join(" "));
    // Verify the Ed25519 signature against every key the yarnball knows.
    let sig: [u8; 64] = knot.signature;
    let msg = knot.signed_bytes();
    let mut verified = false;
    let mut bad = false;
    for pk in &y.pubkeys {
        match crate::keys::verify(pk, &msg, &sig) {
            Ok(()) => verified = true,
            Err(_) => bad = true,
        }
    }
    if verified {
        println!("signature: verified");
    } else if bad || !y.pubkeys.is_empty() {
        println!("signature: FAILED");
    } else {
        println!("signature: no known keys");
    }
    println!();
    println!("{}", diff::hide_footer(&knot.message));
    println!();
    // Diff vs first parent (or empty tree for root).
    let new_tree = y.tree(&knot.tree_id)?;
    let old_tree = match knot.parents.first() {
        Some(p) => Some(y.tree(&y.knot(p)?.tree_id)?),
        None => None,
    };
    print_tree_diff(&y, old_tree.as_ref(), &new_tree)
}

fn strand(name: Option<&str>) -> Result<()> {
    let repo = repo()?;
    let ball = repo.ball;
    let mut y = Yarnball::open(&ball)?;
    match name {
        None => {
            for (n, id) in &y.strands {
                let marker = if *n == y.head { "*" } else { " " };
                let subj = y
                    .knot(id)
                    .ok()
                    .map(|k| diff::hide_footer(&k.message).lines().next().unwrap_or("").to_string())
                    .unwrap_or_default();
                println!("{} {} {}", marker, n, subj);
            }
        }
        Some(n) => {
            if y.strands.contains_key(n) {
                bail!("strand '{n}' already exists");
            }
            let head_knot = y.head_knot()?.context("no knots yet; run `knit tie` first")?;
            let n = n.to_string();
            y.mutate(|f| {
                format::append_records(f, &[(KIND_REF, object::encode_ref(&n, &head_knot))])
            })?;
            println!("strand '{n}' created at {}", object::short_id(&head_knot));
        }
    }
    Ok(())
}

fn switch(name: &str) -> Result<()> {
    let repo = repo()?;
    let root = repo.wt;
    let ball = repo.ball;
    let mut y = Yarnball::open(&ball)?;
    let target = *y.strands.get(name).context("no such strand")?;
    if name == y.head {
        println!("already on '{name}'");
        return Ok(());
    }
    let new_tree = y.tree(&y.knot(&target)?.tree_id)?;
    let old_tree = head_tree(&y)?;
    // Materialize first, then move HEAD.
    worktree::materialize(&root, &y, &new_tree, old_tree.as_ref())?;
    let n = name.to_string();
    y.mutate(|f| format::append_records(f, &[(KIND_HEAD, object::encode_head(&n))]))?;
    println!("switched to strand '{name}'");
    Ok(())
}

// ---------- merge commands (join / frog / lift) ----------

fn knot_tree(y: &Yarnball, knot_id: &ObjectId) -> Result<Tree> {
    let k = y.knot(knot_id)?;
    y.tree(&k.tree_id)
}

/// Choose how to store `tree`: identical to base reuses the base id (zero
/// bytes), otherwise the smaller of full vs delta encoding. Returns the
/// object id and an optional TREE record payload to append.
fn plan_tree(
    y: &Yarnball,
    tree: &Tree,
    base: Option<(ObjectId, Tree)>,
) -> Result<(ObjectId, Option<Vec<u8>>)> {
    if let Some((base_id, base_tree)) = &base {
        if base_tree == tree {
            return Ok((*base_id, None));
        }
    }
    let full = object::encode_tree(tree);
    let full_id = object::object_id(&full);
    if let Some((base_id, base_tree)) = &base {
        if let Some(delta) = object::encode_tree_delta(base_id, base_tree, tree) {
            let delta_id = object::object_id(&delta);
            if !y.has_object(&delta_id) {
                return Ok((delta_id, Some(delta)));
            }
            return Ok((delta_id, None));
        }
    }
    if !y.has_object(&full_id) {
        Ok((full_id, Some(full)))
    } else {
        Ok((full_id, None))
    }
}

/// The tree of the first parent knot, for delta encoding.
fn base_tree(y: &Yarnball, parents: &[ObjectId]) -> Result<Option<(ObjectId, Tree)>> {
    let Some(p) = parents.first() else { return Ok(None) };
    let knot = y.knot(p)?;
    Ok(Some((knot.tree_id, y.tree(&knot.tree_id)?)))
}

/// INDEX payload: delta against the previous cache when smaller.
fn index_payload(prev: &std::collections::BTreeMap<String, object::CacheEntry>, new: &std::collections::BTreeMap<String, object::CacheEntry>) -> Vec<u8> {
    let full = object::encode_index(new);
    match object::encode_index_delta(prev, new) {
        Some(d) if d.len() < full.len() => d,
        _ => full,
    }
}

fn store_tree(y: &mut Yarnball, tree: &Tree) -> Result<ObjectId> {
    let parents = y.head_knot()?.into_iter().collect::<Vec<_>>();
    let base = base_tree(y, &parents)?;
    let (id, record) = plan_tree(y, tree, base)?;
    if let Some(payload) = record {
        y.mutate(|f| format::append_records(f, &[(KIND_TREE, payload)]))?;
    }
    Ok(id)
}

fn sign_and_commit(
    y: &mut Yarnball,
    root: &Path,
    tree_id: ObjectId,
    parents: Vec<ObjectId>,
    message: &str,
) -> Result<ObjectId> {
    let ks = KeyStorage::new()?;
    let kp = ks.load_keypair()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let mut knot = Knot {
        tree_id,
        parents,
        author: author(),
        timestamp_secs: (now / 1_000_000_000) as u64,
        timestamp_nanos: (now % 1_000_000_000) as u32,
        message: message.to_string(),
        git_id: String::new(),
        signature: [0u8; 64],
    };
    knot.signature = kp.sign(&knot.signed_bytes());
    let knot_id = knot.id();
    let head = y.head.clone();
    let pubkey = kp.public_key_bytes();

    let mut records = vec![(KIND_KNOT, knot.encode())];
    if !y.pubkeys.contains(&pubkey) {
        records.push((format::KIND_KEY, pubkey.to_vec()));
    }
    records.push((KIND_REF, object::encode_ref(&head, &knot_id)));
    y.mutate(|f| format::append_records(f, &records))?;

    // Refresh the stat cache from the materialized worktree.
    let (_, new_cache, blobs) = worktree::scan(root, &BTreeMap::new())?;
    persist_blobs(y, &blobs)?;
    let ip = index_payload(&y.cache, &new_cache);
    y.mutate(|f| format::append_records(f, &[(KIND_INDEX, ip)]))?;
    Ok(knot_id)
}

/// Shared engine for join/frog/lift. `ours` is the HEAD tree; `theirs` and
/// `base` come from the other knot(s). Clean merges commit immediately;
/// snags write conflict markers and leave a pending JOIN record.
fn merge_and_finish(
    root: &Path,
    y: &mut Yarnball,
    base: Option<&Tree>,
    ours: &Tree,
    theirs: &Tree,
    extra_parents: Vec<ObjectId>,
    message: &str,
    label: &str,
) -> Result<()> {
    let head_knot = y.head_knot()?.context("nothing to merge into yet")?;
    let outcome = merge::merge_trees(base, ours, theirs);

    if outcome.conflicts.is_empty() {
        let tree_id = store_tree(y, &outcome.tree)?;
        let mut parents = vec![head_knot];
        parents.extend(extra_parents);
        let id = sign_and_commit(y, root, tree_id, parents, message)?;
        worktree::materialize(root, y, &outcome.tree, Some(ours))?;
        println!("knot {} {}", object::short_id(&id), message);
        return Ok(());
    }

    // Snags: materialize the clean part, then write conflict markers.
    worktree::materialize(root, y, &outcome.tree, Some(ours))?;
    for c in &outcome.conflicts {
        let content = merge::conflict_content(y, c);
        let dest = root.join(&c.path);
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(&dest, content)?;
        println!("snag: {}", c.path);
    }
    let join = object::JoinState { resolved: false, extra_parents, label: label.to_string() };
    y.mutate(|f| format::append_records(f, &[(format::KIND_JOIN, object::encode_join(&join))]))?;
    println!("{} conflict(s); resolve and run `knit tie`", outcome.conflicts.len());
    Ok(())
}

fn join(strand: &str) -> Result<()> {
    let repo = repo()?;
    let root = repo.wt;
    let ball = repo.ball;
    let mut y = Yarnball::open(&ball)?;
    if strand == y.head {
        bail!("cannot join the active strand into itself");
    }
    let their_knot = *y.strands.get(strand).context("no such strand")?;
    let head_knot = y.head_knot()?.context("no knots on this strand yet")?;
    if their_knot == head_knot {
        println!("already up to date");
        return Ok(());
    }

    let base_knot = merge::merge_base(&y, head_knot, their_knot);
    let base_tree = match base_knot {
        Some(b) => Some(knot_tree(&y, &b)?),
        None => None,
    };
    let ours = knot_tree(&y, &head_knot)?;
    let theirs = knot_tree(&y, &their_knot)?;
    let msg = format!("join {strand}");
    merge_and_finish(
        &root, &mut y, base_tree.as_ref(), &ours, &theirs,
        vec![their_knot], &msg, &msg,
    )
}

fn frog(arg: &str) -> Result<()> {
    let repo = repo()?;
    let root = repo.wt;
    let ball = repo.ball;
    let mut y = Yarnball::open(&ball)?;
    let k = resolve_knot(&y, arg)?;
    let knot = y.knot(&k)?;
    let parent = knot.parents.first().copied();
    let ours = knot_tree(&y, &y.head_knot()?.context("no knots on this strand yet")?)?;
    // Revert = merge with base=knot.tree, theirs=knot.parent.tree.
    let base_tree = y.tree(&knot.tree_id)?;
    let theirs = match parent {
        Some(p) => knot_tree(&y, &p)?,
        None => Tree::new(),
    };
    let msg = format!("frog {}", object::short_id(&k));
    let extra = parent.into_iter().collect();
    merge_and_finish(&root, &mut y, Some(&base_tree), &ours, &theirs, extra, &msg, &msg)
}

fn lift(arg: &str) -> Result<()> {
    let repo = repo()?;
    let root = repo.wt;
    let ball = repo.ball;
    let mut y = Yarnball::open(&ball)?;
    let k = resolve_knot(&y, arg)?;
    let knot = y.knot(&k)?;
    let parent = knot.parents.first().copied();
    let ours = knot_tree(&y, &y.head_knot()?.context("no knots on this strand yet")?)?;
    // Cherry-pick = merge with base=knot.parent.tree, theirs=knot.tree.
    let base_tree = match parent {
        Some(p) => knot_tree(&y, &p)?,
        None => Tree::new(),
    };
    let theirs = y.tree(&knot.tree_id)?;
    let msg = format!("lift {}", object::short_id(&k));
    merge_and_finish(&root, &mut y, Some(&base_tree), &ours, &theirs, Vec::new(), &msg, &msg)
}

fn pickup(path: &Path) -> Result<()> {
    let root = std::env::current_dir()?;
    if format::yarnball_path(&root).exists() {
        bail!("a yarnball already exists here; pickup into an empty directory");
    }
    let stats = crate::gitmod::pickup(&root, path)?;
    println!("picked up {}", path.display());
    println!("  commits: {}", stats.commits);
    println!("  trees:   {}", stats.trees);
    println!("  blobs:   {}", stats.blobs);
    if stats.gitlinks_skipped > 0 {
        println!("  skipped {} submodule gitlinks", stats.gitlinks_skipped);
    }
    Ok(())
}

// ---------- mark / stash / sweep / track / loom / block ----------

fn mark(args: MarkArgs) -> Result<()> {
    let repo = repo()?;
    let mut y = Yarnball::open(&repo.ball)?;
    match &args.name {
        None => {
            if y.marks.is_empty() {
                println!("no marks");
            }
            for (name, id) in &y.marks {
                let subj = y.knot(id).ok()
                    .map(|k| diff::hide_footer(&k.message).lines().next().unwrap_or("").to_string())
                    .unwrap_or_default();
                println!("{} {} {}", name, object::short_id(id), subj);
            }
            return Ok(());
        }
        Some(name) => {
            let knot_id = match &args.knot {
                Some(k) => resolve_knot(&y, k)?,
                None => y.head_knot()?.context("no knots yet")?,
            };
            let zero = [0u8; 32];
            let id = if args.delete { zero } else { knot_id };
            y.mutate(|f| format::append_records(f, &[(
                format::KIND_MARK,
                object::encode_mark(!args.delete, name, &id),
            )]))?;
            if args.delete {
                println!("mark '{name}' removed");
            } else {
                println!("mark '{name}' -> {}", object::short_id(&knot_id));
            }
        }
    }
    Ok(())
}

fn stash(cmd: Option<&StashCmd>, message: Option<&str>) -> Result<()> {
    let repo = repo()?;
    let mut y = Yarnball::open(&repo.ball)?;
    match cmd {
        Some(StashCmd::List) => {
            if y.stashes.is_empty() {
                println!("no stashes");
            }
            for (i, s) in y.stashes.iter().enumerate().rev() {
                println!("stash@{{{}}} {} {}", i, fmt_time(s.timestamp_secs), s.label);
            }
            return Ok(());
        }
        Some(StashCmd::Pop) => {
            let s = y.stashes.last().context("no stashes")?.clone();
            let tree = y.tree(&s.tree_id)?;
            let head_tree = head_tree(&y)?;
            worktree::materialize(&repo.wt, &y, &tree, head_tree.as_ref())?;
            let drop = object::encode_stash_drop(&s.stash_id());
            y.mutate(|f| format::append_records(f, &[(format::KIND_STASH_DROP, drop)]))?;
            // Refresh cache.
            let (_, cache, blobs) = worktree::scan(&repo.wt, &std::collections::BTreeMap::new())?;
            persist_blobs(&mut y, &blobs)?;
            let ip = index_payload(&y.cache, &cache);
            y.mutate(|f| format::append_records(f, &[(KIND_INDEX, ip)]))?;
            println!("applied {}", s.label);
            return Ok(());
        }
        None => {}
    }

    // push (default)
    let (tree, _, blobs) = worktree::scan(&repo.wt, &y.cache)?;
    let tree_payload = object::encode_tree(&tree);
    let tree_id = object::object_id(&tree_payload);
    let label = message.unwrap_or("stash").to_string();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let entry = object::StashEntry { tree_id, label: label.clone(), timestamp_secs: now };

    let mut records: Vec<(u8, Vec<u8>)> = blobs
        .iter()
        .filter(|(id, _)| !y.has_object(id))
        .map(|(_, c)| (format::KIND_BLOB, c.clone()))
        .collect();
    if !y.has_object(&tree_id) {
        records.push((KIND_TREE, tree_payload));
    }
    records.push((format::KIND_STASH, entry.encode()));
    y.mutate(|f| format::append_records(f, &records))?;

    // Restore the worktree to HEAD.
    if let Some(t) = head_tree(&y)? {
        worktree::materialize(&repo.wt, &y, &t, Some(&tree))?;
    }
    let (_, cache, blobs2) = worktree::scan(&repo.wt, &std::collections::BTreeMap::new())?;
    persist_blobs(&mut y, &blobs2)?;
    let ip = index_payload(&y.cache, &cache);
    y.mutate(|f| format::append_records(f, &[(KIND_INDEX, ip)]))?;
    println!("stashed: {}", label);
    Ok(())
}

fn sweep() -> Result<()> {
    let repo = repo()?;
    let y = Yarnball::open(&repo.ball)?;
    let head = head_tree(&y)?;
    let tracked: std::collections::HashSet<&String> = head
        .as_ref()
        .map(|t| t.keys().collect())
        .unwrap_or_default();
    let (wt_tree, _, _) = worktree::scan(&repo.wt, &y.cache)?;
    let mut removed = 0;
    for path in wt_tree.keys() {
        if !tracked.contains(path) {
            let p = repo.wt.join(path);
            if p.is_file() || p.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                fs::remove_file(&p)?;
                removed += 1;
            }
        }
    }
    println!("swept {} untracked file(s)", removed);
    Ok(())
}

fn track(file: &str) -> Result<()> {
    let repo = repo()?;
    let y = Yarnball::open(&repo.ball)?;
    let mut cur = y.head_knot()?.context("no knots yet")?;
    let mut touching = Vec::new();
    loop {
        let knot = y.knot(&cur)?;
        let tree = y.tree(&knot.tree_id)?;
        let parent_tree = match knot.parents.first() {
            Some(p) => Some(y.tree(&y.knot(p)?.tree_id)?),
            None => None,
        };
        let now_blob = tree.get(file).map(|e| e.blob_id);
        let old_blob = parent_tree.as_ref().and_then(|t| t.get(file)).map(|e| e.blob_id);
        if now_blob != old_blob {
            touching.push((cur, knot.clone()));
        }
        match knot.parents.first() {
            Some(p) => cur = *p,
            None => break,
        }
    }
    if touching.is_empty() {
        println!("no knots touched {file}");
        return Ok(());
    }
    for (id, knot) in touching {
        let subj = diff::hide_footer(&knot.message).lines().next().unwrap_or("");
        println!("{} {} {} {}", object::short_id(&id), fmt_time(knot.timestamp_secs), knot.author, subj);
    }
    Ok(())
}

fn loom(strand: &str, dir: &Path) -> Result<()> {
    let repo = repo()?;
    let y = Yarnball::open(&repo.ball)?;
    let tip = *y.strands.get(strand).context("no such strand")?;
    let tree = knot_tree(&y, &tip)?;
    if dir.join(".knit").exists() || dir.join(".loom").exists() {
        bail!("{} already contains a yarnball or loom pointer", dir.display());
    }
    fs::create_dir_all(dir)?;
    worktree::materialize(dir, &y, &tree, None)?;
    fs::write(
        dir.join(".loom"),
        format!("root {}\nstrand {}\n", repo.ball.display(), strand),
    )?;
    println!("loom '{}' at {}", strand, dir.display());
    Ok(())
}

fn block() -> Result<()> {
    let repo = repo()?;
    let y = Yarnball::open(&repo.ball)?;

    // Collect reachable object ids: knots (strand tips + marks + ancestors),
    // their trees, and all blobs referenced by those trees. Stash trees too.
    let mut knot_ids: std::collections::BTreeSet<ObjectId> = std::collections::BTreeSet::new();
    let mut tree_ids: std::collections::BTreeSet<ObjectId> = std::collections::BTreeSet::new();
    let mut blob_ids: std::collections::BTreeSet<ObjectId> = std::collections::BTreeSet::new();

    let mut knot_queue: Vec<ObjectId> = y.strands.values().copied().collect();
    knot_queue.extend(y.marks.values().copied());
    for s in &y.stashes {
        tree_ids.insert(s.tree_id);
    }
    while let Some(id) = knot_queue.pop() {
        if !knot_ids.insert(id) {
            continue;
        }
        if let Ok(k) = y.knot(&id) {
            tree_ids.insert(k.tree_id);
            knot_queue.extend(k.parents);
        }
    }
    let mut tree_queue: Vec<ObjectId> = tree_ids.iter().copied().collect();
    while let Some(tid) = tree_queue.pop() {
        if let Ok(t) = y.tree(&tid) {
            for e in t.values() {
                blob_ids.insert(e.blob_id);
            }
        }
    }

    let mut keep: std::collections::HashSet<ObjectId> =
        knot_ids.union(&tree_ids).chain(blob_ids.iter()).copied().collect();

    // Rewrite the file under the exclusive lock.
    let frames = format::read_frames(&y.path)?;

    // Delta trees reference base trees; keep the whole chain.
    loop {
        let mut added = false;
        for fr in &frames {
            if fr.kind == KIND_TREE && keep.contains(&object::object_id(&fr.payload)) {
                if let Some(b) = object::tree_delta_base(&fr.payload) {
                    added |= keep.insert(b);
                }
            }
        }
        if !added {
            break;
        }
    }

    let tmp = y.path.with_extension("knit.tmp");
    {
        use std::io::Write;
        let mut f = fs::File::create(&tmp)?;
        let mut header = [0u8; format::HEADER_LEN];
        header[..4].copy_from_slice(format::MAGIC);
        header[4..8].copy_from_slice(&format::VERSION.to_le_bytes());
        f.write_all(&header)?;
        for fr in &frames {
            match fr.kind {
                format::KIND_BLOB | KIND_TREE | KIND_KNOT => {
                    let id = object::object_id(&fr.payload);
                    if keep.contains(&id) {
                        f.write_all(&format::encode_frame(fr.kind, &fr.payload))?;
                    }
                }
                KIND_INDEX => {}
                _ => {
                    f.write_all(&format::encode_frame(fr.kind, &fr.payload))?;
                }
            }
        }
        // INDEX may be a delta; write the resolved cache in full.
        f.write_all(&format::encode_frame(KIND_INDEX, &object::encode_index(&y.cache)))?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &y.path)?;
    println!("blocked: {} knots, {} trees, {} blobs kept", knot_ids.len(), tree_ids.len(), blob_ids.len());
    Ok(())
}

// ---------- hub / send / gather / haul ----------

fn hub(cmd: HubCmd) -> Result<()> {
    let repo = repo()?;
    let mut y = Yarnball::open(&repo.ball)?;
    match cmd {
        HubCmd::Add { name, url } => {
            y.mutate(|f| format::append_records(f, &[(
                format::KIND_HUB,
                object::encode_hub(&name, &url),
            )]))?;
            println!("hub '{name}' -> {url}");
        }
        HubCmd::List => {
            if y.hubs.is_empty() {
                println!("no hubs");
            }
            for (n, u) in &y.hubs {
                println!("{n} {u}");
            }
        }
        HubCmd::Remove { name } => {
            y.mutate(|f| format::append_records(f, &[(
                format::KIND_HUB,
                object::encode_hub(&name, ""),
            )]))?;
            println!("hub '{name}' removed");
        }
    }
    Ok(())
}

/// A resolved hub endpoint.
enum HubTarget {
    /// Local directory of bare yarnballs.
    Dir(PathBuf),
    /// SSH endpoint: (user@host, remote knits dir)
    Ssh { dest: String, dir: String },
    /// GitHub repo "owner/name", driven through `gh api` (no git involved).
    Github { repo: String },
}

fn hub_target(repo: &Repo, y: &Yarnball, name: &str) -> Result<(HubTarget, String)> {
    let url = y.hubs.get(name).with_context(|| format!("no hub '{name}'; `knit hub add` first"))?.clone();
    let repo_name = repo.ball.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    // url forms: github.com/o/r | https://github.com/o/r | gh:o/r |
    //            file:///dir | /dir | dir | user@host[:dir]
    let gh_repo = url
        .strip_prefix("gh:")
        .map(|s| s.to_string())
        .or_else(|| url.strip_prefix("https://github.com/").map(|s| s.to_string()))
        .or_else(|| url.strip_prefix("http://github.com/").map(|s| s.to_string()))
        .or_else(|| url.strip_prefix("github.com/").map(|s| s.to_string()));
    if let Some(r) = gh_repo {
        let r = r.trim_end_matches('/').trim_end_matches(".git").to_string();
        if r.split('/').count() == 2 {
            return Ok((HubTarget::Github { repo: r }, repo_name));
        }
        bail!("bad github hub url '{url}'; want github.com/owner/repo");
    }
    if let Some(rest) = url.strip_prefix("file://") {
        return Ok((HubTarget::Dir(PathBuf::from(rest)), repo_name));
    }
    if url.starts_with('/') || !url.contains('@') {
        return Ok((HubTarget::Dir(PathBuf::from(&url)), repo_name));
    }
    let (dest, dir) = match url.split_once(':') {
        Some((d, p)) if !p.starts_with('/') => (d.to_string(), format!("{p}")),
        Some((d, p)) => (d.to_string(), p.to_string()),
        None => (url.clone(), "$HOME/.knits".to_string()),
    };
    Ok((HubTarget::Ssh { dest, dir }, repo_name))
}

fn remote_path(dir: &str, name: &str) -> String {
    format!("{dir}/{name}.knit")
}

/// Read the remote yarnball into a temp file. Returns None if absent.
fn fetch_remote(target: &HubTarget, name: &str) -> Result<Option<PathBuf>> {
    let tmp = std::env::temp_dir().join(format!("knit-remote-{}-{}", name, std::process::id()));
    match target {
        HubTarget::Dir(d) => {
            let p = d.join(format!("{name}.knit"));
            if !p.exists() {
                return Ok(None);
            }
            fs::copy(&p, &tmp)?;
        }
        HubTarget::Github { repo } => {
            if !github_fetch(repo, &tmp)? {
                return Ok(None);
            }
        }
        HubTarget::Ssh { dest, dir } => {
            let remote = remote_path(dir, name);
            let status = std::process::Command::new("ssh")
                .arg(dest)
                .arg(format!("test -f '{remote}' && cat '{remote}' || knits --dir '{dir}' cat '{name}'"))
                .stdout(fs::File::create(&tmp)?)
                .status()?;
            if !status.success() || fs::metadata(&tmp).map(|m| m.len() == 0).unwrap_or(true) {
                let _ = fs::remove_file(&tmp);
                return Ok(None);
            }
        }
    }
    Ok(Some(tmp))
}

/// Object ids present in a `.knit` file (for divergence checks).
fn file_object_ids(path: &Path) -> Result<std::collections::HashSet<ObjectId>> {
    let mut ids = std::collections::HashSet::new();
    for f in format::read_frames(path)? {
        if matches!(f.kind, format::KIND_BLOB | KIND_TREE | KIND_KNOT) {
            ids.insert(object::object_id(&f.payload));
        }
    }
    Ok(ids)
}

fn send(hub_name: &str) -> Result<()> {
    let repo = repo()?;
    let y = Yarnball::open(&repo.ball)?;
    let (target, name) = hub_target(&repo, &y, hub_name)?;

    match &target {
        HubTarget::Github { repo: gh_repo } => {
            github_send(&y, gh_repo)?;
            return Ok(());
        }
        _ => {}
    }

    // Refuse to clobber a diverged remote.
    if let Some(remote) = fetch_remote(&target, &name)? {
        let remote_ids = file_object_ids(&remote)?;
        let _ = fs::remove_file(&remote);
        let missing: Vec<_> = remote_ids.iter().filter(|id| !y.has_object(id)).collect();
        if !missing.is_empty() {
            bail!("remote '{hub_name}' has {} object(s) we lack; run `knit gather {hub_name}` first", missing.len());
        }
    }

    match &target {
        HubTarget::Dir(d) => {
            fs::create_dir_all(d)?;
            let dest = d.join(format!("{name}.knit"));
            let tmp = dest.with_extension("knit.tmp");
            fs::copy(&y.path, &tmp)?;
            fs::rename(&tmp, &dest)?;
        }
        HubTarget::Ssh { dest, dir } => {
            let mut child = std::process::Command::new("ssh")
                .arg(dest)
                .arg(format!("mkdir -p '{dir}' && knits --dir '{dir}' receive '{name}'"))
                .stdin(fs::File::open(&y.path)?)
                .spawn()?;
            let status = child.wait()?;
            if !status.success() {
                bail!("ssh send failed");
            }
        }
        HubTarget::Github { .. } => unreachable!(),
    }
    println!("sent {} to {hub_name}", y.path.display());
    Ok(())
}

/// Gather remote objects into the local yarnball. Returns the remote HEAD
/// strand name when present.
fn gather(hub_name: &str, haul: bool) -> Result<()> {
    let repo = repo()?;
    let mut y = Yarnball::open(&repo.ball)?;
    let (target, name) = hub_target(&repo, &y, hub_name)?;
    let Some(remote_path_buf) = fetch_remote(&target, &name)? else {
        println!("nothing to gather from {hub_name}");
        return Ok(());
    };

    let frames = format::read_frames(&remote_path_buf)?;
    let _ = fs::remove_file(&remote_path_buf);

    let mut records: Vec<(u8, Vec<u8>)> = Vec::new();
    // Last REF per strand name wins.
    let mut remote_refs: std::collections::BTreeMap<String, ObjectId> =
        std::collections::BTreeMap::new();
    let mut remote_head: Option<String> = None;
    let mut new_objects = 0usize;
    let mut new_pubkeys: Vec<[u8; 32]> = Vec::new();

    for f in &frames {
        match f.kind {
            format::KIND_BLOB | KIND_TREE | KIND_KNOT => {
                let id = object::object_id(&f.payload);
                if !y.has_object(&id) {
                    records.push((f.kind, f.payload.clone()));
                    new_objects += 1;
                }
            }
            format::KIND_KEY => {
                if f.payload.len() == 32 {
                    let pk: [u8; 32] = f.payload[..].try_into().unwrap();
                    if !y.pubkeys.contains(&pk) {
                        new_pubkeys.push(pk);
                        records.push((f.kind, f.payload.clone()));
                    }
                }
            }
            KIND_REF => {
                if let Ok((s, k)) = object::decode_ref(&f.payload) {
                    remote_refs.insert(s, k);
                }
            }
            KIND_HEAD => {
                remote_head = object::decode_head(&f.payload).ok();
            }
            format::KIND_MARK => records.push((f.kind, f.payload.clone())),
            _ => {}
        }
    }

    // Strand naming: absent -> same name; divergent -> name@hub.
    for (s, k) in remote_refs.iter() {
        match y.strands.get(s) {
            Some(existing) if *existing == *k => continue,
            Some(_) => {
                let diverged = format!("{s}@{hub_name}");
                records.push((KIND_REF, object::encode_ref(&diverged, k)));
                println!("gathered diverged strand '{s}' as '{diverged}'");
            }
            None => {
                records.push((KIND_REF, object::encode_ref(s, k)));
                println!("gathered strand '{s}'");
            }
        }
    }
    if !records.is_empty() {
        y.mutate(|f| format::append_records(f, &records))?;
    }
    println!("gathered {} object(s), {} new key(s) from {hub_name}", new_objects, new_pubkeys.len());

    if haul {
        // Join the remote head strand (or its diverged twin).
        if let Some(head_name) = remote_head {
            if remote_refs.contains_key(&head_name) {
                let remote_tip = remote_refs.get(&head_name);
                let local_tip = y.strands.get(&head_name);
                let up_to_date = local_tip.is_some() && local_tip == remote_tip;
                if !up_to_date {
                    let target = if local_tip.is_some() {
                        format!("{head_name}@{hub_name}")
                    } else {
                        head_name
                    };
                    join(&target)?;
                }
            }
        }
    }
    Ok(())
}

// ---------- github hub (via `gh api`, never git) ----------

/// Run `gh api <endpoint>` with an optional JSON body file. Returns parsed JSON.
fn gh_api(endpoint: &str, method: &str, body: Option<&serde_json::Value>) -> Result<serde_json::Value> {
    let mut cmd = std::process::Command::new("gh");
    cmd.arg("api").arg(endpoint).arg("-X").arg(method);
    let tmp;
    if let Some(b) = body {
        tmp = std::env::temp_dir().join(format!("knit-gh-{}", std::process::id()));
        fs::write(&tmp, serde_json::to_string(b)?)?;
        cmd.arg("--input").arg(&tmp);
    }
    let out = cmd.output().context("failed to run `gh`; is it installed and authed?")?;
    let _ = tmp;
    if !out.status.success() {
        bail!("gh api {endpoint} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null))
}

fn gh_blob(gh_repo: &str, content: &[u8]) -> Result<String> {
    let v = gh_api(&format!("repos/{gh_repo}/git/blobs"), "POST", Some(&serde_json::json!({
        "content": base64_encode(content),
        "encoding": "base64",
    })))?;
    Ok(v["sha"].as_str().context("blob response missing sha")?.to_string())
}

fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Upload a tree level: `files` maps full path -> (git mode, blob sha).
/// `prefix` is the directory this level represents ("" = root).
fn gh_tree(gh_repo: &str, files: &BTreeMap<String, (String, String)>, prefix: &str) -> Result<String> {
    // Group immediate children under prefix.
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut dirs_seen: BTreeMap<String, ()> = BTreeMap::new();
    for (path, (mode, sha)) in files {
        let Some(rest) = path.strip_prefix(prefix) else { continue };
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        if rest.is_empty() { continue; }
        match rest.split_once('/') {
            None => {
                entries.push(serde_json::json!({
                    "path": rest, "mode": mode, "type": "blob", "sha": sha,
                }));
            }
            Some((dir, _)) => {
                dirs_seen.insert(dir.to_string(), ());
            }
        }
    }
    for dir in dirs_seen.keys() {
        let sub_prefix = if prefix.is_empty() { dir.clone() } else { format!("{prefix}/{dir}") };
        let sub_files: BTreeMap<String, (String, String)> = files
            .iter()
            .filter(|(p, _)| p.starts_with(&format!("{sub_prefix}/")))
            .map(|(p, (m, s))| (p.clone(), (m.clone(), s.clone())))
            .collect();
        let tree_sha = gh_tree(gh_repo, &sub_files, &sub_prefix)?;
        entries.push(serde_json::json!({
            "path": dir, "mode": "040000", "type": "tree", "sha": tree_sha,
        }));
    }
    entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    let v = gh_api(&format!("repos/{gh_repo}/git/trees"), "POST", Some(&serde_json::json!({
        "tree": entries,
    })))?;
    Ok(v["sha"].as_str().context("tree response missing sha")?.to_string())
}

/// Push the HEAD tree plus the .knit yarnball to a GitHub repo as one commit.
fn github_send(y: &Yarnball, gh_repo: &str) -> Result<()> {
    let head_knot = y.head_knot()?.context("no knots yet; tie first")?;
    let knot = y.knot(&head_knot)?;
    let tree = y.tree(&knot.tree_id)?;

    // Find current head of the remote default branch.
    let repo_info = gh_api(&format!("repos/{gh_repo}"), "GET", None)?;
    let branch = repo_info["default_branch"].as_str().unwrap_or("main").to_string();
    let head_sha = gh_api(&format!("repos/{gh_repo}/git/ref/heads/{branch}"), "GET", None)
        .ok()
        .and_then(|v| v["object"]["sha"].as_str().map(|s| s.to_string()));

    let subj = diff::hide_footer(&knot.message).lines().next().unwrap_or("").to_string();
    let msg = format!("knit {} {}", object::short_id(&head_knot), subj);

    // GitHub refuses git-database writes on a repo with zero commits.
    // Bootstrap with a single Contents-API commit carrying the yarnball.
    if head_sha.is_none() {
        let ball_bytes = fs::read(&y.path)?;
        gh_api(&format!("repos/{gh_repo}/contents/.knit"), "PUT", Some(&serde_json::json!({
            "message": msg,
            "content": base64_encode(&ball_bytes),
            "branch": branch,
        })))?;
        println!("sent {} -> github:{gh_repo} ({branch}, bootstrap)", object::short_id(&head_knot));
        return Ok(());
    }

    // Collect (path -> mode, blob sha) for every file in HEAD, plus .knit.
    let mut files: BTreeMap<String, (String, String)> = BTreeMap::new();
    for (path, e) in &tree {
        let bytes = y.blob_bytes(&e.blob_id)?;
        let sha = gh_blob(gh_repo, &bytes)?;
        let mode = match e.mode {
            worktree::MODE_EXE => "100755",
            worktree::MODE_LINK => "120000",
            _ => "100644",
        };
        files.insert(path.clone(), (mode.to_string(), sha));
    }
    let ball_bytes = fs::read(&y.path)?;
    let ball_sha = gh_blob(gh_repo, &ball_bytes)?;
    files.insert(".knit".to_string(), ("100644".to_string(), ball_sha));

    let tree_sha = gh_tree(gh_repo, &files, "")?;

    let parents: Vec<String> = vec![head_sha.clone().unwrap()];
    let commit = gh_api(&format!("repos/{gh_repo}/git/commits"), "POST", Some(&serde_json::json!({
        "message": msg,
        "tree": tree_sha,
        "parents": parents,
    })))?;
    let commit_sha = commit["sha"].as_str().context("commit response missing sha")?.to_string();

    gh_api(&format!("repos/{gh_repo}/git/refs/heads/{branch}"), "PATCH", Some(&serde_json::json!({
        "sha": commit_sha,
    })))?;
    println!("sent {} -> github:{gh_repo} ({branch} @ {})", object::short_id(&head_knot), &commit_sha[..7]);
    Ok(())
}

/// Download `.knit` from a GitHub repo via the contents API.
fn github_fetch(gh_repo: &str, tmp: &Path) -> Result<bool> {
    let mut cmd = std::process::Command::new("gh");
    cmd.arg("api")
        .arg(format!("repos/{gh_repo}/contents/.knit"))
        .arg("-H")
        .arg("Accept: application/vnd.github.raw")
        .stdout(fs::File::create(tmp)?);
    let status = cmd.status().context("failed to run `gh`")?;
    if !status.success() || fs::metadata(tmp).map(|m| m.len() == 0).unwrap_or(true) {
        let _ = fs::remove_file(tmp);
        return Ok(false);
    }
    Ok(true)
}
