//! Resource dependency graph checks.
//!
//! FXServer reads `dependency` and `dependencie` entries (`dependencies { }`
//! strips one `s`) and starts dependencies first. Entries starting with `/` are
//! constraints (`/server:7290`, `/onesync`, `/gameBuild:...`), not resources.
//! `@other/file` script entries load a file from another resource.

use crate::Resource;
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub enum Issue {
    MissingDependency { resource: String, dependency: String },
    DependencyHasNoManifest { resource: String, dependency: String },
    Cycle { path: Vec<String> },
    MissingCrossFile { resource: String, key: String, reference: String },
    CrossFileResourceMissing { resource: String, key: String, reference: String },
}

impl Issue {
    pub fn is_error(&self) -> bool {
        true
    }
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Issue::MissingDependency { resource, dependency } => write!(f, "{resource}: depends on `{dependency}`, which does not exist"),
            Issue::DependencyHasNoManifest { resource, dependency } => write!(f, "{resource}: depends on `{dependency}`, which has no valid manifest"),
            Issue::Cycle { path } => write!(f, "dependency cycle: {}", path.join(" -> ")),
            Issue::MissingCrossFile { resource, key, reference } => write!(f, "{resource}: {key} `{reference}` points at a file that does not exist"),
            Issue::CrossFileResourceMissing { resource, key, reference } => write!(f, "{resource}: {key} `{reference}` points at a resource that does not exist"),
        }
    }
}

pub struct Graph<'a> {
    pub by_name: HashMap<&'a str, &'a Resource>,
    pub edges: BTreeMap<&'a str, Vec<String>>,
}

/// Dependencies of a resource, constraints removed.
pub fn dependencies(r: &Resource) -> Vec<String> {
    let Ok(m) = &r.manifest else { return Vec::new() };
    // `dependencies { }` records `dependencie` (one `s` stripped); FXServer
    // reads both keys (ResourceDependencyLoader.cpp).
    m.get("dependency")
        .chain(m.get("dependencie"))
        .filter(|d| !d.starts_with('/') && !d.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn graph(all: &[Resource]) -> Graph<'_> {
    let by_name: HashMap<&str, &Resource> = all.iter().map(|r| (r.name.as_str(), r)).collect();
    let edges = all.iter().map(|r| (r.name.as_str(), dependencies(r))).collect();
    Graph { by_name, edges }
}

/// Check `selected` (and everything they pull in) against the whole scan.
pub fn check(all: &[Resource], selected: &[&Resource]) -> Vec<Issue> {
    let g = graph(all);
    let mut issues = BTreeSet::new();

    // closure over dependencies
    let mut todo: Vec<&str> = selected.iter().map(|r| r.name.as_str()).collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    while let Some(n) = todo.pop() {
        if !seen.insert(n) {
            continue;
        }
        for d in g.edges.get(n).into_iter().flatten() {
            match g.by_name.get(d.as_str()) {
                None => {
                    issues.insert(Issue::MissingDependency { resource: n.into(), dependency: d.clone() });
                }
                Some(dr) if dr.manifest.is_err() => {
                    issues.insert(Issue::DependencyHasNoManifest { resource: n.into(), dependency: d.clone() });
                }
                Some(dr) => todo.push(dr.name.as_str()),
            }
        }
    }

    // cycles (DFS colouring over the reached set)
    let mut colour: HashMap<&str, u8> = HashMap::new();
    let mut stack: Vec<&str> = Vec::new();
    fn dfs<'a>(n: &'a str, g: &Graph<'a>, colour: &mut HashMap<&'a str, u8>, stack: &mut Vec<&'a str>, issues: &mut BTreeSet<Issue>) {
        colour.insert(n, 1);
        stack.push(n);
        for d in g.edges.get(n).into_iter().flatten() {
            let Some(dr) = g.by_name.get(d.as_str()) else { continue };
            let dn = dr.name.as_str();
            match colour.get(dn).copied().unwrap_or(0) {
                0 => dfs(dn, g, colour, stack, issues),
                1 => {
                    let start = stack.iter().position(|x| *x == dn).unwrap_or(0);
                    let mut path: Vec<String> = stack[start..].iter().map(|s| s.to_string()).collect();
                    path.push(dn.to_string());
                    // canonical rotation so the same cycle reports once
                    let body = &path[..path.len() - 1];
                    let min = body.iter().enumerate().min_by_key(|(_, s)| s.as_str()).map(|(i, _)| i).unwrap_or(0);
                    let mut canon: Vec<String> = body[min..].iter().chain(body[..min].iter()).cloned().collect();
                    canon.push(canon[0].clone());
                    issues.insert(Issue::Cycle { path: canon });
                }
                _ => {}
            }
        }
        stack.pop();
        colour.insert(n, 2);
    }
    for n in &seen {
        if colour.get(n).copied().unwrap_or(0) == 0 {
            dfs(n, &g, &mut colour, &mut stack, &mut issues);
        }
    }

    // @other/file references in script and file entries
    for n in &seen {
        let Some(r) = g.by_name.get(n) else { continue };
        let Ok(m) = &r.manifest else { continue };
        for key in ["client_script", "server_script", "shared_script", "file"] {
            for v in m.get(key).filter(|v| v.starts_with('@')) {
                let rest = &v[1..];
                let Some((other, file)) = rest.split_once('/') else { continue };
                match g.by_name.get(other) {
                    None => {
                        issues.insert(Issue::CrossFileResourceMissing { resource: n.to_string(), key: key.into(), reference: v.into() });
                    }
                    Some(o) => {
                        let found = if file.contains('*') { !fxs_manifest::glob_value(&o.fx_path, file).is_empty() } else { o.dir.join(file).is_file() };
                        if !found {
                            issues.insert(Issue::MissingCrossFile { resource: n.to_string(), key: key.into(), reference: v.into() });
                        }
                    }
                }
            }
        }
    }
    issues.into_iter().collect()
}

/// Start order FXServer would use: dependencies before dependents, then the
/// order the resources were requested in.
pub fn start_order(all: &[Resource], requested: &[&str]) -> Vec<String> {
    let g = graph(all);
    let mut out = Vec::new();
    let mut done: BTreeSet<String> = BTreeSet::new();
    fn visit(n: &str, g: &Graph<'_>, done: &mut BTreeSet<String>, onstack: &mut BTreeSet<String>, out: &mut Vec<String>) {
        if done.contains(n) || !onstack.insert(n.to_string()) {
            return;
        }
        for d in g.edges.get(n).into_iter().flatten() {
            if g.by_name.contains_key(d.as_str()) {
                visit(d, g, done, onstack, out);
            }
        }
        onstack.remove(n);
        if done.insert(n.to_string()) {
            out.push(n.to_string());
        }
    }
    for n in requested {
        visit(n, &g, &mut done, &mut BTreeSet::new(), &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn res(root: &std::path::Path, name: &str, manifest: &str) {
        let d = root.join("resources").join(name);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("fxmanifest.lua"), manifest).unwrap();
    }

    #[test]
    fn finds_problems() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        res(r, "lib", "fx_version 'cerulean'\ngame 'gta5'\n");
        fs::write(r.join("resources/lib/init.lua"), "").unwrap();
        res(r, "a", "fx_version 'cerulean'\ngame 'gta5'\ndependencies { '/server:7290', 'lib', 'ghost' }\nshared_scripts { '@lib/init.lua', '@lib/nope.lua', '@missing/x.lua' }\n");
        res(r, "b", "fx_version 'cerulean'\ngame 'gta5'\ndependency 'c'\n");
        res(r, "c", "fx_version 'cerulean'\ngame 'gta5'\ndependency 'b'\n");
        let rep = crate::scan(r, &crate::ScanOptions::for_root(r));
        let sel: Vec<&Resource> = rep.resources.iter().filter(|x| x.name == "a" || x.name == "b").collect();
        let issues: Vec<String> = check(&rep.resources, &sel).iter().map(|i| i.to_string()).collect();
        assert!(issues.iter().any(|i| i.contains("`ghost`, which does not exist")), "{issues:?}");
        assert!(issues.iter().any(|i| i.contains("@lib/nope.lua")), "{issues:?}");
        assert!(issues.iter().any(|i| i.contains("@missing/x.lua")), "{issues:?}");
        assert!(issues.iter().any(|i| i.contains("cycle: b -> c -> b")), "{issues:?}");
        assert!(!issues.iter().any(|i| i.contains("/server")), "constraints are not resources");
        assert!(!issues.iter().any(|i| i.contains("@lib/init.lua")));
        assert_eq!(start_order(&rep.resources, &["a"]), ["lib", "a"]);
    }
}
