//! Port of FXServer's manifest globbing (`MatchFiles` / `GlobValueInternal`).
//!
//! The semantics are odd in places and are kept on purpose, because the
//! client file list (and with it `resource.rpf` and its `.db`) only matches
//! FXServer's if the same files come out:
//! - each path segment is an anchored, case-sensitive regex where `?` is `.`
//!   and `*` is `.*`
//! - `**` only counts at the start of a segment, and is expanded as
//!   `a/*/b` (which matches nothing, see below), `a/b`, and recursion into
//!   every sub-directory
//! - a `*` followed later by `/` in a non-recursive pattern drops the
//!   character before the slash, so `dir/*/x.lua` matches nothing and
//!   `dir/cl*/x.lua` only matches a directory named exactly `cl`

use crate::normalize::path_normalize;
use regex::Regex;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

thread_local! {
    static RE_CACHE: RefCell<HashMap<String, Regex>> = RefCell::new(HashMap::new());
}

fn segment_regex(seg: &str) -> Regex {
    RE_CACHE.with(|c| {
        if let Some(r) = c.borrow().get(seg) {
            return r.clone();
        }
        let mut pat = String::with_capacity(seg.len() * 2 + 2);
        pat.push('^');
        for ch in seg.chars() {
            match ch {
                '?' => pat.push('.'),
                '*' => pat.push_str(".*"),
                // std::regex leaves { } unescaped; Rust's regex would reject a
                // bare brace, so escape them (only differs for `{n}` patterns)
                '\\' | '^' | '.' | '$' | '|' | '(' | ')' | '[' | ']' | '+' | '/' | '{' | '}' => {
                    pat.push('\\');
                    pat.push(ch);
                }
                c => pat.push(c),
            }
        }
        pat.push('$');
        let re = Regex::new(&pat).unwrap_or_else(|_| Regex::new("^\\b$").unwrap());
        c.borrow_mut().insert(seg.to_string(), re.clone());
        re
    })
}

fn get_directory(s: &str) -> &str {
    match s.rfind('/') {
        Some(i) => &s[..i],
        None => ".",
    }
}

/// List entries of the directory part of `pattern` whose names match its
/// last segment. Yields `(name, is_dir)`.
fn find_matches(pattern: &str) -> Vec<(String, bool)> {
    let (root, after) = match pattern.rfind('/') {
        Some(i) => (format!("{}/", &pattern[..i]), &pattern[i + 1..]),
        None => (format!("{pattern}/"), pattern),
    };
    let re = segment_regex(after);
    let Ok(rd) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if !re.is_match(&name) {
            continue;
        }
        let is_dir = match ent.file_type() {
            Ok(t) if t.is_symlink() => std::fs::metadata(ent.path()).map(|m| m.is_dir()).unwrap_or(false),
            Ok(t) => t.is_dir(),
            Err(_) => false,
        };
        out.push((name, is_dir));
    }
    out
}

fn match_files(pattern: &str, depth: u32, emit: &mut dyn FnMut(String)) {
    if depth > 64 {
        return;
    }
    let pn = path_normalize(pattern);
    let b = pn.as_bytes();
    let star = pn.find('*');
    let before = get_directory(match star {
        Some(s) => &pn[..s],
        None => &pn,
    })
    .to_string();
    let slash = star.and_then(|s| pn[s..].find('/').map(|i| i + s));
    let after = slash.map(|s| &pn[s + 1..]).unwrap_or("");
    let recurse = star.is_some_and(|s| b.get(s + 1) == Some(&b'*') && (s == 0 || b[s - 1] == b'/'));

    if let (true, Some(s)) = (recurse, star) {
        for off in [1usize, 2] {
            let sub = format!("{}{}", &pn[..s], &pn[(s + off).min(pn.len())..]);
            match_files(&sub, depth + 1, emit);
        }
        for (name, is_dir) in find_matches(&pn[..s + 1]) {
            if is_dir {
                let p = format!("{before}/{name}/{}", &pn[s..]);
                match_files(&p, depth + 1, emit);
            }
        }
    } else {
        let find_pattern = match slash {
            Some(sp) => &pn[..sp.saturating_sub(1)],
            None => &pn[..],
        };
        for (name, is_dir) in find_matches(find_pattern) {
            let is_file = !is_dir;
            if slash.is_some() && is_file {
                continue;
            }
            let mp = format!("{before}/{name}");
            if !after.is_empty() {
                match_files(&format!("{mp}/{after}"), depth + 1, emit);
            } else if is_file {
                emit(mp);
            }
        }
    }
}

/// Expand one manifest value relative to `resource_path` (FXServer's
/// `GetPath()`, no trailing slash). Results are byte-sorted, de-duplicated,
/// and relative to the resource root. `@other/file` values pass through.
pub fn glob_value(resource_path: &str, value: &str) -> Vec<String> {
    if value.is_empty() {
        return Vec::new();
    }
    if value.starts_with('@') {
        return vec![value.to_string()];
    }
    let root_path = format!("{resource_path}/");
    let rel_root = path_normalize(&root_path);
    let mut set = BTreeSet::new();
    match_files(&format!("{root_path}{value}"), 0, &mut |f| {
        if f.len() >= rel_root.len() + 1 {
            if let Some(rel) = f.get(rel_root.len() + 1..) {
                set.insert(rel.to_string());
            }
        }
    });
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        for f in [
            "client/a.lua",
            "client/b.lua",
            "client/sub/c.lua",
            "client/sub/deep/d.lua",
            "shared.lua",
            "html/index.html",
            "html/js/app.js",
            "cl/x.lua",
            "client/x.lua",
        ] {
            let p = r.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, b"x").unwrap();
        }
        d
    }

    fn root(d: &tempfile::TempDir) -> String {
        d.path().to_string_lossy().replace('\\', "/")
    }

    #[test]
    fn plain_and_star() {
        let d = tree();
        let r = root(&d);
        assert_eq!(glob_value(&r, "shared.lua"), ["shared.lua"]);
        assert_eq!(glob_value(&r, "client/*.lua"), ["client/a.lua", "client/b.lua", "client/x.lua"]);
        assert!(glob_value(&r, "Shared.lua").is_empty(), "case-sensitive");
        assert!(glob_value(&r, "missing.lua").is_empty());
        assert_eq!(glob_value(&r, "@ox_lib/init.lua"), ["@ox_lib/init.lua"]);
    }

    #[test]
    fn double_star() {
        let d = tree();
        let r = root(&d);
        assert_eq!(
            glob_value(&r, "client/**/*.lua"),
            ["client/a.lua", "client/b.lua", "client/sub/c.lua", "client/sub/deep/d.lua", "client/x.lua"]
        );
        // trailing `**` expands through submask `html/*`, so it lists files
        assert_eq!(glob_value(&r, "html/**"), ["html/index.html", "html/js/app.js"]);
        let mut all = glob_value(&r, "html/**/*");
        all.sort();
        assert_eq!(all, ["html/index.html", "html/js/app.js"]);
    }

    #[test]
    fn fxserver_quirk_star_dir() {
        let d = tree();
        let r = root(&d);
        // `*` directly before a slash drops the char before the slash
        assert!(glob_value(&r, "*/x.lua").is_empty());
        // `cl*/x.lua` only matches a directory named exactly `cl`
        assert_eq!(glob_value(&r, "cl*/x.lua"), ["cl/x.lua"]);
    }
}
