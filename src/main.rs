mod app;
mod engine;
mod migrate;
mod platform;
mod tui;
use engine::Result;
use rayon::prelude::*;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    io::{self, IsTerminal, Write},
    path::PathBuf,
    time::Instant,
};

fn legacy() -> Result<()> {
    let v: Value = serde_json::from_reader(io::stdin().lock())?;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(v["workers"].as_u64().unwrap_or(8).clamp(1, 32) as usize)
        .build()?;
    let result: Vec<Value> = match v["command"].as_str() {
        Some("scan") => {
            let paths = v["paths"].as_array().ok_or("Missing paths")?;
            let provider = v["provider"].as_str().ok_or("Missing provider")?;
            pool.install(|| {
                paths
                    .par_iter()
                    .map(|p| {
                        Ok(serde_json::to_value(engine::scan(
                            p.as_str().ok_or("Invalid path")?,
                            Some(provider),
                        )?)?)
                    })
                    .collect::<Result<_>>()
            })?
        }
        Some("copy") => {
            let files = v["files"].as_array().ok_or("Missing files")?;
            let parallel_threshold = files
                .iter()
                .try_fold(0u64, |sum, f| sum.checked_add(f["size"].as_u64()?))
                .map_or(u64::MAX, |bytes| bytes.div_ceil(2).max(64 * 1024 * 1024));
            pool.install(|| {
                files
                    .par_iter()
                    .map(|f| {
                        let p: Vec<engine::Patch> = serde_json::from_value(f["patches"].clone())?;
                        engine::copy(
                            f["path"].as_str().ok_or("Missing path")?,
                            f["temp"].as_str().ok_or("Missing staging path")?,
                            &p,
                            parallel_threshold,
                        )?;
                        Ok(Value::Null)
                    })
                    .collect::<Result<_>>()
            })?
        }
        _ => return Err("Unknown legacy engine operation".into()),
    };
    serde_json::to_writer(io::stdout().lock(), &result)?;
    Ok(())
}
fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() && !io::stdin().is_terminal() {
        return legacy();
    }
    let mut home = platform::codex_home();
    let mut command = "tui";
    let mut provider = String::new();
    let mut ids = HashSet::new();
    let mut all = false;
    let mut yes = false;
    let mut workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(16);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--home" | "--provider" | "--select" | "--workers" => {
                let flag = &args[i];
                i += 1;
                let v = args.get(i).ok_or("Missing argument value")?;
                match flag.as_str() {
                    "--home" => home = Some(PathBuf::from(v)),
                    "--provider" => provider = v.clone(),
                    "--select" => {
                        ids.insert(v.clone());
                    }
                    _ => {
                        workers = v.parse()?;
                        if !(1..=32).contains(&workers) {
                            return Err("Workers must be 1..32".into());
                        }
                    }
                }
            }
            "--list" => command = "list",
            "--list-json" => command = "list-json",
            "--plan-json" => command = "plan-json",
            "--audit" => command = "audit",
            "--apply" => command = "apply",
            "--recover" => command = "recover",
            "--all" => all = true,
            "--yes" => yes = true,
            "--version" => {
                println!(
                    "Codex Provider Switcher {} (standalone Rust)",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "Codex Provider Switcher — standalone Rust / Windows + macOS\n\nNo arguments: interactive TUI\n--home PATH             Codex home (default CODEX_HOME or ~/.codex)\n--list / --list-json     Read-only conversations and forks\n--plan-json / --audit    Read-only migration plan / aggregate timings\n--apply                 Migrate selected conversations and config.toml\n--recover               Recover/clean up an interrupted transaction\n--provider ID           Registered target provider\n--select ID             Select a conversation or its subagent (repeatable)\n--all                   All nonarchived conversations and their subagents\n--workers 1..32         Parallel workers (default up to 16)\n--yes                   Required for noninteractive writes\n\nClose Codex before writing. No model requests are sent."
                );
                return Ok(());
            }
            v => return Err(format!("Unknown argument: {v}").into()),
        }
        i += 1;
    }
    rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build_global()?;
    let home = home.ok_or("Cannot locate Codex home; use --home PATH")?;
    let settings = app::Settings::load(&home)?;
    if command == "tui" {
        return tui::run(settings);
    }
    if command == "recover" {
        if !yes {
            return Err("Recovery requires --yes".into());
        }
        let _lock = app::lock(&settings.home)?;
        let done = migrate::recover(&settings)?;
        println!("{}", json!({"recovered":done}));
        return Ok(());
    }
    if command == "apply" {
        if !yes {
            return Err("Writing requires --yes".into());
        }
        let result = migrate::migrate(&settings, &ids, all, &provider, &|s| eprintln!("{s}"))?;
        println!("{result}");
        return Ok(());
    }
    let data = app::catalog(&settings)?;
    if command == "list" || command == "list-json" {
        let groups:Vec<_>=data.groups.iter().map(|g|json!({"root":g.root,"forked_from":g.forked_from,"rows":g.rows,"ids":g.ids,"files":g.files.len(),"archived":g.archived,"issues":g.issues})).collect();
        if command == "list-json" {
            println!(
                "{}",
                json!({"groups":groups,"files":data.files.len(),"seconds":data.seconds})
            );
        } else {
            for g in &data.groups {
                if !g.archived {
                    let r = g.rows.iter().find(|r| r.id == g.root).unwrap();
                    println!("{} | {} conversations | {}", r.label, g.rows.len(), g.root);
                }
            }
        }
        return Ok(());
    }
    if all {
        ids.extend(
            data.groups
                .iter()
                .filter(|g| !g.archived)
                .map(|g| g.root.clone()),
        );
    }
    if command == "plan-json" || command == "audit" {
        let start = Instant::now();
        let p = migrate::plan(&settings, &data, &ids, &provider)?;
        if command == "plan-json" {
            println!(
                "{}",
                json!({"groups":p.groups,"rows":p.rows,"files":p.files,"indices":p.indices})
            );
        } else {
            println!(
                "{}",
                json!({"read_only":true,"groups":p.groups,"files":p.files.len(),"bytes":p.files.iter().map(|f|f.stamp.size).sum::<u64>(),"patches":p.files.iter().map(|f|f.patches.len()).sum::<usize>(),"indices":p.indices.len(),"repaired_indices":p.indices.iter().filter(|u|u.repaired).count(),"catalog_seconds":data.seconds,"plan_seconds":start.elapsed().as_secs_f64()})
            );
        }
        return Ok(());
    }
    Err("No operation selected".into())
}
fn main() {
    let interactive = std::env::args().len() == 1 && io::stdin().is_terminal();
    if let Err(e) = run() {
        eprintln!("Error: {e}");
        let _ = io::stderr().flush();
        if interactive {
            eprintln!("Press Enter to close.");
            let _ = io::stdin().read_line(&mut String::new());
        }
        std::process::exit(1);
    }
}
