use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use fxs_assets::hashindex::HashIndex;
use fxs_assets::warm::{Mode, SetState, Warmer};
use fxs_scan::{Resource, ScanOptions};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "fxs", version, about = "Rust tooling that runs alongside stock FXServer")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct RootArgs {
    /// Server data root (the folder holding server.cfg, resources/, cache/)
    root: PathBuf,
    /// FXServer's path prefix for this root, as written in its cache
    /// databases (auto-detected from cache/files when omitted)
    #[arg(long)]
    fx_root: Option<String>,
    /// Only resources started by this cfg (follows exec, ensure, start)
    #[arg(long)]
    cfg: Option<PathBuf>,
    /// Only these resources (comma separated)
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Discover resources and evaluate manifests, report timing
    Scan {
        #[command(flatten)]
        r: RootArgs,
        /// Print name, escrow flag and folder for every selected resource
        #[arg(long)]
        list: bool,
    },
    /// Pre-build FXServer's cache/files so the next start skips hashing and packing
    Warm {
        #[command(flatten)]
        r: RootArgs,
        /// Compare against FXServer's current cache; write nothing
        #[arg(long)]
        compare: bool,
        /// Print every resource, not just the summary
        #[arg(long)]
        verbose: bool,
    },
    /// Serve FXServer's published files for `fileserver_add`
    Serve {
        #[command(flatten)]
        r: RootArgs,
        #[arg(long, default_value = "0.0.0.0:30130")]
        bind: String,
        /// Reject requests that lack the X-CitizenFX-Token header clients send
        #[arg(long)]
        require_token: bool,
    },
    /// Print `fileserver_add` lines that route only non-escrowed resources to
    /// the Rust file server; escrowed (.fxap) resources stay on FXServer
    FileserverCfg {
        #[command(flatten)]
        r: RootArgs,
        /// Public URL of `fxs serve`, as clients will reach it
        url: String,
    },
    /// Read-only breakdown of a running process's committed memory
    Memmap {
        pid: u32,
        #[arg(long, default_value_t = 15)]
        top: usize,
    },
    /// Read-only: summarize a memory block of a running process (zero ratio, top strings)
    Peek {
        pid: u32,
        /// Address, hex with 0x prefix
        addr: String,
        /// Bytes to read
        #[arg(long, default_value_t = 4 << 20)]
        len: usize,
    },
    /// Download every published file from a file server, verify SHA-1, report throughput
    BenchDownload {
        #[command(flatten)]
        r: RootArgs,
        /// Base URL, e.g. http://127.0.0.1:41252/files or http://127.0.0.1:30130
        url: String,
        #[arg(long, default_value_t = 16)]
        concurrency: usize,
    },
}

fn detect_fx_root(root: &Path) -> Option<String> {
    let files = root.join("cache").join("files");
    for ent in std::fs::read_dir(files).ok()?.flatten() {
        for db in ["resource.rpf.db", "resource.sfl.db"] {
            if let Some(d) = fxs_assets::db::FileDb::load(&ent.path().join(db)) {
                if let Some(n) = d.entries.keys().next() {
                    if let Some(i) = n.find("resources/") {
                        return Some(n[..i].to_string());
                    }
                }
            }
        }
    }
    None
}

fn scan_opts(a: &RootArgs) -> ScanOptions {
    let mut o = ScanOptions::for_root(&a.root);
    if let Some(r) = a.fx_root.clone().or_else(|| detect_fx_root(&a.root)) {
        o.fx_root = r;
    }
    o
}

/// Resource names started by a cfg, in order. Follows `exec`, reads
/// `ensure`/`start`, expands `[category]` to every resource under it.
fn cfg_started(root: &Path, cfg: &Path, out: &mut Vec<String>, seen: &mut HashSet<PathBuf>) {
    let path = if cfg.is_absolute() { cfg.to_path_buf() } else { root.join(cfg) };
    if !seen.insert(path.clone()) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    for line in text.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        match (it.next(), it.next()) {
            (Some("exec"), Some(p)) => cfg_started(root, Path::new(p.trim_matches('"')), out, seen),
            (Some("ensure" | "start"), Some(n)) => out.push(n.trim_matches('"').to_string()),
            (Some("stop"), Some(n)) => out.retain(|x| x != n.trim_matches('"')),
            _ => {}
        }
    }
}

fn select<'a>(a: &RootArgs, all: &'a [Resource]) -> Vec<&'a Resource> {
    let mut wanted: Option<HashSet<String>> = None;
    if let Some(cfg) = &a.cfg {
        let mut names = Vec::new();
        cfg_started(&a.root, cfg, &mut names, &mut HashSet::new());
        let mut set = HashSet::new();
        for n in names {
            if n.starts_with('[') {
                for r in all.iter().filter(|r| r.categories.iter().any(|c| *c == n)) {
                    set.insert(r.name.clone());
                }
            } else {
                set.insert(n);
            }
        }
        wanted = Some(set);
    }
    if !a.only.is_empty() {
        wanted = Some(a.only.iter().cloned().collect());
    }
    all.iter().filter(|r| wanted.as_ref().is_none_or(|w| w.contains(&r.name))).collect()
}

fn rss_mb() -> Option<u64> {
    // Peak working set is not portable; report what is cheap and honest.
    None
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Scan { r: a, list } => {
            let opts = scan_opts(&a);
            let rep = fxs_scan::scan(&a.root, &opts);
            let sel = select(&a, &rep.resources);
            let bad = sel.iter().filter(|r| r.manifest.is_err()).count();
            let escrowed = sel.iter().filter(|r| fxs_assets::resource::is_escrowed(&r.dir)).count();
            println!("fx root      {}", opts.fx_root);
            println!("resources    {} found, {} selected, {} without valid manifest, {} escrowed", rep.resources.len(), sel.len(), bad, escrowed);
            println!("duplicates   {}", rep.duplicates.len());
            println!("walk         {:?}", rep.walk_time);
            println!("manifests    {:?} (parallel)", rep.manifest_time);
            if list {
                for x in &sel {
                    println!("{}\t{}\t{}", x.name, if fxs_assets::resource::is_escrowed(&x.dir) { "escrowed" } else { "open" }, x.dir.display());
                }
            }
        }
        Cmd::Warm { r, compare, verbose } => {
            let opts = scan_opts(&r);
            let t0 = Instant::now();
            let rep = fxs_scan::scan(&r.root, &opts);
            let sel = select(&r, &rep.resources);
            let t_scan = t0.elapsed();

            let cache_files = r.root.join("cache").join("files");
            let index_path = r.root.join("cache").join("fxs").join("hash-index.json");
            let index = HashIndex::load(&index_path);
            let seeded = index.seed_from_fxserver_cache(&cache_files);
            let t1 = Instant::now();
            let w = Warmer { cache_files, index: &index, mode: if compare { Mode::Compare } else { Mode::Apply } };
            let results = w.run(&sel);
            let t_warm = t1.elapsed();
            if !compare {
                index.save(&index_path).context("save hash index")?;
            }

            let mut counts: BTreeMap<(&str, String), usize> = BTreeMap::new();
            for res in &results {
                *counts.entry(("stream", format!("{:?}", res.stream))).or_default() += 1;
                *counts.entry(("rpf", format!("{:?}", res.rpf))).or_default() += 1;
                let interesting = matches!(res.stream, SetState::Differs | SetState::Stale) || matches!(res.rpf, SetState::Differs | SetState::Stale) || res.note.is_some();
                if verbose || (interesting && compare) {
                    println!("{:<40} stream={:?}({}) rpf={:?}({}) {}", res.name, res.stream, res.stream_files, res.rpf, res.client_files, res.note.as_deref().unwrap_or(""));
                }
            }
            use std::sync::atomic::Ordering::Relaxed;
            println!("--");
            println!("mode         {}", if compare { "compare (no writes)" } else { "apply" });
            println!("resources    {} selected", sel.len());
            for ((k, s), n) in &counts {
                println!("{k:<6} {s:<10} {n}");
            }
            println!("hash index   {} seeded from FXServer cache, {} reused, {} hashed ({:.1} MiB)", seeded, index.reused_files.load(Relaxed), index.hashed_files.load(Relaxed), index.hashed_bytes.load(Relaxed) as f64 / 1048576.0);
            println!("time         scan {:?}, warm {:?}", t_scan, t_warm);
            let _ = rss_mb();
        }
        Cmd::Serve { r, bind, require_token } => {
            let opts = scan_opts(&r);
            let rep = fxs_scan::scan(&r.root, &opts);
            let escrowed: Vec<String> = rep.resources.iter().filter(|x| fxs_assets::resource::is_escrowed(&x.dir)).map(|x| x.name.clone()).collect();
            let mut fs = fxs_fileserver::FileServer::new(r.root.join("cache").join("files"));
            fs.require_token = require_token;
            let srv = Arc::new(fs);
            println!("refusing {} escrowed resources", escrowed.len());
            srv.set_escrowed(escrowed);
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind(&bind).await?;
                println!("serving {} on http://{bind}", r.root.display());
                axum::serve(listener, fxs_fileserver::router(srv)).await?;
                anyhow::Ok(())
            })?;
        }
        Cmd::FileserverCfg { r, url } => {
            let opts = scan_opts(&r);
            let rep = fxs_scan::scan(&r.root, &opts);
            let sel: Vec<&Resource> = select(&r, &rep.resources).into_iter().filter(|x| x.manifest.is_ok()).collect();
            let (escrowed, open): (Vec<&&Resource>, Vec<&&Resource>) = sel.iter().partition(|x| fxs_assets::resource::is_escrowed(&x.dir));
            println!("## generated by `fxs fileserver-cfg`; exec this after the resource lists.");
            println!("## {} resources served by fxs, {} escrowed resources stay on FXServer.", open.len(), escrowed.len());
            // FXServer matches each pattern with std::regex_match against the
            // resource name; one exact-name pattern per resource keeps escrowed
            // names out by construction.
            for x in &open {
                let mut esc = String::new();
                for c in x.name.chars() {
                    if "\\^$.|?*+()[]{}".contains(c) {
                        esc.push('\\');
                    }
                    esc.push(c);
                }
                println!("fileserver_add \"{esc}\" \"{}\"", url.trim_end_matches('/'));
            }
        }
        Cmd::Memmap { pid, top } => {
            let m = fxs_probe::memmap(pid).map_err(anyhow::Error::msg)?;
            let mb = |b: u64| b as f64 / 1048576.0;
            println!("committed {:.0} MiB: private {:.0}, image {:.0}, mapped {:.0}; reserved-only {:.0} MiB", mb(m.committed_total), mb(m.private_total), mb(m.image_total), mb(m.mapped_total), mb(m.reserved_total));
            println!("\nprivate commit by allocation size:");
            for (n, b, c) in &m.private_buckets {
                println!("  {n:<16} {:>8.1} MiB in {c} allocations", mb(*b));
            }
            println!("\nlargest private allocations:");
            for (base, sz, res) in m.private_allocs.iter().take(top) {
                println!("  0x{base:016x} {:>8.1} MiB committed {:>8.1} MiB reserved ({} bytes)", mb(*sz), mb(*res), sz);
            }
            if let Ok(r) = fxs_probe::residency(pid) {
                println!("\nprivate memory, committed vs resident (physically in RAM):");
                println!("  total            {:>8.1} MiB committed {:>8.1} MiB resident", mb(r.private_committed), mb(r.private_resident));
                println!("  NT heap segments {:>8.1} MiB committed {:>8.1} MiB resident ({} segments)", mb(r.heap_segments_committed), mb(r.heap_segments_resident), r.heap_segments);
                println!("  other            {:>8.1} MiB committed {:>8.1} MiB resident", mb(r.other_committed), mb(r.other_resident));
                println!("  largest:");
                for (b, c, res, heap) in r.allocs.iter().take(top) {
                    println!("    0x{b:016x} {:>7.1} MiB committed {:>7.1} MiB resident{}", mb(*c), mb(*res), if *heap { "  heap segment" } else { "" });
                }
            }
            println!("\nimages (committed, writable):");
            for g in m.images.iter().take(top) {
                println!("  {:<40} {:>7.1} {:>7.1} MiB", g.label, mb(g.committed), mb(g.private_writable));
            }
            println!("\nmapped:");
            for g in m.mapped.iter().take(top) {
                println!("  {:<40} {:>7.1} MiB in {} regions", g.label, mb(g.committed), g.regions);
            }
        }
        Cmd::Peek { pid, addr, len } => {
            let a = u64::from_str_radix(addr.trim_start_matches("0x"), 16).context("address")?;
            let d = fxs_probe::peek(pid, a, len).map_err(anyhow::Error::msg)?;
            let zeros = d.iter().filter(|&&b| b == 0).count();
            println!("read {} bytes, {:.1}% zero", d.len(), zeros as f64 * 100.0 / d.len() as f64);
            println!("first 64: {}", hex::encode(&d[..64.min(d.len())]));
            for (s, n) in fxs_probe::strings_summary(&d, 6, 25) {
                println!("  {n:>6}x {s}");
            }
        }
        Cmd::BenchDownload { r, url, concurrency } => bench(&r, &url, concurrency)?,
    }
    Ok(())
}

fn bench(r: &RootArgs, url: &str, concurrency: usize) -> Result<()> {
    use futures::StreamExt;
    use sha1::Digest;
    let opts = scan_opts(r);
    let rep = fxs_scan::scan(&r.root, &opts);
    let sel = select(r, &rep.resources);
    let cache = r.root.join("cache").join("files");
    let mut jobs: Vec<(String, String, String)> = Vec::new();
    for res in sel {
        if fxs_assets::resource::is_escrowed(&res.dir) {
            continue;
        }
        let dir = cache.join(&res.name);
        if let Ok(p) = std::fs::read(dir.join("resource.rpf")) {
            let h = hex::encode(sha1::Sha1::digest(&p));
            jobs.push((res.name.clone(), "resource.rpf".into(), h));
        }
        if let Ok(sfl) = std::fs::read(dir.join("resource.sfl")) {
            for e in fxs_assets::sfl::decode(&sfl) {
                jobs.push((res.name.clone(), e.name, e.hash));
            }
        }
    }
    if jobs.is_empty() {
        bail!("nothing to download: no cache/files for the selected resources");
    }
    let base = url.trim_end_matches('/').to_string();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let client = reqwest::Client::builder().pool_max_idle_per_host(concurrency).build()?;
        let t0 = Instant::now();
        let total = jobs.len();
        let results: Vec<(bool, u64, String)> = futures::stream::iter(jobs.into_iter().map(|(res, file, hash)| {
            let client = client.clone();
            let u = format!("{base}/{res}/{}", file.replace('+', "%2B").replace(' ', "%20"));
            async move {
                match client.get(&u).send().await {
                    Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                        Ok(b) => {
                            let got = hex::encode(sha1::Sha1::digest(&b));
                            (got == hash, b.len() as u64, if got == hash { String::new() } else { format!("hash mismatch {u}") })
                        }
                        Err(e) => (false, 0, format!("{u}: {e}")),
                    },
                    Ok(resp) => (false, 0, format!("{u}: HTTP {}", resp.status())),
                    Err(e) => (false, 0, format!("{u}: {e}")),
                }
            }
        }))
        .buffer_unordered(concurrency)
        .collect()
        .await;
        let el = t0.elapsed();
        let ok = results.iter().filter(|r| r.0).count();
        let bytes: u64 = results.iter().map(|r| r.1).sum();
        for (_, _, e) in results.iter().filter(|r| !r.0).take(10) {
            println!("FAIL {e}");
        }
        println!("files        {ok}/{total} verified");
        println!("bytes        {:.1} MiB", bytes as f64 / 1048576.0);
        println!("time         {:?}", el);
        println!("throughput   {:.1} MiB/s, {:.0} files/s", bytes as f64 / 1048576.0 / el.as_secs_f64(), total as f64 / el.as_secs_f64());
        anyhow::Ok(())
    })
}
