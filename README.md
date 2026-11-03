# fxserver-rs

Rust replacements for the slow parts of FXServer (FiveM), built to run
**alongside** stock FXServer. Cfx.re's license, auth and asset escrow stay
exactly where they are.

This is an open-source port idea, not a release. It ships no FXServer code or
binaries.

## Why alongside, not instead

FXServer's license key handling, Cfx.re ticket auth, server list heartbeat and
`.fxap` escrow live in closed components (`svadhesive.dll` plus closed code
compiled into `citizen-server-impl`). They plug into FXServer's C++ internals
and can only run inside a stock FXServer process. A from-scratch Rust server
could only ever run as an unlisted `sv_lan` server, with no license
identifiers and no escrowed resources.

So this project replaces work FXServer does inefficiently, through mechanisms
FXServer already supports, and never touches licensing, auth or escrow.

```
            players (stock FiveM client)
              |  connect, auth, sync        |  file downloads (fileserver_add)
              v                             v
   +----------------------+        +--------------------+
   | stock FXServer       |        | fxs serve (Rust)   |
   |  + svadhesive        | <----- |  reads FXServer's  |
   |  + escrow (.fxap)    |  cache |  published files   |
   +----------------------+  files +--------------------+
              ^
              | cache/files/<res>/ prebuilt before boot
   +----------------------+
   | fxs warm (Rust)      |
   +----------------------+
```

## What it does

| Command | Replaces | How |
|---|---|---|
| `fxs warm <root>` | FXServer's per-resource stream hashing and `resource.rpf` packing at resource start | Writes FXServer's own `cache/files/<res>/` format byte for byte (`resource.sfl` + `.db`, `resource.rpf` RPF2 + `.db`, `stream_cache` hard links). FXServer then finds every set up to date and skips the work. Hashing is incremental per file (FXServer rehashes every stream file of a resource if one changes) and parallel across all resources. |
| `fxs warm <root> --compare` | none (verification) | Recomputes everything and diffs it against FXServer's current cache, writing nothing. |
| `fxs serve <root>` | FXServer's `/files` handler for client downloads | Serves FXServer's published bytes (never rebuilt, so hashes always match) for `fileserver_add`. Adds ETag, `If-None-Match`, `Range`. Refuses escrowed resources. |
| `fxs fileserver-cfg <root> <url>` | none | Prints exact-name `fileserver_add` lines for non-escrowed resources only. |
| `fxs scan <root>` | FXServer's resource scan | Same discovery rules, manifests evaluated in parallel. |
| `fxs bench-download <root> <url>` | none | Downloads every published file and verifies its SHA-1. |

`--cfg server.cfg` limits any command to resources the cfg starts (follows
`exec`, `ensure`, `start`, `[category]`), and `--only a,b` to named ones.

## Escrowed (`.fxap`) resources

They keep working unchanged. Stock FXServer loads, validates and serves them
through Cfx.re's own escrow code and your license.

- `fxs warm` skips any resource with a `.fxap`, so FXServer builds its cache,
  including the encrypted flag that only the closed code sets.
- `fxs serve` returns 403 for them. FXServer runs a per-request files filter
  on escrowed downloads that can block or rewrite bytes; serving them from
  anywhere else would skip it.
- `fxs fileserver-cfg` never routes them away from FXServer.

## Measured (Rivo dev server, i9-14900KF, 32 threads, NVMe, Windows 11)

156 resources, 15 escrowed, 9,746 stream files, 1.97 GB.

| What | Result |
|---|---|
| Resource scan + 156 manifests | 9 ms walk, 59 ms manifests |
| Fidelity vs FXServer's own cache, every set FXServer considers current | 60 of 60 `resource.rpf` + `.db` and 78 of 78 `resource.sfl` + `.db` **byte-identical**, 0 differ |
| Cold build of every cache, 1.97 GB hashed | 8.1 s on 1 thread, 1.23 s on 32 threads |
| Rebuild with nothing changed | 0.42 s |
| `fxs serve`: 28 started resources, 7,683 files, 1010 MiB | all SHA-1 verified, 550 to 1003 MiB/s, 21 to 34 MB RAM |

Caveats, stated plainly:
- Timings ran with the files already in the OS page cache. Cold disk reads
  will be slower for both FXServer and fxs.
- The single-thread number approximates FXServer's serial hashing; it is not
  a measurement of FXServer itself.
- FXServer's own `/files` throughput could not be measured: it answers 403 to
  anything that is not a connected client.
- No FXServer boot was run against a warmed cache in this work. Acceptance
  rests on the byte-identical output above and a port of FXServer's own
  freshness rules.

## Using it

```bat
:: before starting FXServer (e.g. in your run script)
fxs warm "Z:\path\to\server" --cfg server.cfg

:: optional: offload client downloads
fxs serve "Z:\path\to\server" --bind 0.0.0.0:30130 --require-token
fxs fileserver-cfg "Z:\path\to\server" http://your.host:30130 --cfg server.cfg > cfg\fileserver.cfg
:: then add `exec cfg/fileserver.cfg` to server.cfg
```

`fxs serve` is a public file server, like any CDN behind `fileserver_add`.
`--require-token` rejects requests without the `X-CitizenFX-Token` header
real clients send. The token cannot be validated outside FXServer, so this
only filters casual scrapers.

## Compatibility notes (ported FXServer behaviour, including quirks)

- Manifest DSL: unknown globals become recorders, a table call strips one
  trailing `s`, and `key 'v' { extra }` records `key_extra` as JSON.
  `io`, `os`, `load`, `loadfile` and `dofile` are absent.
- Globs: per-segment anchored, case-sensitive regex. `**` expands via the
  `a/b` legacy submask plus recursion. `dir/*/x.lua` matches nothing, and
  `dir/cl*/x.lua` only matches a directory named exactly `cl`, as in
  FXServer.
- `path_normalize` is a byte-level port, including splitting at whitespace
  and `%` tokens.
- Stream files: depth-first with a directory's own files before its
  subdirectories, `<res>/stream//file` path strings, the first basename wins,
  and the `RSC\x05` physical-flags quirk is kept.
- RPF2: children sorted by bytes and contiguous, names in pre-order, data in
  insertion order, 2048-byte alignment, `tocSize` absolute.

## Roadmap to a fuller Rust server

Done: asset pipeline (scan, manifests, hashing, caches, RPF2), file serving.

Next candidates, all of which can run next to stock FXServer:
1. Resource-start preflight inside the run script, with incremental re-warm on file change.
2. A `getConfiguration`-shaped manifest dump and a resource dependency graph, for tooling and CI.
3. Memory profiling of FXServer with the real resource set, to find where RAM goes (script runtimes and state sync are the likely large parts).
4. Rust-side services resources already call over HTTP or exports (DB pooling and similar).

Out of scope while license, auth and escrow must stay intact:
- Replacing FXServer's connect, auth and heartbeat.
- Replacing state sync or the ENet layer in a listed server. The closed
  components link into FXServer's C++ objects.
- Loading `.fxap` outside FXServer.

## License

LGPL-2.0-only (see `LICENSE`). The FXServer behaviour ported here comes from
`code/components/citizen-*` in [citizenfx/fivem](https://github.com/citizenfx/fivem),
which is LGPL-2.0. `path_normalize` derives from Premake (BSD-3-Clause, Jason
Perkins and contributors). No code from the parts under the Rockstar Games
Creator Platform License is included. FiveM and FXServer are trademarks of
their owners; this project is not affiliated with Cfx.re or Rockstar Games.
