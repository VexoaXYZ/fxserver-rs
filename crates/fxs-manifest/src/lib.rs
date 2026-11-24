//! Resource manifest evaluation, compatible with FXServer's metadata loader.
//!
//! FXServer evaluates `fxmanifest.lua` (or the legacy `__resource.lua`) in a
//! fresh Lua 5.4 state where every unknown global is a function that records a
//! `(key, value)` pair. We reproduce that DSL exactly, including the plural
//! stripping (`client_scripts { .. }` records `client_script`) and the legacy
//! `key 'value' { extra }` form that records `key_extra` as JSON.

mod glob;
mod normalize;

pub use glob::glob_value;
pub use normalize::path_normalize;

use mlua::{Function, Lua, LuaOptions, StdLib, Table, Value, Variadic};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("no manifest (fxmanifest.lua or __resource.lua)")]
    NoManifest,
    #[error("could not parse {file}: {message}")]
    Parse { file: String, message: String },
    #[error("could not execute resource metadata file {file}: {message}")]
    Exec { file: String, message: String },
    #[error("lua: {0}")]
    Lua(String),
}

impl From<mlua::Error> for ManifestError {
    fn from(e: mlua::Error) -> Self {
        ManifestError::Lua(e.to_string())
    }
}

/// Ordered metadata, matching FXServer's `std::multimap<key, value>`:
/// iteration is by key (byte order) and, within a key, insertion order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub entries: Vec<(String, String)>,
    pub is_cfxv2: bool,
}

impl Manifest {
    /// All values for `key`, in insertion order.
    pub fn get<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.entries
            .iter()
            .filter(move |(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn first<'a>(&'a self, key: &'a str) -> Option<&'a str> {
        self.get(key).next()
    }

    pub fn has(&self, key: &str) -> bool {
        self.get(key).next().is_some()
    }

    /// Glob every value of `key` the way `GlobEntriesVector` does: each value
    /// is expanded and sorted on its own, then results are concatenated.
    pub fn glob_entries(&self, resource_path: &str, key: &str) -> Vec<String> {
        let mut out = Vec::new();
        for v in self.get(key) {
            out.extend(glob_value(resource_path, v));
        }
        out
    }

    /// Entries ignoring the `*_extra` keys, which FXServer ignores when
    /// deciding whether a refreshed manifest counts as changed.
    pub fn without_extra(&self) -> Vec<(&str, &str)> {
        let mut v: Vec<_> = self
            .entries
            .iter()
            .filter(|(k, _)| !k.ends_with("_extra"))
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        v.sort();
        v
    }
}

const DSL: &str = r#"
local addMetaData = AddMetaData
return function(chunk)
    setmetatable(_G, {
        __index = function(t, k)
            local raw = rawget(t, k)
            if raw then return raw end
            return function(value)
                local key = k
                if type(value) == 'table' then
                    if k:sub(-1) == 's' then key = k:sub(1, -2) end
                    for _, v in ipairs(value) do addMetaData(key, v) end
                else
                    addMetaData(k, value)
                end
                return function(extra)
                    addMetaData(key .. '_extra', json.encode(extra))
                end
            end
        end,
    })
    chunk()
    setmetatable(_G, nil)
end
"#;

/// Evaluate the manifest of the resource rooted at `dir`.
pub fn load(dir: &Path) -> Result<Manifest, ManifestError> {
    let candidates = [("fxmanifest.lua", true), ("__resource.lua", false)];
    let mut picked = None;
    for (name, v2) in candidates {
        let p = dir.join(name);
        if let Ok(src) = std::fs::read(&p) {
            picked = Some((name, v2, src));
            break;
        }
    }
    let (file, v2, src) = picked.ok_or(ManifestError::NoManifest)?;
    eval(file, &src, v2)
}

/// Evaluate manifest source. `file` is used for error messages and chunk name.
pub fn eval(file: &str, src: &[u8], is_cfxv2: bool) -> Result<Manifest, ManifestError> {
    let libs = StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::COROUTINE | StdLib::UTF8;
    let lua = Lua::new_with(libs, LuaOptions::default())?;
    let collected: Rc<RefCell<Vec<(String, String)>>> = Rc::default();

    {
        let sink = collected.clone();
        let add = lua.create_function(move |lua, (k, v): (mlua::String, Value)| {
            let v = match v {
                Value::String(s) => s.to_str()?.to_string(),
                // luaL_checkstring coerces numbers with Lua's own formatting
                n @ (Value::Integer(_) | Value::Number(_)) => lua
                    .coerce_string(n)?
                    .map(|s| s.to_str().map(|s| s.to_string()))
                    .transpose()?
                    .unwrap_or_default(),
                other => {
                    return Err(mlua::Error::runtime(format!(
                        "bad argument #2 to 'AddMetaData' (string expected, got {})",
                        other.type_name()
                    )))
                }
            };
            let k = k.to_str()?.to_string();
            if !k.eq_ignore_ascii_case("is_cfxv2") {
                sink.borrow_mut().push((k, v));
            }
            Ok(())
        })?;
        lua.globals().set("AddMetaData", add)?;
    }
    install_json(&lua)?;
    let globals = lua.globals();
    for unsafe_global in ["dofile", "load", "loadfile"] {
        globals.set(unsafe_global, Value::Nil)?;
    }

    let wrapper: Function = lua.load(DSL).set_name("=resource_init").eval()?;
    let chunk = lua
        .load(src)
        .set_name(format!("@{file}"))
        .into_function()
        .map_err(|e| ManifestError::Parse {
            file: file.to_string(),
            message: e.to_string(),
        })?;
    wrapper.call::<()>(chunk).map_err(|e| ManifestError::Exec {
        file: file.to_string(),
        message: e.to_string(),
    })?;
    drop(globals);
    drop(lua);

    let mut entries = Rc::try_unwrap(collected)
        .map(RefCell::into_inner)
        .unwrap_or_else(|rc| rc.borrow().clone());
    if is_cfxv2 {
        entries.push(("is_cfxv2".into(), "true".into()));
    }
    // multimap order: stable sort by key keeps insertion order within a key
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    Ok(Manifest { entries, is_cfxv2 })
}

/// Minimal `json.encode` used only by the legacy `_extra` form.
fn install_json(lua: &Lua) -> mlua::Result<()> {
    let json: Table = lua.create_table()?;
    let encode = lua.create_function(|_, v: Variadic<Value>| {
        let v = v.into_iter().next().unwrap_or(Value::Nil);
        let j = to_json(&v, 0)?;
        Ok(serde_json::to_string(&j).unwrap_or_else(|_| "null".into()))
    })?;
    json.set("encode", encode)?;
    lua.globals().set("json", json)?;
    Ok(())
}

fn to_json(v: &Value, depth: usize) -> mlua::Result<serde_json::Value> {
    use serde_json::Value as J;
    if depth > 64 {
        return Err(mlua::Error::runtime("json.encode: nesting too deep"));
    }
    Ok(match v {
        Value::Nil => J::Null,
        Value::Boolean(b) => J::Bool(*b),
        Value::Integer(i) => J::from(*i),
        Value::Number(n) => serde_json::Number::from_f64(*n).map(J::Number).unwrap_or(J::Null),
        Value::String(s) => J::String(s.to_str()?.to_string()),
        Value::Table(t) => {
            let len = t.raw_len();
            let mut pairs = 0usize;
            for p in t.clone().pairs::<Value, Value>() {
                p?;
                pairs += 1;
            }
            if len > 0 && pairs == len {
                let mut arr = Vec::with_capacity(len);
                for i in 1..=len {
                    arr.push(to_json(&t.raw_get::<Value>(i)?, depth + 1)?);
                }
                J::Array(arr)
            } else if pairs == 0 {
                J::Array(vec![])
            } else {
                let mut map = serde_json::Map::new();
                for p in t.clone().pairs::<Value, Value>() {
                    let (k, val) = p?;
                    let key = match k {
                        Value::String(s) => s.to_str()?.to_string(),
                        Value::Integer(i) => i.to_string(),
                        Value::Number(n) => n.to_string(),
                        _ => continue,
                    };
                    map.insert(key, to_json(&val, depth + 1)?);
                }
                J::Object(map)
            }
        }
        _ => J::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsl_basic() {
        let src = br#"
fx_version 'cerulean'
game 'gta5'
client_scripts { 'client/*.lua', 'shared.lua' }
server_script 'server.lua'
files { 'ui/index.html' }
ui_page 'ui/index.html'
my_data 'x' { a = 1 }
"#;
        let m = eval("fxmanifest.lua", src, true).unwrap();
        let cs: Vec<_> = m.get("client_script").collect();
        assert_eq!(cs, ["client/*.lua", "shared.lua"]);
        assert_eq!(m.first("server_script"), Some("server.lua"));
        assert_eq!(m.first("file"), Some("ui/index.html"));
        assert_eq!(m.first("my_data_extra"), Some(r#"{"a":1}"#));
        assert!(m.has("is_cfxv2"));
        // keys sorted like std::multimap
        let keys: Vec<_> = m.entries.iter().map(|e| e.0.as_str()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn no_io_or_os() {
        // unknown globals resolve to DSL functions, so check with rawget
        let src = b"for _, k in ipairs({'io','os','load','dofile','loadfile'}) do assert(rawget(_G, k) == nil, k) end";
        eval("fxmanifest.lua", src, true).unwrap();
    }

    #[test]
    fn parse_error() {
        assert!(matches!(
            eval("fxmanifest.lua", b"fx_version (", true),
            Err(ManifestError::Parse { .. })
        ));
    }
}
