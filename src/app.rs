use crate::engine::Result;
use crate::platform::{self, stamp_file};
pub use crate::platform::{available_space, replace};
use rayon::prelude::*;
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const JOURNAL: &str = ".provider-switch.pending.json";
pub fn norm(path: &Path) -> Result<PathBuf> {
    Ok(fs::canonicalize(path)?)
}
pub fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
pub fn read_db(path: &Path) -> Result<Connection> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(Duration::from_secs(5))?;
    Ok(db)
}
pub fn write_db(path: &Path) -> Result<Connection> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(Duration::from_secs(5))?;
    platform::configure_db(&db)?;
    Ok(db)
}
pub fn sql_json(v: ValueRef<'_>) -> Result<Value> {
    Ok(match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(n) => json!(n),
        ValueRef::Real(n) => json!(n),
        ValueRef::Text(s) => json!(std::str::from_utf8(s)?),
        ValueRef::Blob(_) => return Err("Unexpected binary database metadata".into()),
    })
}
pub fn columns(db: &Connection, table: &str) -> Result<BTreeSet<String>> {
    let mut q = db.prepare(&format!(
        "PRAGMA table_info(\"{}\")",
        table.replace('"', "\"\"")
    ))?;
    Ok(q.query_map([], |r| r.get(1))?
        .collect::<std::result::Result<_, _>>()?)
}
pub fn tables(db: &Connection) -> Result<BTreeSet<String>> {
    let mut q = db.prepare("SELECT name FROM sqlite_master WHERE type='table'")?;
    Ok(q.query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?)
}
#[derive(Clone, Default)]
pub struct NewProvider {
    pub id: String,
    pub base_url: String,
    pub env_key: String,
    pub api_key: String,
}
pub fn validate_provider_id(provider: &str) -> Result<()> {
    if provider.is_empty()
        || provider.len() > 128
        || !provider
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_alphanumeric() || i > 0 && b"._-".contains(&b))
    {
        return Err(
            "Use 1-128 letters/digits, dots, underscores or hyphens; start with a letter or digit."
                .into(),
        );
    }
    Ok(())
}
impl NewProvider {
    pub fn validate(&self) -> Result<()> {
        validate_provider_id(&self.id)?;
        if [
            "openai",
            "ollama",
            "lmstudio",
            "amazon-bedrock",
            "amazon-bedrock-runtime",
        ]
        .contains(&self.id.as_str())
        {
            return Err("This provider name is reserved by Codex; choose another name.".into());
        }
        if self
            .base_url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err("URL cannot contain spaces or control characters.".into());
        }
        let url = url::Url::parse(&self.base_url)
            .map_err(|_| "Enter a complete URL, such as http://127.0.0.1:3000/v1")?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err("URL must use http:// or https:// and include a host.".into());
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("Use a base URL without credentials, query parameters or a fragment. Enter the key in the API key field.".into());
        }
        if self.base_url.ends_with("/responses") || self.base_url.ends_with("/chat/completions") {
            return Err("Enter the API base URL (usually ending in /v1), not a /responses or /chat/completions endpoint.".into());
        }
        if !self.env_key.is_empty()
            && !self
                .env_key
                .bytes()
                .enumerate()
                .all(|(i, b)| b.is_ascii_alphabetic() || b == b'_' || i > 0 && b.is_ascii_digit())
        {
            return Err("Enter an environment variable NAME, such as MY_API_KEY. Use letters, digits and underscores; do not paste the key itself.".into());
        }
        if !self.api_key.is_empty() {
            if !self.env_key.is_empty() {
                return Err("Choose either an API key or an environment variable.".into());
            }
            if !self.api_key.bytes().all(|b| b.is_ascii_graphic()) {
                return Err("API key must contain printable ASCII characters without spaces or control characters.".into());
            }
        }
        Ok(())
    }
    fn config(&self) -> Value {
        let mut value = json!({"name":self.id,"base_url":self.base_url,"wire_api":"responses"});
        if !self.env_key.is_empty() {
            value["env_key"] = json!(self.env_key);
        }
        if !self.api_key.is_empty() {
            value["experimental_bearer_token"] = json!(self.api_key);
        }
        value
    }
}
pub struct Settings {
    pub home: PathBuf,
    pub database: PathBuf,
    pub providers: Vec<String>,
    pub raw: Vec<u8>,
    pub new_provider: Option<NewProvider>,
    pub update_default: bool,
}
impl Settings {
    pub fn load(home: &Path) -> Result<Self> {
        let home = norm(home)?;
        let raw = fs::read(home.join("config.toml"))?;
        let config: Value = toml_edit::de::from_str(std::str::from_utf8(&raw)?)?;
        let database = config["sqlite_home"]
            .as_str()
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("CODEX_SQLITE_HOME")
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| home.clone());
        if !database.is_absolute() {
            return Err("sqlite_home must be an absolute path".into());
        }
        let mut providers: BTreeSet<String> = config["model_providers"]
            .as_object()
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default();
        providers.insert("openai".into());
        let mut providers: Vec<_> = providers.into_iter().collect();
        providers.sort_by_key(|p| (p != "openai", p.clone()));
        Ok(Self {
            home,
            database: norm(&database)?,
            providers,
            raw,
            new_provider: None,
            update_default: true,
        })
    }
    pub fn default_provider(&self) -> Result<String> {
        let config: Value = toml_edit::de::from_str(std::str::from_utf8(&self.raw)?)?;
        Ok(config["model_provider"]
            .as_str()
            .unwrap_or("openai")
            .to_owned())
    }
    pub fn save_configuration(&self, provider: &str) -> Result<Self> {
        let _lock = lock(&self.home)?;
        if self.home.join(JOURNAL).exists() {
            return Err(
                "Recover the interrupted conversation switch before editing providers.".into(),
            );
        }
        let mut current = Self::load(&self.home)?;
        current.new_provider = self.new_provider.clone();
        current.update_default = self.update_default;
        let updated = current.new_config(provider)?;
        let path = current.home.join("config.toml");
        if fs::read(&path)? != current.raw {
            return Err(
                "Configuration changed while saving. Retry to use the latest version.".into(),
            );
        }
        if updated != current.raw {
            atomic_write(&path, &updated)?;
        }
        Self::load(&self.home)
    }
    pub fn validate_provider(&self, provider: &str) -> Result<()> {
        validate_provider_id(provider)?;
        if !self.providers.iter().any(|p| p == provider) {
            return Err(format!("Provider '{provider}' is not configured. Add [model_providers.{provider}] with its connection settings to config.toml, then return here. No tasks have been changed.").into());
        }
        Ok(())
    }
    pub fn new_config(&self, provider: &str) -> Result<Vec<u8>> {
        let addition = self.new_provider.as_ref().filter(|p| p.id == provider);
        if let Some(p) = addition {
            p.validate()?;
            if self.providers.iter().any(|id| id == provider) {
                return Err("This provider name was configured meanwhile. Return to provider selection and choose the saved entry; it will not be overwritten.".into());
            }
        } else {
            self.validate_provider(provider)?;
        }
        let raw = std::str::from_utf8(&self.raw)?;
        let mut expected: Value = toml_edit::de::from_str(raw)?;
        if (!self.update_default || expected["model_provider"] == provider) && addition.is_none() {
            return Ok(self.raw.clone());
        }
        if self.update_default {
            expected["model_provider"] = json!(provider);
        }
        let doc = toml_edit::Document::parse(raw)?;
        let replacement = serde_json::to_string(provider)?;
        let mut new = if !self.update_default {
            raw.to_owned()
        } else if let Some(old) = doc.get("model_provider") {
            let span = old
                .as_value()
                .and_then(|v| v.span())
                .ok_or("Missing TOML provider span")?;
            format!("{}{}{}", &raw[..span.start], replacement, &raw[span.end..])
        } else {
            let newline = if raw.contains("\r\n") { "\r\n" } else { "\n" };
            format!("model_provider = {replacement}{newline}{raw}")
        };
        if let Some(p) = addition {
            if expected.get("model_providers").is_none() {
                expected["model_providers"] = json!({});
            }
            expected["model_providers"][provider] = p.config();
            // Append a serialized table; preserve every original config byte otherwise.
            let newline = if raw.contains("\r\n") { "\r\n" } else { "\n" };
            let appended = format!(
                "[model_providers.{}]\n{}",
                serde_json::to_string(provider)?,
                toml_edit::ser::to_string(&p.config())?
            )
            .replace('\n', newline);
            new.push_str(newline);
            new.push_str(&appended);
        }
        if toml_edit::de::from_str::<Value>(&new)? != expected {
            return Err("Configuration semantics changed unexpectedly".into());
        }
        Ok(new.into_bytes())
    }
}

pub fn ensure_closed() -> Result<()> {
    let pids = crate::platform::codex_pids()?;
    if !pids.is_empty() {
        return Err(
            format!("Close Codex Desktop and CLI before writing (running PIDs: {pids:?})").into(),
        );
    }
    Ok(())
}
pub fn lock(home: &Path) -> Result<File> {
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(home.join(".provider-switch.lock"))?;
    f.try_lock()
        .map_err(|e| format!("Another switcher is writing: {e}"))?;
    Ok(f)
}
pub fn atomic_write(path: &Path, raw: &[u8]) -> Result<()> {
    let temp = path.with_extension("provider-tmp");
    let mut f = platform::private_file(
        OpenOptions::new().create(true).truncate(true).write(true),
        &temp,
    )?;
    f.write_all(raw)?;
    platform::inherit_permissions(path, &f)?;
    f.sync_all()?;
    drop(f);
    replace(&temp, path)
}
pub fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => platform::sync_parent(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
pub fn roots(home: &Path) -> Result<Vec<PathBuf>> {
    ["sessions", "archived_sessions"]
        .iter()
        .filter(|n| home.join(n).exists())
        .map(|n| norm(&home.join(n)))
        .collect()
}
pub fn checked(path: &Path, roots: &[PathBuf]) -> Result<PathBuf> {
    let path = norm(path)?;
    if !roots.iter().any(|r| path.starts_with(r)) {
        return Err("History path is outside Codex history directories".into());
    }
    Ok(path)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stamp {
    pub size: u64,
    pub mtime: i64,
    pub atime: i64,
    pub links: u64,
}
pub fn stamp(path: &Path) -> Result<Stamp> {
    stamp_file(&File::open(path)?)
}
impl Stamp {
    pub fn same(&self, other: &Self) -> bool {
        self.size == other.size && self.mtime == other.mtime && self.links == other.links
    }
}
pub fn restore_times(path: &Path, s: &Stamp) -> Result<()> {
    let time = |n: i64| {
        filetime::FileTime::from_unix_time(
            n.div_euclid(1_000_000_000),
            n.rem_euclid(1_000_000_000) as u32,
        )
    };
    filetime::set_file_times(path, time(s.atime), time(s.mtime))?;
    Ok(())
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Row {
    pub id: String,
    pub model_provider: Option<String>,
    pub rollout_path: String,
    pub archived: i64,
    pub history_mode: Option<String>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub updated: i64,
    #[serde(default)]
    pub subagent: bool,
}
#[derive(Clone, Debug)]
pub struct Header {
    pub path: PathBuf,
    pub owner: String,
    pub physical: String,
    pub record: Value,
    pub stamp: Stamp,
}
#[derive(Clone, Debug)]
pub struct Group {
    pub root: String,
    pub forked_from: Option<String>,
    pub ids: BTreeSet<String>,
    pub rows: Vec<Row>,
    pub files: Vec<usize>,
    pub archived: bool,
    pub updated: i64,
    pub issues: Vec<String>,
}
pub struct Catalog {
    pub groups: Vec<Group>,
    pub files: Vec<Header>,
    pub seconds: f64,
}
fn parent(source: &Value) -> Option<String> {
    let parsed = source
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok());
    let v = parsed.as_ref().unwrap_or(source);
    v.pointer("/subagent/thread_spawn/parent_thread_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
fn walk(root: &Path, paths: &mut BTreeSet<PathBuf>) -> Result<()> {
    for e in fs::read_dir(root)? {
        let e = e?;
        let t = e.file_type()?;
        if t.is_symlink() {
            return Err("Linked history directory/file requires inspection".into());
        }
        if t.is_dir() {
            walk(&e.path(), paths)?;
        } else if e.path().extension().is_some_and(|s| s == "jsonl") {
            paths.insert(e.path());
        } else if e.file_name().to_string_lossy().ends_with(".jsonl.zst") {
            return Err("Compressed histories are not supported".into());
        }
    }
    Ok(())
}
fn header(path: &Path) -> Result<Header> {
    let file = File::open(path)?;
    let before = stamp_file(&file)?;
    let mut raw = Vec::new();
    std::io::Read::take(BufReader::new(file), 16 * 1024 * 1024).read_until(b'\n', &mut raw)?;
    if raw.last() != Some(&b'\n') {
        return Err("Incomplete or oversized history header".into());
    }
    let record: Value = serde_json::from_slice(&raw)?;
    if record["type"] != "session_meta" {
        return Err("Missing session_meta".into());
    }
    let owner = record["payload"]["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("Missing history owner")?
        .to_owned();
    if !before.same(&stamp(path)?) {
        return Err("History changed while reading; refresh after closing Codex".into());
    }
    let name = path.file_stem().unwrap_or_default().to_string_lossy();
    let physical = if name.len() >= 36 && name.is_char_boundary(name.len() - 36) {
        let tail = &name[name.len() - 36..];
        if tail.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        }) {
            tail.to_owned()
        } else {
            owner.clone()
        }
    } else {
        owner.clone()
    };
    Ok(Header {
        path: path.to_owned(),
        owner,
        physical,
        record,
        stamp: before,
    })
}
pub fn catalog(settings: &Settings) -> Result<Catalog> {
    let start = Instant::now();
    let db = read_db(&settings.database.join("state_5.sqlite"))?;
    db.execute_batch("BEGIN")?;
    let available = columns(&db, "threads")?;
    for c in [
        "id",
        "model_provider",
        "rollout_path",
        "archived",
        "history_mode",
    ] {
        if !available.contains(c) {
            return Err(format!("Missing threads column {c}").into());
        }
    }
    let wanted: Vec<_> = [
        "id",
        "name",
        "title",
        "cwd",
        "model_provider",
        "updated_at",
        "rollout_path",
        "archived",
        "history_mode",
        "source",
        "thread_source",
    ]
    .into_iter()
    .filter(|c| available.contains(*c))
    .collect();
    let mut q = db.prepare(&format!("SELECT {} FROM threads", wanted.join(",")))?;
    let mut cursor = q.query([])?;
    let mut rows = BTreeMap::new();
    let mut sources = HashMap::new();
    while let Some(r) = cursor.next()? {
        let mut v = serde_json::Map::new();
        for (i, c) in wanted.iter().enumerate() {
            v.insert((*c).to_owned(), sql_json(r.get_ref(i)?)?);
        }
        let v = Value::Object(v);
        let id = v["id"].as_str().ok_or("Invalid task ID")?.to_owned();
        sources.insert(id.clone(), parent(&v["source"]));
        rows.insert(
            id.clone(),
            Row {
                id,
                model_provider: v["model_provider"].as_str().map(str::to_owned),
                rollout_path: v["rollout_path"]
                    .as_str()
                    .ok_or("Missing rollout path")?
                    .to_owned(),
                archived: v["archived"].as_i64().ok_or("Invalid archive flag")?,
                history_mode: v["history_mode"].as_str().map(str::to_owned),
                label: v["name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .or_else(|| v["title"].as_str().filter(|s| !s.is_empty()))
                    .unwrap_or("(untitled)")
                    .to_owned(),
                cwd: v["cwd"].as_str().unwrap_or("").to_owned(),
                updated: v["updated_at"].as_i64().unwrap_or(0),
                subagent: v["thread_source"] == "subagent" || parent(&v["source"]).is_some(),
            },
        );
    }
    let mut edges: Vec<(String, String)> = vec![];
    if tables(&db)?.contains("thread_spawn_edges") {
        let mut q =
            db.prepare("SELECT parent_thread_id,child_thread_id FROM thread_spawn_edges")?;
        edges = q
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
    }
    drop(cursor);
    drop(q);
    drop(db);
    let roots = roots(&settings.home)?;
    let mut paths = BTreeSet::new();
    for root in &roots {
        walk(root, &mut paths)?;
    }
    // Files are still canonicalized and link-checked; independent filesystem
    // lookups run on the same bounded worker pool as header reads.
    let mut paths: BTreeSet<PathBuf> = paths.par_iter().map(|p| norm(p)).collect::<Result<_>>()?;
    let mut by_name: HashMap<std::ffi::OsString, Vec<PathBuf>> = HashMap::new();
    for p in &paths {
        by_name
            .entry(p.file_name().unwrap().to_owned())
            .or_default()
            .push(p.clone());
    }
    let mut issues: HashMap<String, Vec<String>> = HashMap::new();
    let mut row_paths = HashMap::new();
    let resolved: Vec<_> = rows
        .values()
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|row| (row, checked(Path::new(&row.rollout_path), &roots)))
        .collect();
    for (row, result) in resolved {
        match result {
            Ok(p) => {
                paths.insert(p.clone());
                row_paths.insert(row.id.clone(), p);
            }
            Err(e) => {
                let matches = Path::new(&row.rollout_path)
                    .file_name()
                    .and_then(|n| by_name.get(n));
                if let Some(v) = matches.filter(|v| v.len() == 1) {
                    row_paths.insert(row.id.clone(), v[0].clone());
                } else {
                    issues
                        .entry(row.id.clone())
                        .or_default()
                        .push(e.to_string());
                }
            }
        }
    }
    let paths: Vec<_> = paths.into_iter().collect();
    let scanned: Vec<_> = paths.par_iter().map(|p| (p, header(p))).collect();
    let mut files = vec![];
    let mut physical = HashMap::new();
    let mut global = vec![];
    for (p, result) in scanned {
        match result {
            Ok(f) => {
                if physical.insert(f.physical.clone(), files.len()).is_some() {
                    global.push("Duplicate physical history ID".into());
                }
                files.push(f);
            }
            Err(e) => {
                let owners: Vec<_> = row_paths
                    .iter()
                    .filter(|(_, v)| *v == p)
                    .map(|(id, _)| id.clone())
                    .collect();
                if owners.is_empty() {
                    global.push(format!(
                        "Unowned unreadable history: {}",
                        p.file_name().unwrap_or_default().to_string_lossy()
                    ));
                } else {
                    for id in owners {
                        issues.entry(id).or_default().push(e.to_string());
                    }
                }
            }
        }
    }
    let mut graph: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut parents: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut link = |p: Option<String>, c: &str| {
        if let Some(p) = p.filter(|p| !p.is_empty() && p != c) {
            graph.entry(p.clone()).or_default().insert(c.to_owned());
            graph.entry(c.to_owned()).or_default().insert(p.clone());
            parents.entry(c.to_owned()).or_default().insert(p);
        }
    };
    for (p, c) in edges {
        link(Some(p), &c);
    }
    for (id, p) in sources {
        link(p, &id);
    }
    for f in &files {
        let m = &f.record["payload"];
        link(parent(&m["source"]), &f.owner);
        if rows.get(&f.owner).is_some_and(|r| r.subagent) {
            link(m["parent_thread_id"].as_str().map(str::to_owned), &f.owner);
        }
        if let Some(base) = m.get("history_base").filter(|v| !v.is_null())
            && !base["thread_id"]
                .as_str()
                .is_some_and(|id| physical.contains_key(id))
        {
            issues
                .entry(f.owner.clone())
                .or_default()
                .push("Missing base history".into());
        }
    }
    let by_path: HashMap<_, _> = files.iter().map(|f| (&f.path, f)).collect();
    // Some service agents store only a lineage/session link. Use it only when
    // no explicit spawn parent exists; ordinary forks remain independent.
    for row in rows.values().filter(|r| r.subagent) {
        if parents.contains_key(&row.id) {
            continue;
        }
        if let Some(f) = row_paths.get(&row.id).and_then(|p| by_path.get(p)) {
            let m = &f.record["payload"];
            let p = m["forked_from_id"]
                .as_str()
                .filter(|p| *p != row.id)
                .or_else(|| {
                    m["history_base"]["thread_id"]
                        .as_str()
                        .and_then(|id| physical.get(id))
                        .map(|&i| files[i].owner.as_str())
                        .filter(|p| *p != row.id)
                })
                .or_else(|| m["session_id"].as_str().filter(|p| *p != row.id))
                .filter(|p| !p.is_empty());
            if let Some(p) = p {
                graph
                    .entry(p.to_owned())
                    .or_default()
                    .insert(row.id.clone());
                graph
                    .entry(row.id.clone())
                    .or_default()
                    .insert(p.to_owned());
                parents
                    .entry(row.id.clone())
                    .or_default()
                    .insert(p.to_owned());
            }
        }
    }
    for row in rows.values() {
        if let Some(f) = row_paths.get(&row.id).and_then(|p| by_path.get(p))
            && f.owner != row.id
        {
            issues
                .entry(row.id.clone())
                .or_default()
                .push("Database/history owner mismatch".into());
        }
        if issues.contains_key(&row.id) && row.subagent && !parents.contains_key(&row.id) {
            global.push("Damaged child history without a parent".into());
        }
    }
    let mut owner_files: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        owner_files.entry(&f.owner).or_default().push(i);
    }
    let mut seen = HashSet::new();
    let mut groups = vec![];
    for id in rows.keys() {
        if seen.contains(id) {
            continue;
        }
        let mut stack = vec![id.clone()];
        let mut family = BTreeSet::new();
        while let Some(n) = stack.pop() {
            if family.insert(n.clone())
                && let Some(next) = graph.get(&n)
            {
                stack.extend(next.iter().filter(|v| !family.contains(*v)).cloned());
            }
        }
        seen.extend(family.iter().cloned());
        let members: Vec<_> = family
            .iter()
            .filter_map(|id| rows.get(id).cloned())
            .collect();
        let root = members
            .iter()
            .min_by_key(|r| (r.subagent, parents.contains_key(&r.id), r.updated, &r.id))
            .unwrap()
            .id
            .clone();
        let root_row = rows.get(&root).unwrap();
        let forked_from = row_paths
            .get(&root)
            .and_then(|p| by_path.get(p))
            .and_then(|f| f.record["payload"]["forked_from_id"].as_str())
            .filter(|id| !id.is_empty() && *id != root)
            .map(str::to_owned);
        let mut problems = global.clone();
        for n in &family {
            if let Some(e) = issues.get(n) {
                problems.extend(e.clone());
            }
        }
        if members.iter().all(|r| r.subagent) {
            problems.push("No root task found for this family".into());
        }
        if members.iter().filter(|r| !r.subagent).count() > 1 {
            problems.push(
                "Conflicting subagent parents; inspect the conversation links before switching"
                    .into(),
            );
        }
        problems.sort();
        problems.dedup();
        let family_files = family
            .iter()
            .filter_map(|id| owner_files.get(id.as_str()))
            .flatten()
            .copied()
            .collect();
        groups.push(Group {
            root,
            forked_from,
            archived: root_row.archived != 0,
            updated: members.iter().map(|r| r.updated).max().unwrap_or(0),
            ids: family,
            rows: members,
            files: family_files,
            issues: problems,
        });
    }
    groups.sort_by(|a, b| b.updated.cmp(&a.updated).then(a.root.cmp(&b.root)));
    Ok(Catalog {
        groups,
        files,
        seconds: start.elapsed().as_secs_f64(),
    })
}

#[cfg(test)]
mod provider_tests {
    use super::*;
    #[test]
    fn custom_provider_config_preserves_original_settings() {
        for newline in ["\n", "\r\n"] {
            let raw = "# keep this\nmodel_provider = \"openai\" # keep comment\n[model_providers.existing]\nname = \"Existing\"\nhttp_headers = { test = \"value\" }\n".replace('\n', newline).into_bytes();
            let mut settings = Settings {
                home: PathBuf::new(),
                database: PathBuf::new(),
                providers: vec!["openai".into(), "existing".into()],
                raw: raw.clone(),
                update_default: true,
                new_provider: Some(NewProvider {
                    id: "My.Proxy".into(),
                    base_url: "http://[::1]:3000/v1".into(),
                    env_key: "MY_API_KEY".into(),
                    api_key: String::new(),
                }),
            };
            let updated = settings.new_config("My.Proxy").unwrap();
            let parsed: Value =
                toml_edit::de::from_str(std::str::from_utf8(&updated).unwrap()).unwrap();
            assert_eq!(parsed["model_provider"], "My.Proxy");
            assert_eq!(
                parsed["model_providers"]["My.Proxy"]["env_key"],
                "MY_API_KEY"
            );
            assert_eq!(
                parsed["model_providers"]["My.Proxy"]["wire_api"],
                "responses"
            );
            assert_eq!(
                parsed["model_providers"]["existing"]["http_headers"]["test"],
                "value"
            );
            assert!(
                std::str::from_utf8(&updated)
                    .unwrap()
                    .contains("# keep comment")
            );
            assert_eq!(settings.raw, raw);
            settings.new_provider.as_mut().unwrap().env_key.clear();
            let parsed: Value = toml_edit::de::from_str(
                std::str::from_utf8(&settings.new_config("My.Proxy").unwrap()).unwrap(),
            )
            .unwrap();
            assert!(
                parsed["model_providers"]["My.Proxy"]
                    .get("env_key")
                    .is_none()
            );
            settings.update_default = false;
            settings.new_provider.as_mut().unwrap().api_key = "test-key-\"quoted\"\\escaped".into();
            let saved: Value = toml_edit::de::from_str(
                std::str::from_utf8(&settings.new_config("My.Proxy").unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(saved["model_provider"], "openai");
            assert_eq!(
                saved["model_providers"]["My.Proxy"]["experimental_bearer_token"],
                "test-key-\"quoted\"\\escaped"
            );
            assert!(
                saved["model_providers"]["My.Proxy"]
                    .get("env_key")
                    .is_none()
            );
            settings.providers.push("My.Proxy".into());
            assert!(settings.new_config("My.Proxy").is_err());
            settings.new_provider = None;
            assert_eq!(settings.new_config("openai").unwrap(), raw);
            assert_eq!(settings.new_config("existing").unwrap(), raw);
        }
    }
    #[test]
    fn custom_provider_validation() {
        let mut p = NewProvider {
            id: "mine".into(),
            base_url: "https://example.com/v1".into(),
            env_key: "API_KEY".into(),
            api_key: String::new(),
        };
        assert!(p.validate().is_ok());
        for bad in [
            "localhost:3000",
            "ftp://host/v1",
            "https://user:secret@host/v1",
            "https://host/v1?key=secret",
            "https://host/v1#fragment",
            "https://host:99999/v1",
            "https://host/responses",
            "https://host/chat/completions",
            "https://ho st/v1",
        ] {
            p.base_url = bad.into();
            assert!(p.validate().is_err(), "{bad}");
        }
        p.base_url = "http://localhost:3000/v1".into();
        p.env_key = "not-an-environment-name".into();
        assert!(p.validate().is_err());
        p.env_key.clear();
        assert!(p.validate().is_ok());
        for key in ["bad\r\nkey", "bad key", "bad\0key", "ключ"] {
            p.api_key = key.into();
            assert!(p.validate().is_err());
        }
        p.api_key = "test-api-key".into();
        assert!(p.validate().is_ok());
        p.env_key = "MY_KEY".into();
        assert!(p.validate().is_err());
        p.env_key.clear();
        p.id = "openai".into();
        assert!(p.validate().is_err());
    }
}
