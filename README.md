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
| `fxs scan <root>` | FXServer's resource scan | Same discovery rules (`resources/`, then `citizen/system_resources`), manifests evaluated in parallel. `--list` prints folders and escrow flags. |
| `fxs watch <root>` | FXServer's rebuild on next start | Watches `resources/` and re-warms only the resources whose files changed, while you edit. |
| `fxs deps <root>` | FXServer's dependency loader errors at start time | Missing dependencies (both `dependency` and `dependencies` forms), cycles, broken `@resource/file` references. `--order` prints FXServer's start order. Exits 1 on any issue, for CI or a pre-boot gate. |
| `fxs memmap <pid>`, `fxs peek <pid> <addr>` | none (diagnosis) | Read-only memory breakdown of a running FXServer. |
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

### Real FXServer boots against an fxs-built cache

Separate test root on its own port, 18 resources (989 MiB of stream files),
licensed, FXServer v1.0.0.35265 win32, three alternating pairs:

| Run | fxs warm | FXServer ready (all 18 started) | Cache files FXServer rewrote |
|---|---|---|---|
| FXServer builds its own cache | | 1968, 2136, 1950 ms | |
| `fxs warm` from scratch, then boot | 978, 1211, 899 ms | 1628, 2222, 1547 ms | **0, 0, 0** |
| One stream file changed, `fxs warm` with its index | 262 ms (1 file hashed, 7,655 reused) | 1695 ms | |

- FXServer accepts the fxs-built cache as current: it rewrote nothing in
  any of the three runs. The first boot test caught one bug, which this
  check confirmed fixed: FXServer stamps an unopenable `@other/file` entry
  as `mt 0, s u64::MAX`, not all zeros.
- With 18 resources and hot page cache, hashing is not what makes this
  server's boot slow; ready time is ~1.5 to 2 s either way. The win scales
  with stream size, cold disks and frequent asset edits (FXServer rehashes
  the whole resource; fxs hashes the changed file only).

## Memory: what FXServer's RAM number actually is

`fxs memmap <pid>` reads a running FXServer's address space without writing
to it or injecting anything (`VirtualQueryEx`, `QueryWorkingSetEx`,
`ReadProcessMemory`). `fxs peek <pid> <addr>` summarizes one block.

Live Rivo server (28 resources started, 156 scanned, idle, no players):

| | Committed | Physically resident |
|---|---|---|
| Private memory, total | 887 MiB | 101 MiB |
| NT heap (C++ objects, strings; 26 segments) | 232 MiB | 40 MiB |
| Other private (pools, script runtimes, stacks) | 655 MiB | 61 MiB |
| One 128 MiB block (contains event/state bag names, likely a net buffer, UNCONFIRMED) | 128 MiB | 3.9 MiB |
| Four 32 MiB blocks | 128 MiB | 4 MiB total |
| Process working set | | 124 MB |

Cost per script runtime, measured by booting the test root with and without
the one resource that needs it (30 s settle, private memory resident):

| Change | Resident private | Private bytes | Working set |
|---|---|---|---|
| Baseline, 18 resources | 113 MiB | 480 MB | 177 MB |
| Without vMenu (the only C# resource, so no Mono) | 81.5 MiB (**-32 MiB**) | 463 MB | 143 MB |
| `oxmysql` (Node) vs no JS resource | 107 vs 90 MiB (**-17 MiB**) | 406 vs 388 MB | 167 vs 147 MB |

The cheapest RAM cut on this server is the Mono runtime: if vMenu is the
only C# resource, a Lua menu saves about 32 MiB resident.

What this means:
- The "908 MB private bytes" figure is mostly commit charge for
  preallocated, never-touched buffers. Real physical use is ~100 MiB
  private plus shared images. Commit charge still matters on a VPS with a
  small pagefile (Windows) or strict overcommit (Linux).
- The heap holds parsed manifest metadata for **every scanned resource**,
  started or not: one `std::string` key per entry (`client_script`,
  `server_script`, `data_file` repeated hundreds of times). Keeping unused
  resources out of `resources/` reduces it.
- Lua resources each get their own rpmalloc heap on Windows; Node (`libnode`,
  86 MiB image) loads for any JS resource and Mono for any C# resource
  (here vMenu). Each runtime has a fixed cost even for one small resource.
- "Resident" excludes pages Windows trimmed to standby, so it is a lower
  bound on touched memory. Attribution of the large blocks to specific
  FXServer code needs symbols and was not done.

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

Done: asset pipeline (scan, manifests, hashing, caches, RPF2), file serving,
memory probe, boot-verified cache compatibility, per-runtime RAM costs.

Next candidates, all of which can run next to stock FXServer:
1. Done since: incremental re-warm (`fxs watch`) and dependency checks (`fxs deps`).
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
