use crate::{
    app::{self, Catalog, Header, Row, Settings, Stamp},
    engine::{self, Base, Patch, Result},
};
use memchr::{memchr, memrchr};
use rayon::prelude::*;
use rusqlite::{Connection, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Instant,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanFile {
    pub path: String,
    pub physical: String,
    #[serde(flatten)]
    pub stamp: Stamp,
    pub patches: Vec<Patch>,
    #[serde(default)]
    pub bases: Vec<Base>,
    #[serde(default)]
    pub has_ordinals: bool,
    #[serde(default)]
    pub inplace: bool,
    #[serde(default)]
    pub temp: String,
    #[serde(default)]
    pub original: String,
    #[serde(default)]
    pub prepared_mtime: Option<i64>,
    #[serde(skip)]
    ends: Vec<u64>,
    #[serde(skip)]
    shifts: Vec<i64>,
}
impl PlanFile {
    fn map_init(&mut self) -> Result<()> {
        self.patches.sort_by_key(|p| p.start);
        self.ends.clear();
        self.shifts.clear();
        let mut delta = 0i64;
        let mut end = 0;
        for p in &self.patches {
            if p.start < end
                || p.end < p.start
                || p.end > self.stamp.size
                || p.old.len() as u64 != p.end - p.start
            {
                return Err("Invalid/overlapping patch".into());
            }
            delta += p.new.len() as i64 - p.old.len() as i64;
            self.ends.push(p.end);
            self.shifts.push(delta);
            end = p.end;
        }
        Ok(())
    }
    fn map(&self, offset: u64) -> Result<u64> {
        if offset > self.stamp.size {
            return Err("Offset outside rollout".into());
        }
        let i = self.ends.partition_point(|&e| e <= offset);
        let delta = if i == 0 { 0 } else { self.shifts[i - 1] };
        offset
            .checked_add_signed(delta)
            .ok_or_else(|| "Offset overflow".into())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Index {
    pub table: String,
    pub columns: Vec<String>,
    pub rowid: i64,
    pub old: Vec<Option<i64>>,
    pub new: Vec<Option<i64>>,
    #[serde(default)]
    pub repaired: bool,
}
#[derive(Serialize, Deserialize)]
struct Journal {
    version: u32,
    database_dir: String,
    provider: String,
    rows: Vec<Row>,
    files: Vec<PlanFile>,
    indices: Vec<Index>,
    #[serde(with = "crate::engine::bytes")]
    config_old: Vec<u8>,
    #[serde(with = "crate::engine::bytes")]
    config_new: Vec<u8>,
    #[serde(default)]
    committed: bool,
}
fn ordinal_regex() -> &'static regex::bytes::Regex {
    static R: OnceLock<regex::bytes::Regex> = OnceLock::new();
    R.get_or_init(|| regex::bytes::Regex::new(r#""ordinal"\s*:\s*(\d+)"#).unwrap())
}
fn boundary(raw: &[u8], offset: usize) -> bool {
    if offset > raw.len() {
        return false;
    }
    if offset == 0 || raw[offset - 1] == b'\n' {
        return true;
    }
    if raw.get(offset).is_some_and(|b| b"\r\n".contains(b)) {
        let start = memrchr(b'\n', &raw[..offset]).map_or(0, |i| i + 1);
        return serde_json::from_slice::<Value>(&raw[start..offset]).is_ok();
    }
    false
}
fn matches(raw: &[u8], offset: i64, ordinal: Option<i64>, start: bool) -> bool {
    if offset < 0 || !boundary(raw, offset as usize) {
        return false;
    }
    let Some(expected) = ordinal else {
        return true;
    };
    if !start && expected == -1 {
        return offset == 0;
    }
    let mut at = offset as usize;
    if start {
        while raw.get(at).is_some_and(|b| b"\r\n".contains(b)) {
            at += 1;
        }
    } else {
        while at > 0 && b"\r\n".contains(&raw[at - 1]) {
            at -= 1;
        }
        at = memrchr(b'\n', &raw[..at]).map_or(0, |i| i + 1);
    }
    ordinal_regex()
        .captures(&raw[at..raw.len().min(at + 180)])
        .and_then(|c| std::str::from_utf8(&c[1]).ok()?.parse::<i64>().ok())
        == Some(expected)
}
fn ordinal_positions(raw: &[u8], wanted: &HashSet<i64>) -> Result<HashMap<i64, (u64, u64)>> {
    let cuts = if raw.len() >= 64 * 1024 * 1024 && rayon::current_num_threads() > 1 {
        engine::record_cuts(raw, 8 * 1024 * 1024)?
    } else {
        vec![0, raw.len()]
    };
    ordinal_positions_chunks(raw, wanted, &cuts)
}
fn ordinal_positions_chunks(
    raw: &[u8],
    wanted: &HashSet<i64>,
    cuts: &[usize],
) -> Result<HashMap<i64, (u64, u64)>> {
    let parts: Vec<_> = cuts
        .par_windows(2)
        .map(|span| ordinal_positions_range(&raw[span[0]..span[1]], wanted, span[0]))
        .collect::<Result<Vec<_>>>()?;
    let mut found = HashMap::new();
    for part in parts {
        for (n, position) in part {
            if found.insert(n, position).is_some_and(|old| old != position) {
                return Err("Ambiguous rollout ordinal".into());
            }
        }
    }
    if wanted.iter().any(|n| !found.contains_key(n)) {
        return Err("Missing rollout ordinal; cannot repair index".into());
    }
    Ok(found)
}
fn ordinal_positions_range(
    raw: &[u8],
    wanted: &HashSet<i64>,
    offset: usize,
) -> Result<HashMap<i64, (u64, u64)>> {
    let mut found = HashMap::new();
    for cap in ordinal_regex().captures_iter(raw) {
        let n = std::str::from_utf8(&cap[1])?.parse::<i64>()?;
        if !wanted.contains(&n) {
            continue;
        }
        let cap = cap.get(0).unwrap();
        let start = memrchr(b'\n', &raw[..cap.start()]).map_or(0, |i| i + 1);
        let end =
            cap.end() + memchr(b'\n', &raw[cap.end()..]).ok_or("Incomplete ordinal record")? + 1;
        let r: Value = serde_json::from_slice(&raw[start..end])?;
        if r["ordinal"].as_i64() != Some(n) {
            continue;
        }
        if found
            .insert(n, ((start + offset) as u64, (end + offset) as u64))
            .is_some_and(|v| v != ((start + offset) as u64, (end + offset) as u64))
        {
            return Err("Ambiguous rollout ordinal".into());
        }
    }
    Ok(found)
}
fn mapping(path: &str) -> Result<memmap2::Mmap> {
    let f = File::open(path)?;
    Ok(unsafe { memmap2::MmapOptions::new().map(&f)? })
}
pub fn plan_files(
    infos: &[&Header],
    selected: &HashSet<String>,
    writable: &HashSet<String>,
    provider: &str,
) -> Result<Vec<PlanFile>> {
    let mut files: Vec<_> = infos
        .par_iter()
        .map(|h| {
            let scan = engine::scan(
                &app::path_string(&h.path),
                selected.contains(&h.owner).then_some(provider),
            )?;
            if !h.stamp.same(&app::stamp(&h.path)?) {
                return Err("Rollout changed during scan".into());
            }
            let mut p = PlanFile {
                path: app::path_string(&h.path),
                physical: h.physical.clone(),
                stamp: h.stamp.clone(),
                patches: scan.patches,
                bases: scan.bases,
                has_ordinals: h.record.get("ordinal").is_some(),
                inplace: false,
                temp: String::new(),
                original: String::new(),
                prepared_mtime: None,
                ends: vec![],
                shifts: vec![],
            };
            p.map_init()?;
            Ok(p)
        })
        .collect::<Result<_>>()?;
    let by_id: HashMap<_, _> = files
        .iter()
        .enumerate()
        .map(|(i, f)| (f.physical.clone(), i))
        .collect();
    let mut dependencies = vec![0; files.len()];
    let mut children = vec![vec![]; files.len()];
    for (i, f) in files.iter().enumerate() {
        let parents: BTreeSet<_> = f.bases.iter().map(|b| b.physical.clone()).collect();
        dependencies[i] = parents.len();
        for p in parents {
            let &j = by_id.get(&p).ok_or("Missing referenced history")?;
            children[j].push(i);
        }
    }
    let mut ready: VecDeque<_> = dependencies
        .iter()
        .enumerate()
        .filter(|(_, n)| **n == 0)
        .map(|(i, _)| i)
        .collect();
    let mut count = 0;
    // Cache each parent's map/ordinal repairs across children, rather than reopening it per edge.
    let mut maps = HashMap::new();
    let mut repaired: HashMap<(usize, i64), u64> = HashMap::new();
    while let Some(i) = ready.pop_front() {
        for base in files[i].bases.clone() {
            let j = by_id[&base.physical];
            if let std::collections::hash_map::Entry::Vacant(e) = maps.entry(j) {
                e.insert(mapping(&files[j].path)?);
            }
            let raw = &maps[&j];
            let expected = files[j].has_ordinals.then_some(base.ordinal - 1);
            let mut offset = base.offset;
            if !matches(raw, offset as i64, expected, false) {
                let n = expected.ok_or("Invalid history_base without ordinal evidence")?;
                offset = if let Some(v) = repaired.get(&(j, n)) {
                    *v
                } else {
                    let v = ordinal_positions(raw, &HashSet::from([n]))?[&n].1;
                    repaired.insert((j, n), v);
                    v
                };
            }
            let new = files[j].map(offset)?;
            if new != base.offset {
                if !writable.contains(&files[i].physical) {
                    return Err(
                        "Referenced ancestor needs history repair; select it explicitly".into(),
                    );
                }
                let mut p = base.patch;
                p.new = new.to_string().into_bytes();
                files[i].patches.push(p);
            }
        }
        files[i].map_init()?;
        count += 1;
        for &child in &children[i] {
            dependencies[child] -= 1;
            if dependencies[child] == 0 {
                ready.push_back(child);
            }
        }
    }
    if count != files.len() {
        return Err("Cyclic history_base".into());
    }
    Ok(files)
}
fn spec(table: &str) -> Option<&'static [&'static str]> {
    match table {
        "thread_turns" => Some(&["rollout_byte_offset", "rollout_end_byte_offset"]),
        "thread_history_projection_state" => Some(&["next_rollout_byte_offset"]),
        _ => None,
    }
}
pub fn index_updates(database: &Path, files: &[PlanFile]) -> Result<Vec<Index>> {
    let db = app::read_db(&database.join("thread_history_1.sqlite"))?;
    db.execute_batch("BEGIN")?;
    let tables = app::tables(&db)?;
    let mut schema = HashMap::new();
    for t in &tables {
        let c = app::columns(&db, t)?;
        if c.iter()
            .any(|v| v.contains("byte_offset") && !spec(t).unwrap_or(&[]).contains(&v.as_str()))
        {
            return Err(format!("Unknown history offset schema: {t}").into());
        }
        schema.insert(t.clone(), c);
    }
    struct Pending {
        index: Index,
        ordinals: Vec<Option<i64>>,
    }
    let mut pending: HashMap<String, Vec<Pending>> = HashMap::new();
    let selected: HashMap<_, _> = files
        .iter()
        .map(|f| (f.physical.as_str(), f.has_ordinals))
        .collect();
    // One bound JSON array avoids SQLite's parameter limit and lets its existing
    // thread_id indexes skip unrelated histories without changing the database.
    let selected_ids = serde_json::to_string(&selected.keys().collect::<Vec<_>>())?;
    for t in ["thread_turns", "thread_history_projection_state"] {
        let available = schema.get(t).ok_or("Missing history index table")?;
        let cols = spec(t).unwrap();
        let ords: Vec<&str> = if t == "thread_turns" {
            vec!["rollout_ordinal", "rollout_end_ordinal"]
        } else {
            vec!["next_rollout_ordinal"]
        };
        let have = ords.iter().all(|o| available.contains(*o));
        let mut names = cols.to_vec();
        if have {
            names.extend(&ords);
        }
        let mut q = db.prepare(&format!(
            "SELECT thread_id,rowid,{} FROM {t} WHERE thread_id IN (SELECT value FROM json_each(?1))",
            names.join(",")
        ))?;
        let mut rows = q.query([&selected_ids])?;
        while let Some(r) = rows.next()? {
            let id: String = r.get(0)?;
            let Some(&has_ordinals) = selected.get(id.as_str()) else {
                continue;
            };
            let old: Vec<Option<i64>> = (0..cols.len())
                .map(|i| r.get(i + 2))
                .collect::<std::result::Result<_, _>>()?;
            let ordinals: Vec<Option<i64>> = if have && has_ordinals {
                (0..ords.len())
                    .map(|i| r.get(i + 2 + cols.len()))
                    .collect::<std::result::Result<_, _>>()?
            } else {
                vec![None; cols.len()]
            };
            pending.entry(id).or_default().push(Pending {
                index: Index {
                    table: t.to_owned(),
                    columns: cols.iter().map(|s| (*s).to_owned()).collect(),
                    rowid: r.get(1)?,
                    new: old.clone(),
                    old,
                    repaired: false,
                },
                ordinals,
            });
        }
    }
    let mut jobs = vec![];
    for f in files {
        if let Some(p) = pending.remove(&f.physical) {
            jobs.push((f, p));
        }
    }
    let result: Vec<Vec<Index>> = jobs
        .into_par_iter()
        .map(|(f, mut list)| {
            let raw = mapping(&f.path)?;
            let mut needed = HashSet::new();
            let mut repairs = vec![];
            for (j, p) in list.iter_mut().enumerate() {
                for (i, &v) in p.index.old.iter().enumerate() {
                    let Some(v) = v else {
                        continue;
                    };
                    let start = p.index.columns[i] == "rollout_byte_offset";
                    let n = p.ordinals[i].map(|n| {
                        if p.index.columns[i] == "next_rollout_byte_offset" {
                            n - 1
                        } else {
                            n
                        }
                    });
                    if matches(&raw, v, n, start) {
                        p.index.new[i] = Some(f.map(v as u64)? as i64);
                    } else {
                        let n = n.ok_or("Invalid index without ordinal evidence")?;
                        needed.insert(n);
                        repairs.push((j, i, n, start));
                        p.index.repaired = true;
                    }
                }
            }
            let resolved = if needed.is_empty() {
                HashMap::new()
            } else {
                ordinal_positions(&raw, &needed)?
            };
            for (j, i, n, start) in repairs {
                let (a, b) = resolved[&n];
                list[j].index.new[i] = Some(f.map(if start { a } else { b })? as i64);
            }
            Ok(list
                .into_iter()
                .map(|p| p.index)
                .filter(|u| u.old != u.new)
                .collect())
        })
        .collect::<Result<_>>()?;
    Ok(result.into_iter().flatten().collect())
}
fn check_rows(db: &Connection, rows: &[Row], provider: Option<&str>) -> Result<()> {
    let mut q = db.prepare_cached(
        "SELECT model_provider,rollout_path,archived,history_mode FROM threads WHERE id=?",
    )?;
    for r in rows {
        let current: (Option<String>, String, i64, Option<String>) = q.query_row([&r.id], |v| {
            Ok((v.get(0)?, v.get(1)?, v.get(2)?, v.get(3)?))
        })?;
        if current.1 != r.rollout_path
            || current.2 != r.archived
            || current.3 != r.history_mode
            || current.0 != r.model_provider
                && !(provider.is_some() && current.0.as_deref() == provider)
        {
            return Err("Task changed outside migration".into());
        }
    }
    Ok(())
}
fn apply_rows(db: &Connection, rows: &[Row], provider: Option<&str>) -> Result<()> {
    let mut q = db.prepare_cached("UPDATE threads SET model_provider=? WHERE id=?")?;
    for r in rows {
        q.execute(params![provider.or(r.model_provider.as_deref()), r.id])?;
    }
    Ok(())
}
fn check_index(u: &Index) -> Result<()> {
    if spec(&u.table).is_none_or(|s| {
        s.len() != u.columns.len() || !s.iter().zip(&u.columns).all(|(a, b)| a == b)
    }) || u.old.len() != u.columns.len()
        || u.new.len() != u.columns.len()
    {
        return Err("Unknown journal index schema".into());
    }
    Ok(())
}
fn apply_indices(db: &Connection, indices: &[Index], restore: bool) -> Result<()> {
    for u in indices {
        check_index(u)?;
    }
    for table in ["thread_turns", "thread_history_projection_state"] {
        if !indices.iter().any(|u| u.table == table) {
            continue;
        }
        let cols = spec(table).unwrap();
        let set = cols
            .iter()
            .map(|c| format!("{c}=?"))
            .collect::<Vec<_>>()
            .join(",");
        let same = cols
            .iter()
            .map(|c| format!("{c} IS ?"))
            .collect::<Vec<_>>()
            .join(" AND ");
        // Validate and update atomically, including NULLs, with one cached
        // statement per table rather than SELECT + UPDATE for every index row.
        let mut q = db.prepare_cached(&format!(
            "UPDATE {table} SET {set} WHERE rowid=? AND (({same}) OR ({same}))"
        ))?;
        for u in indices.iter().filter(|u| u.table == table) {
            let values = if restore { &u.old } else { &u.new };
            let params = values
                .iter()
                .copied()
                .chain(std::iter::once(Some(u.rowid)))
                .chain(u.old.iter().copied())
                .chain(u.new.iter().copied());
            if q.execute(params_from_iter(params))? != 1 {
                return Err("History index changed outside migration".into());
            }
        }
    }
    Ok(())
}

fn journal_write(home: &Path, j: &Journal) -> Result<()> {
    app::atomic_write(&home.join(app::JOURNAL), &serde_json::to_vec(j)?)
}
fn config_write(settings: &Settings, old: &[u8], new: &[u8]) -> Result<()> {
    app::ensure_closed()?;
    let path = settings.home.join("config.toml");
    if fs::read(&path)? != old {
        return Err("Configuration changed outside migration".into());
    }
    if old != new {
        app::atomic_write(&path, new)?;
    }
    Ok(())
}
fn inplace(file: &PlanFile, restore: bool) -> Result<()> {
    let mut f = OpenOptions::new().read(true).write(true).open(&file.path)?;
    if f.metadata()?.len() != file.stamp.size {
        return Err("Rollout file size changed".into());
    }
    for p in &file.patches {
        if p.new.len() != p.old.len() {
            return Err("Invalid in-place journal patch".into());
        }
        f.seek(SeekFrom::Start(p.start))?;
        let mut actual = vec![0; p.old.len()];
        f.read_exact(&mut actual)?;
        if actual != p.old && (!restore || actual != p.new) {
            return Err("Rollout field changed outside migration".into());
        }
    }
    for p in &file.patches {
        f.seek(SeekFrom::Start(p.start))?;
        f.write_all(if restore { &p.old } else { &p.new })?;
    }
    f.sync_all()?;
    drop(f);
    app::restore_times(Path::new(&file.path), &file.stamp)
}
fn cleanup_committed(home: &Path, files: &[PlanFile]) -> Result<()> {
    app::ensure_closed()?;
    for f in files {
        app::remove(Path::new(&f.temp))?;
        app::remove(Path::new(&f.original))?;
    }
    app::remove(&home.join(app::JOURNAL))
}
pub fn recover(settings: &Settings) -> Result<bool> {
    let path = settings.home.join(app::JOURNAL);
    if !path.exists() {
        return Ok(false);
    }
    app::ensure_closed()?;
    let mut value: Value = serde_json::from_slice(&fs::read(&path)?)?;
    if value["version"] == 2 {
        value["files"] = json!([]);
        value["indices"] = json!([]);
    }
    let mut j: Journal = serde_json::from_value(value)?;
    if ![2, 3, 4].contains(&j.version)
        || app::norm(Path::new(&j.database_dir))? != settings.database
    {
        return Err("Recovery journal version/database mismatch".into());
    }
    let roots = app::roots(&settings.home)?;
    for f in &mut j.files {
        let p = Path::new(&f.path);
        app::checked(p.parent().ok_or("Invalid journal path")?, &roots)?;
        if f.temp != format!("{}.provider-new", f.path)
            || f.original != format!("{}.provider-original", f.path)
        {
            return Err("Invalid journal staging path".into());
        }
        f.map_init()?;
    }
    for u in &j.indices {
        check_index(u)?;
    }
    if j.committed {
        cleanup_committed(&settings.home, &j.files)?;
        return Ok(true);
    }
    let config = fs::read(settings.home.join("config.toml"))?;
    if config != j.config_old && config != j.config_new {
        return Err("Configuration changed outside recovery".into());
    }
    let mut state = app::write_db(&settings.database.join("state_5.sqlite"))?;
    let mut history = app::write_db(&settings.database.join("thread_history_1.sqlite"))?;
    let st = state.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let ht = history.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    check_rows(&st, &j.rows, Some(&j.provider))?;
    apply_indices(&ht, &j.indices, true)?;
    // Validate every rollback file before restoring any file.
    for f in &j.files {
        let p = Path::new(&f.path);
        if f.inplace {
            let raw = mapping(&f.path)?;
            if raw.len() as u64 != f.stamp.size {
                return Err("Recovery file size changed".into());
            }
            for patch in &f.patches {
                let actual = &raw[patch.start as usize..patch.end as usize];
                if actual != patch.old && actual != patch.new {
                    return Err("Recovery field changed outside migration".into());
                }
            }
        } else if Path::new(&f.original).exists() {
            if !f.stamp.same(&app::stamp(Path::new(&f.original))?) {
                return Err("Original rollout changed outside migration".into());
            }
            if p.exists() {
                let s = app::stamp(p)?;
                if s.size != f.map(f.stamp.size)?
                    || s.mtime != f.stamp.mtime && Some(s.mtime) != f.prepared_mtime
                {
                    return Err("Replacement rollout changed outside migration".into());
                }
            }
        } else if !f.stamp.same(&app::stamp(p)?) {
            return Err("Unmodified rollout changed outside migration".into());
        }
    }
    for f in &j.files {
        let p = Path::new(&f.path);
        if f.inplace {
            inplace(f, true)?;
        } else if Path::new(&f.original).exists() {
            app::replace(Path::new(&f.original), p)?;
        }
        app::remove(Path::new(&f.temp))?;
        app::restore_times(p, &f.stamp)?;
    }
    apply_rows(&st, &j.rows, None)?;
    config_write(settings, &config, &j.config_old)?;
    ht.commit()?;
    st.commit()?;
    app::remove(&path)?;
    Ok(true)
}
pub struct Plan {
    pub files: Vec<PlanFile>,
    pub indices: Vec<Index>,
    pub rows: Vec<Row>,
    pub groups: usize,
    pub config_new: Vec<u8>,
}
pub fn plan(
    settings: &Settings,
    data: &Catalog,
    ids: &HashSet<String>,
    provider: &str,
) -> Result<Plan> {
    let groups: Vec<_> = data
        .groups
        .iter()
        .filter(|g| g.ids.iter().any(|id| ids.contains(id)))
        .collect();
    let rows: Vec<_> = groups.iter().flat_map(|g| g.rows.clone()).collect();
    if ids.is_empty() || !ids.iter().all(|id| rows.iter().any(|r| &r.id == id)) {
        return Err("Selected conversations no longer exist".into());
    }
    for g in &groups {
        if !g.issues.is_empty() {
            return Err(g.issues.join("; ").into());
        }
    }
    if rows
        .iter()
        .any(|r| r.history_mode.as_deref() != Some("paginated"))
    {
        return Err("Unsupported history mode; no writes made".into());
    }
    let config_new = settings.new_config(provider)?;
    let selected: HashSet<_> = groups.iter().flat_map(|g| g.ids.iter().cloned()).collect();
    let mut writable: HashSet<usize> = groups
        .iter()
        .flat_map(|g| g.files.iter().copied())
        .collect();
    let by_id: HashMap<_, _> = data
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| (f.physical.as_str(), i))
        .collect();
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, f) in data.files.iter().enumerate() {
        if let Some(&p) = f.record["payload"]["history_base"]["thread_id"]
            .as_str()
            .and_then(|id| by_id.get(id))
        {
            children.entry(p).or_default().push(i);
        }
    }
    // Retag only selected owners. Other branches may need cursor updates when
    // a referenced file changes length; ancestors remain read-only context.
    let mut stack: Vec<_> = writable.iter().copied().collect();
    while let Some(i) = stack.pop() {
        if let Some(next) = children.get(&i) {
            for &child in next {
                if writable.insert(child) {
                    stack.push(child);
                }
            }
        }
    }
    let mut scope = writable.clone();
    let mut stack: Vec<_> = scope.iter().copied().collect();
    while let Some(i) = stack.pop() {
        if let Some(base) = data.files[i].record["payload"]
            .get("history_base")
            .filter(|b| !b.is_null())
        {
            let parent = *base["thread_id"]
                .as_str()
                .and_then(|id| by_id.get(id))
                .ok_or("Missing referenced history")?;
            if scope.insert(parent) {
                stack.push(parent);
            }
        }
    }
    for group in &data.groups {
        if group.files.iter().any(|i| scope.contains(i)) {
            if !group.issues.is_empty() {
                return Err(group.issues.join("; ").into());
            }
            if group
                .rows
                .iter()
                .any(|r| r.history_mode.as_deref() != Some("paginated"))
            {
                return Err("Unsupported referenced history mode; no writes made".into());
            }
        }
    }
    let infos: Vec<_> = data
        .files
        .iter()
        .enumerate()
        .filter(|(i, _)| scope.contains(i))
        .map(|(_, f)| f)
        .collect();
    let writable: HashSet<_> = writable
        .iter()
        .map(|&i| data.files[i].physical.clone())
        .collect();
    let files = plan_files(&infos, &selected, &writable, provider)?;
    let indexed: Vec<_> = files
        .iter()
        .filter(|f| writable.contains(&f.physical))
        .cloned()
        .collect();
    let indices = index_updates(&settings.database, &indexed)?;
    Ok(Plan {
        files,
        indices,
        rows,
        groups: groups.len(),
        config_new,
    })
}
pub fn migrate(
    settings: &Settings,
    ids: &HashSet<String>,
    all: bool,
    provider: &str,
    report: &(dyn Fn(String) + Sync),
) -> Result<Value> {
    let start = Instant::now();
    let _lock = app::lock(&settings.home)?;
    app::ensure_closed()?;
    recover(settings)?;
    // Reload config after recovery; never apply a stale settings snapshot.
    let pending = settings.new_provider.clone();
    let update_default = settings.update_default;
    let mut settings = Settings::load(&settings.home)?;
    settings.new_provider = pending;
    settings.update_default = update_default;
    report("Reading conversations and subagents".into());
    let data = app::catalog(&settings)?;
    let all_ids: HashSet<_> = if all {
        data.groups
            .iter()
            .filter(|g| !g.archived)
            .map(|g| g.root.clone())
            .collect()
    } else {
        HashSet::new()
    };
    let ids = if all { &all_ids } else { ids };
    report("Scanning provider records and reconciling history indices".into());
    let p = plan(&settings, &data, ids, provider)?;
    let changed = p
        .rows
        .iter()
        .filter(|r| r.model_provider.as_deref() != Some(provider))
        .count();
    let mut files: Vec<_> = p
        .files
        .iter()
        .filter(|f| !f.patches.is_empty())
        .cloned()
        .collect();
    for f in &mut files {
        f.inplace = f.stamp.links == 1 && f.patches.iter().all(|p| p.old.len() == p.new.len());
        f.temp = format!("{}.provider-new", f.path);
        f.original = format!("{}.provider-original", f.path);
        if Path::new(&f.temp).exists() || Path::new(&f.original).exists() {
            return Err("Unrecognized staging files; recovery required".into());
        }
    }
    let rewritten: u64 = files
        .iter()
        .filter(|f| !f.inplace)
        .map(|f| f.stamp.size)
        .sum();
    let repaired = p.indices.iter().filter(|u| u.repaired).count();
    let mut needed: BTreeMap<PathBuf, u64> = BTreeMap::new();
    let mut volumes: HashMap<&Path, PathBuf> = HashMap::new();
    for f in &files {
        if !f.inplace {
            let directory = Path::new(&f.path)
                .parent()
                .ok_or("Missing history directory")?;
            let root = match volumes.entry(directory) {
                std::collections::hash_map::Entry::Occupied(v) => v.get().clone(),
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(crate::platform::volume(directory)?).clone()
                }
            };
            *needed.entry(root).or_default() += f.map(f.stamp.size)?;
        }
    }
    for (volume, bytes) in needed {
        if app::available_space(&volume)? < bytes + 16 * 1024 * 1024 {
            return Err("Insufficient free space for transaction".into());
        }
    }
    let count = files.len();
    let tasks = p.rows.len();
    let groups = p.groups;
    if count == 0 && changed == 0 && p.indices.is_empty() && p.config_new == settings.raw {
        return Ok(
            json!({"groups":groups,"tasks":tasks,"changed":0,"files":0,"seconds":start.elapsed().as_secs_f64()}),
        );
    }
    let mut j = Journal {
        version: 4,
        database_dir: app::path_string(&settings.database),
        provider: provider.into(),
        rows: p.rows,
        files,
        indices: p.indices,
        config_old: settings.raw.clone(),
        config_new: p.config_new,
        committed: false,
    };
    app::ensure_closed()?;
    journal_write(&settings.home, &j)?;
    let result = (|| -> Result<()> {
        report(format!(
            "Preparing {} files; {:.1} MiB to rewrite; {} index repairs",
            count,
            rewritten as f64 / 1048576.,
            repaired
        ));
        j.files
            .par_iter_mut()
            .filter(|f| !f.inplace)
            .try_for_each(|f| {
                // Parallelize within a dominant file; balanced batches already
                // have enough independent file jobs to occupy the worker pool.
                engine::copy(
                    &f.path,
                    &f.temp,
                    &f.patches,
                    rewritten.div_ceil(2).max(64 * 1024 * 1024),
                )?;
                f.prepared_mtime = Some(app::stamp(Path::new(&f.temp))?.mtime);
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
            })?;
        journal_write(&settings.home, &j)?;
        let mut state = app::write_db(&settings.database.join("state_5.sqlite"))?;
        let mut history = app::write_db(&settings.database.join("thread_history_1.sqlite"))?;
        let st = state.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let ht = history.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        check_rows(&st, &j.rows, None)?;
        app::ensure_closed()?;
        for f in &p.files {
            if !f.stamp.same(&app::stamp(Path::new(&f.path))?) {
                return Err("History changed during preparation".into());
            }
        }
        report("Committing files, indices and configuration".into());
        // Independent file commits can overlap their fsync waits. Journal covers every file.
        j.files.par_iter().try_for_each(|f| {
            let path = Path::new(&f.path);
            if f.inplace {
                inplace(f, false)?;
            } else {
                app::replace(path, Path::new(&f.original))?;
                app::replace(Path::new(&f.temp), path)?;
                app::restore_times(path, &f.stamp)?;
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        })?;
        apply_indices(&ht, &j.indices, false)?;
        apply_rows(&st, &j.rows, Some(provider))?;
        config_write(&settings, &j.config_old, &j.config_new)?;
        ht.commit()?;
        st.commit()?;
        j.committed = true;
        journal_write(&settings.home, &j)?;
        // The just-written journal is already in memory and validated. Recovery
        // still reads and validates disk journals after an interrupted process.
        cleanup_committed(&settings.home, &j.files)?;
        Ok(())
    })();
    if let Err(e) = result {
        if let Err(recovery) = recover(&settings) {
            return Err(format!(
                "{e}; recovery stopped: {recovery}. Keep Codex closed and preserve the journal."
            )
            .into());
        }
        return Err(e);
    }
    Ok(
        json!({"groups":groups,"tasks":tasks,"changed":changed,"files":count,"rewritten_bytes":rewritten,"repaired_indices":repaired,"seconds":start.elapsed().as_secs_f64()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinal_partition_boundaries_and_ambiguity() {
        let raw =
            b"{\"ordinal\":0}\r\n{\"ordinal\":1,\"payload\":{\"ordinal\":1}}\n{\"ordinal\":2}\n";
        let wanted = HashSet::from([0, 1, 2]);
        let expected = ordinal_positions(raw, &wanted).unwrap();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        for block in 1..=raw.len() {
            let cuts = engine::record_cuts(raw, block).unwrap();
            assert_eq!(
                pool.install(|| ordinal_positions_chunks(raw, &wanted, &cuts))
                    .unwrap(),
                expected
            );
        }
        let duplicate = b"{\"ordinal\":0}\n{\"ordinal\":0}\n";
        assert!(
            ordinal_positions_chunks(
                duplicate,
                &HashSet::from([0]),
                &engine::record_cuts(duplicate, 1).unwrap()
            )
            .is_err()
        );
        assert!(ordinal_positions(raw, &HashSet::from([999])).is_err());
    }
    #[test]
    fn index_cas_preserves_nulls_and_rejects_external_changes() {
        let db = Connection::open_in_memory().unwrap();
        apply_indices(&db, &[], false).unwrap();
        db.execute_batch("CREATE TABLE thread_turns(rollout_byte_offset INTEGER,rollout_end_byte_offset INTEGER); INSERT INTO thread_turns VALUES(1,NULL)").unwrap();
        let mut change = Index {
            table: "thread_turns".into(),
            columns: spec("thread_turns")
                .unwrap()
                .iter()
                .map(|s| (*s).into())
                .collect(),
            rowid: 1,
            old: vec![Some(1), None],
            new: vec![Some(2), Some(3)],
            repaired: false,
        };
        for _ in 0..2 {
            apply_indices(&db, std::slice::from_ref(&change), false).unwrap();
        }
        let read = || {
            db.query_row(
                "SELECT rollout_byte_offset,rollout_end_byte_offset FROM thread_turns",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .unwrap()
        };
        assert_eq!(read(), (2, Some(3)));
        apply_indices(&db, std::slice::from_ref(&change), true).unwrap();
        assert_eq!(read(), (1, None));
        db.execute("UPDATE thread_turns SET rollout_byte_offset=9", [])
            .unwrap();
        assert!(apply_indices(&db, std::slice::from_ref(&change), false).is_err());
        assert_eq!(read(), (9, None));
        change.rowid = 99;
        assert!(apply_indices(&db, std::slice::from_ref(&change), true).is_err());
    }
}
