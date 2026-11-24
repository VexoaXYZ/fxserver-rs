//! Byte-for-byte port of the `path_normalize` routine FXServer uses before
//! globbing (originally from Premake, BSD-3-Clause, Jason Perkins et al.).
//!
//! Quirks are kept on purpose: the path is processed in sub-paths split at
//! whitespace and at `%`, `$(`, `${` and `%{` tokens, and each sub-path is
//! normalized on its own. Glob results only match FXServer if this matches.

#[inline]
fn at(p: &[u8], i: usize) -> u8 {
    p.get(i).copied().unwrap_or(0)
}
#[inline]
fn is_sep(c: u8) -> bool {
    c == b'/' || c == b'\\'
}
#[inline]
fn is_quote(c: u8) -> bool {
    c == b'"' || c == b'\''
}
#[inline]
fn is_alpha(c: u8) -> bool {
    c.is_ascii_alphabetic()
}
#[inline]
fn is_space(c: u8) -> bool {
    (b'\t'..=b'\r').contains(&c) || c == b' '
}
fn win_env_start(p: &[u8], i: usize) -> bool {
    at(p, i) == b'%'
}
fn vs_var_start(p: &[u8], i: usize) -> bool {
    at(p, i) == b'$' && at(p, i + 1) == b'('
}
fn unix_env_start(p: &[u8], i: usize) -> bool {
    at(p, i) == b'$' && at(p, i + 1) == b'{'
}
fn premake_start(p: &[u8], i: usize) -> bool {
    at(p, i) == b'%' && at(p, i + 1) == b'{'
}
fn any_token_start(p: &[u8], i: usize) -> bool {
    win_env_start(p, i) || vs_var_start(p, i) || unix_env_start(p, i) || premake_start(p, i)
}

fn skip_tokens(p: &[u8], mut r: usize) -> usize {
    let mut skipped = 0usize;
    // `do { skipped++ } while (!END(readPtr++))`, bounded by the string end
    let skip_until = |r: &mut usize, end: u8, skipped: &mut usize| loop {
        *skipped += 1;
        let c = at(p, *r);
        *r += 1;
        if c == end || *r > p.len() {
            break;
        }
    };
    loop {
        if premake_start(p, r) {
            skip_until(&mut r, b'}', &mut skipped);
        }
        if win_env_start(p, r) {
            skip_until(&mut r, b'%', &mut skipped);
        }
        if vs_var_start(p, r) {
            skip_until(&mut r, b')', &mut skipped);
        }
        if unix_env_start(p, r) {
            skip_until(&mut r, b'}', &mut skipped);
        }
        if !any_token_start(p, r) || r >= p.len() {
            break;
        }
    }
    skipped
}

fn normalize_substring(p: &[u8], mut s: usize, e: usize, out: &mut Vec<u8>) {
    let is_end = |i: usize| i >= e || at(p, i) == 0;
    let sep_or_end = |i: usize| is_end(i) || is_sep(at(p, i));

    if is_alpha(at(p, s)) && at(p, s + 1) == b':' {
        out.push(p[s]);
        out.push(b':');
        s += 2;
    }
    if is_sep(at(p, s)) {
        s += 1;
        out.push(b'/');
        if is_sep(at(p, s)) {
            s += 1;
            out.push(b'/');
        }
    }
    let root = out.len();
    let mut depth = 0u32;

    while !is_end(s) {
        while is_sep(at(p, s)) || (at(p, s) == b'.' && sep_or_end(s + 1)) {
            s += 1;
        }
        if is_end(s) {
            break;
        }
        if at(p, s) == b'.' && at(p, s + 1) == b'.' && sep_or_end(s + 2) {
            if depth > 0 {
                let mut len = out.len();
                loop {
                    len -= 1;
                    if len == root || is_sep(out[len - 1]) {
                        break;
                    }
                }
                out.truncate(len);
                depth -= 1;
            } else {
                out.extend_from_slice(b"../");
            }
            s += 3;
        } else {
            while !sep_or_end(s) {
                out.push(p[s]);
                s += 1;
            }
            if is_sep(at(p, s)) {
                out.push(b'/');
                s += 1;
                depth += 1;
            }
        }
    }
    while out.len() != root && is_sep(out[out.len() - 1]) {
        out.pop();
    }
}

pub fn path_normalize(path: &str) -> String {
    let p = path.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(p.len());
    let mut r = 0usize;
    while r < p.len() && is_space(p[r]) {
        r += 1;
    }
    let mut end = r;
    while end < p.len() {
        let skipped = skip_tokens(p, r);
        if skipped > 0 {
            if r != 0 && !out.is_empty() && is_sep(at(p, r - 1)) && !is_sep(out[out.len() - 1]) {
                out.push(p[r - 1]);
            }
            for _ in 0..skipped {
                if r < p.len() {
                    out.push(p[r]);
                }
                r += 1;
            }
            end = r;
        }
        while end < p.len() && !is_space(p[end]) && !any_token_start(p, end) {
            end += 1;
        }
        if r != end && r < p.len() && is_quote(p[r]) && is_quote(p[end - 1]) && p[r] == p[end - 1] {
            out.push(p[r]);
            r += 1;
        }
        normalize_substring(p, r, end, &mut out);
        while end < p.len() && is_space(p[end]) {
            out.push(p[end]);
            end += 1;
        }
        r = end;
        if r >= p.len() {
            break;
        }
    }
    while out.last().is_some_and(|c| is_space(*c)) {
        out.pop();
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::path_normalize as n;

    #[test]
    fn basics() {
        assert_eq!(n("Z:/a//b/./c/"), "Z:/a/b/c");
        assert_eq!(n("/srv/res//[cat]/x/"), "/srv/res/[cat]/x");
        assert_eq!(n("a/b/../c"), "a/c");
        assert_eq!(n("../a"), "../a");
        assert_eq!(n("a\\b\\c"), "a/b/c");
        assert_eq!(n("a/b/"), "a/b");
        assert_eq!(n("a//b"), "a/b");
    }

    #[test]
    fn percent_and_spaces() {
        assert_eq!(n("Z:/r/Film%20Maker/x.lua"), "Z:/r/Film%20Maker/x.lua");
        assert_eq!(n("Z:/r/my res//x.lua"), "Z:/r/my res/x.lua");
    }
}
