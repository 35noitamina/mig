use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use git2::{ObjectType, Oid, Repository, TreeWalkMode, TreeWalkResult};

use crate::oplog;

const AUTO_LOOSE_LIMIT: usize = 1000;
const PRUNE_AGE: Duration = Duration::from_secs(14 * 24 * 3600);

pub struct GcReport {
  pub objects: usize,
  pub removed_loose: usize,
  pub removed_packs: usize,
  pub before: u64,
  pub after: u64,
}

fn objects_dir(repo: &Repository) -> PathBuf {
  repo.path().join("objects")
}

fn loose_objects(dir: &Path) -> Vec<(Oid, PathBuf)> {
  let mut out = Vec::new();
  let Ok(rd) = std::fs::read_dir(dir) else { return out };
  for sub in rd.flatten() {
    let prefix = sub.file_name().to_string_lossy().into_owned();
    if prefix.len() != 2 || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
      continue;
    }
    let Ok(files) = std::fs::read_dir(sub.path()) else {
      continue;
    };
    for f in files.flatten() {
      if let Ok(oid) = Oid::from_str(&format!("{prefix}{}", f.file_name().to_string_lossy())) {
        out.push((oid, f.path()));
      }
    }
  }
  out
}

fn dir_size(p: &Path) -> u64 {
  let Ok(rd) = std::fs::read_dir(p) else { return 0 };
  rd.flatten()
    .map(|e| match e.metadata() {
      Ok(m) if m.is_dir() => dir_size(&e.path()),
      Ok(m) => m.len(),
      Err(_) => 0,
    })
    .sum()
}

// Object ids listed in a pack index (v1 or v2).
fn read_idx(path: &Path) -> Result<HashSet<Oid>> {
  let data = std::fs::read(path)?;
  let (fanout_at, names_at, stride) = if data.starts_with(b"\xfftOc") {
    (8, 8 + 1024, 20)
  } else {
    (0, 1024 + 4, 24)
  };
  if data.len() < fanout_at + 1024 {
    bail!("truncated pack index {}", path.display());
  }
  let n = u32::from_be_bytes(data[fanout_at + 1020..fanout_at + 1024].try_into()?) as usize;
  let mut out = HashSet::with_capacity(n);
  for i in 0..n {
    let at = names_at + i * stride;
    let raw = data.get(at..at + 20).context("truncated pack index")?;
    out.insert(Oid::from_bytes(raw)?);
  }
  Ok(out)
}

pub fn loose_count(repo: &Repository) -> usize {
  loose_objects(&objects_dir(repo)).len()
}

pub fn auto(repo: &Repository) -> Result<Option<GcReport>> {
  if loose_count(repo) > AUTO_LOOSE_LIMIT {
    gc(repo).map(Some)
  } else {
    Ok(None)
  }
}

// Everything that must survive: refs, reflogs, mig's op log and the index.
fn roots(repo: &Repository) -> Result<Vec<Oid>> {
  let mut out = Vec::new();
  let mut names = vec!["HEAD".to_string()];
  for r in repo.references()? {
    let r = r?;
    if let Some(t) = r.target() {
      out.push(t);
    }
    if let Ok(n) = r.name() {
      names.push(n.to_string());
    }
  }
  if let Ok(h) = repo.head()
    && let Some(t) = h.target()
  {
    out.push(t);
  }
  for n in &names {
    if let Ok(log) = repo.reflog(n) {
      for e in log.iter() {
        out.push(e.id_new());
        out.push(e.id_old());
      }
    }
  }
  for op in oplog::load(repo)?.ops {
    for snap in [op.before, op.after] {
      out.extend(snap.refs.values().filter_map(|s| Oid::from_str(s).ok()));
      out.extend(Oid::from_str(&snap.head).ok());
    }
  }
  out.retain(|o| !o.is_zero());
  out.sort();
  out.dedup();
  Ok(out)
}

pub fn gc(repo: &Repository) -> Result<GcReport> {
  let objects = objects_dir(repo);
  let pack_dir = objects.join("pack");
  let before = dir_size(&objects);

  let mut pb = repo.packbuilder()?;
  let mut walk = repo.revwalk()?;
  let mut pending = roots(repo)?;
  while let Some(id) = pending.pop() {
    let Ok(obj) = repo.find_object(id, None) else { continue };
    match obj.kind() {
      Some(ObjectType::Commit) => walk.push(id)?,
      Some(ObjectType::Tag) => {
        pb.insert_object(id, None)?;
        pending.push(obj.as_tag().unwrap().target_id());
      }
      _ => pb.insert_recursive(id, None)?,
    }
  }
  pb.insert_walk(&mut walk)?;
  for e in repo.index()?.iter() {
    let _ = pb.insert_object(e.id, None);
  }
  let count = pb.object_count();
  if count == 0 {
    return Ok(GcReport {
      objects: 0,
      removed_loose: 0,
      removed_packs: 0,
      before,
      after: before,
    });
  }
  std::fs::create_dir_all(&pack_dir)?;
  pb.write(&pack_dir, 0)?;
  let name = pb.name()?.context("pack was not written")?.to_string();
  let new_idx = pack_dir.join(format!("pack-{name}.idx"));
  let packed = read_idx(&new_idx)?;

  let now = SystemTime::now();
  let mut removed_loose = 0;
  for (oid, path) in loose_objects(&objects) {
    let old = std::fs::metadata(&path)
      .and_then(|m| m.modified())
      .map(|t| now.duration_since(t).unwrap_or_default() > PRUNE_AGE)
      .unwrap_or(false);
    if (packed.contains(&oid) || old) && std::fs::remove_file(&path).is_ok() {
      removed_loose += 1;
      if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir(parent);
      }
    }
  }

  let mut removed_packs = 0;
  for e in std::fs::read_dir(&pack_dir)?.flatten() {
    let p = e.path();
    if p.extension().is_none_or(|x| x != "idx") || p == new_idx || p.with_extension("keep").exists() {
      continue;
    }
    if read_idx(&p).is_ok_and(|ids| ids.is_subset(&packed)) {
      for ext in ["pack", "rev", "bitmap", "mtimes", "idx"] {
        let _ = std::fs::remove_file(p.with_extension(ext));
      }
      removed_packs += 1;
    }
  }
  repo.odb()?.refresh()?;
  Ok(GcReport {
    objects: count,
    removed_loose,
    removed_packs,
    before,
    after: dir_size(&objects),
  })
}

pub struct SizeReport {
  pub loose_count: usize,
  pub loose_bytes: u64,
  pub pack_count: usize,
  pub pack_bytes: u64,
  pub largest: Vec<(String, u64)>,
}

pub fn size(repo: &Repository, top: usize) -> Result<SizeReport> {
  let objects = objects_dir(repo);
  let loose = loose_objects(&objects);
  let loose_bytes = loose
    .iter()
    .filter_map(|(_, p)| std::fs::metadata(p).ok())
    .map(|m| m.len())
    .sum();
  let packs: Vec<u64> = std::fs::read_dir(objects.join("pack"))
    .map(|rd| {
      rd.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .filter_map(|e| e.metadata().ok().map(|m| m.len()))
        .collect()
    })
    .unwrap_or_default();

  let odb = repo.odb()?;
  let mut walk = repo.revwalk()?;
  let _ = walk.push_glob("refs/*");
  let _ = walk.push_head();
  let mut seen_trees = HashSet::new();
  let mut blobs: HashMap<Oid, (String, u64)> = HashMap::new();
  for id in walk {
    let tree = repo.find_commit(id?)?.tree()?;
    if !seen_trees.insert(tree.id()) {
      continue;
    }
    tree.walk(TreeWalkMode::PreOrder, |root, entry| {
      if entry.kind() == Some(ObjectType::Blob) && !blobs.contains_key(&entry.id()) {
        let size = odb.read_header(entry.id()).map(|(s, _)| s as u64).unwrap_or(0);
        blobs.insert(entry.id(), (format!("{root}{}", entry.name().unwrap_or("?")), size));
      }
      TreeWalkResult::Ok
    })?;
  }
  let mut largest: Vec<(String, u64)> = blobs.into_values().collect();
  largest.sort_by(|a, b| b.1.cmp(&a.1));
  largest.truncate(top);
  Ok(SizeReport {
    loose_count: loose.len(),
    loose_bytes,
    pack_count: packs.len(),
    pack_bytes: packs.iter().sum(),
    largest,
  })
}
