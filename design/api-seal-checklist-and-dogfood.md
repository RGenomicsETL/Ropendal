# API seal checklist and dogfood applications

This document turns the low-level Ropendal API into a concrete checklist before
calling the byte-substrate API sealed. The goal is not to finish every future
adapter, but to prove that consumers can build filesystem-like, chunk-store, and
archive-store layers without forcing new semantics into the core.

## Seal target

Seal only the low-level substrate when these are true:

- byte operations and byte-store operations have stable names, shapes, and error
  semantics;
- async operations are the primary primitive and synchronous helpers are wrappers
  or waits over the same semantics;
- `OpendalBytes`/ALTREP and native C caller-owned-buffer paths are stable enough
  for downstream consumers;
- cache adapters are explicit and do not hide format policy;
- codec extension points are explicit raw-byte transforms, not format sniffing;
- dogfood applications can be written as adapters over the public API rather than
  by reaching into internals.

## API item checklist

| API area | Seal criteria | Current status | Missing / decision before seal | Dogfood that should exercise it |
|---|---|---:|---|---|
| Filesystem constructor and URI handling | `opendal()`, `opendal_uri()`, credentials, headers, path normalization, capabilities are documented and stable | usable | keep capability fields stable enough for adapters; audit secrets redaction | remote Zarr store over S3/HTTP fixture |
| Byte reads | complete reads, scalar ranges, vectorized paths/ranges, `byte_ranges()` shape rules, tuning knobs | usable | final shape/error audit for vectorized nested results | indexed-range reader, zip central-directory parser |
| Byte writes | create-only `fs_write()`, overwrite `fs_replace()`, append split, vectorized data, tuning | usable | audit create-vs-replace behavior across store wrappers and C API | Rarr-like full-array write and partial-chunk update |
| Metadata / existence | `fs_stat()`, `fs_exists()` and Aio are stable | usable | add public `store_stat()` / `store_stat_aio()` and C `ropendal_store_stat_aio()` if stores are a first-class consumer target | Rarr-like backend, zarrs_zip-style virtual store |
| Namespace operations | list/walk/delete/mkdir/copy/rename and Aio | usable | decide if `store_mkdir()`, `store_copy()`, `store_rename()` are in scope or explicitly out | empty Zarr group markers; array relocation tests |
| `OpendalBytes` | immutable handle, length, slicing, raw ALTREP facade, explicit materialization boundary | usable | profile read-only `DATAPTR()` only after downstream coverage; keep writable pointer materializing | Rarr-like chunk reads, C/R ALTREP consumer |
| Async Aio | pending/unresolved, collect/call, stop, race, cv, monitor, callbacks | usable | broader cancellation race/service coverage; freeze active-binding names | async multi-chunk reads/writes; native C callbacks |
| R byte store | prefix-scoped key-to-bytes read/write/replace/exists/list/delete, vectorized/Aio wrappers, escape rejection | usable | `store_stat()` gap; decide hierarchy helpers; document no hidden codecs | Rarr-like Zarr backend |
| Store caches | explicit full-object cache and fixed-size block cache, validation modes, mutation invalidation, repair semantics | usable | eviction/readahead policy intentionally future; maybe expose cache stats later, not for seal | remote chunk store, zip archive block-cache reads |
| Native C API | pure C, async-first, zero-initializable options, caller-owned buffers, byte/store/cache/codec/readv coverage | usable | add store stat if R store stat is added; keep ABI version and wasm stubs synchronized | tiny downstream C store consumer |
| Codecs | explicit raw-byte transforms via `codec_config()` and C encode/decode; no auto-selection in stores | usable for identity/gzip/zlib | additional codecs can be added incrementally; document complete-object vs range-safe semantics | zstd/lz4/blosc codec spike |
| webR | package loads and unsupported paths fail explicitly | shim-first usable | keep new generated symbols stubbed; no claim of full OpenDAL browser runtime | webR build smoke |
| Docs/tests | status, refinement log, testing plan, NEWS, generated Rd synchronized | usable | every API-seal decision gets status/test coverage | full local gates and CI |

## zarrs_zip-style stress test

`zarrs_zip` is a useful stress model because it adapts one archive object into a
hierarchical read-only Zarr store. The storage adapter needs two layers:

1. **Parent archive object** — range-readable bytes plus a known size.
2. **Virtual ZIP entry store** — entry keys, entry sizes, entry range reads, and
   directory/prefix listings derived from the ZIP central directory.

### Parent archive object checklist

| Required by a ZIP adapter | Ropendal primitive | Status / note |
|---|---|---|
| Open local/HTTP/S3 archive object | `opendal()` / `opendal_uri()` | present |
| Get archive byte length | `fs_stat()` | present for filesystem paths; store-level equivalent missing |
| Read central-directory ranges | `fs_read(offset=, size=)` / `fs_read_bytes()` | present |
| Read many archive ranges | `byte_ranges()` and C `readv` | present |
| Cache repeated range reads | `store_block_cache()` when archive is represented as a one-key store, or future fs-level block cache | store-level present; fs-level archive adapter still future |
| Async range reads | `fs_read_aio()` / C readv Aio | present |

### Virtual ZIP entry store checklist

| Required by a virtual entry store | Ropendal shape today | Gap / decision |
|---|---|---|
| `exists(key)` | `store_exists()` if implemented as a byte store adapter | present |
| `size_key(key)` / entry stat | no public `store_stat()` | add before sealing store API |
| `read(key)` | virtual adapter can return `OpendalBytes` / raw | feasible |
| `read(key, ranges)` | stored ZIP entries can translate to parent archive ranges; compressed entries need full decompression | feasible in adapter, not core |
| `list()` / `list_prefix()` | `store_list(recursive=TRUE)` can be filtered | feasible but a structured `list_dir` keys/prefixes helper may be useful |
| `list_dir()` split keys/prefixes | derive from entries today | optional convenience; not required for byte substrate |
| write/update archive entries | not provided by read-only ZIP adapter | separate writable ZIP transaction/finalization layer |

Conclusion: the interface is flexible enough for a **read-only zarrs_zip-style
adapter**, but the stress test highlights two seal items: public store stat and a
decision on structured directory listings. Writable ZIP remains a separate
archive writer/finalizer, not a byte-store feature.

## Dogfood applications

These should be small packages, vignettes, or tests that use only public API.
They are meant to reveal abstraction gaps before API freeze.

### 1. Rarr-like Zarr byte-store backend

Build a tiny adapter with methods equivalent to `exists`, `read`, `write`,
`replace`, `list`, and later `stat` over `byte_store()`.

Minimum scenario:

- write `zarr.json` or `.zarray` metadata;
- write several chunk keys;
- read a subset by computing touched chunk keys;
- update a partial chunk by read/decode/patch/encode/replace;
- repeat through `store_cache()` and `store_block_cache()`;
- run against local `fs`, HTTP fixture or public object store if possible.

Likely gaps revealed:

- `store_stat()`;
- whether `store_mkdir()` is useful for empty groups;
- whether vectorized store read/write shapes are ergonomic enough;
- whether cache invalidation is obvious for format-level updates.

### 2. zarrs_zip-style read-only ZIP store

Build a proof adapter over a ZIP archive object:

- stat archive object for size;
- parse central directory using range reads;
- expose entry names as keys;
- support `exists`, `stat`, `read`, `list`, and `list_dir` over entries;
- fast-path stored entries by translating entry ranges to parent archive ranges;
- full-read/decompress deflated entries in the adapter;
- use `store_block_cache()` or a future fs-level block cache to avoid repeated
  central-directory/header reads.

Likely gaps revealed:

- need for public `store_stat()` or a generic `stat` method on store-like objects;
- whether a common interface should distinguish `list_recursive()` from
  `list_dir()` returning immediate keys plus prefixes;
- whether fs-level block cache is worth adding in addition to store block cache;
- explicit unsupported behavior for writable ZIP.

### 3. Sharded Zarr / large-object range index

Build a toy sharded array where many logical chunk keys map to byte ranges in one
large object plus a small index.

Minimum scenario:

- read shard index;
- map chunk coordinates to `(path, offset, size)`;
- read many ranges via `byte_ranges()`;
- cache shard reads with fixed-size block cache;
- update policy documented as rewrite-shard, not in-place key write.

Likely gaps revealed:

- vectorized range result names and nested shapes;
- cache behavior for many ranges in the same large object;
- whether we need a public range-store adapter distinct from key-to-object store.

### 4. Native C downstream store consumer

Create a tiny installed-library C roundtrip outside Ropendal internals that:

- opens an fs and store;
- writes and replaces keys;
- reads into caller-owned buffers;
- lists keys;
- stats keys once available;
- uses callbacks or monitor to collect multiple async completions.

Likely gaps revealed:

- C option naming/zero defaults;
- lifetime rules for bytes, entries, and callback userdata;
- missing C helpers that R wrappers accidentally hide.

### 5. Codec spike: zstd or lz4 as a raw-byte codec

Add one additional codec only as a spike branch or small PR.

Checklist:

- Rust encode/decode implementation;
- R `codec_config("zstd")` or `codec_config("lz4")` support;
- C encode/decode support;
- sync/Aio/C roundtrip tests;
- documentation whether partial decode is unsupported.

Likely gaps revealed:

- codec registry extensibility;
- whether codec errors map cleanly to error values;
- whether complete-object-only codec semantics are sufficiently explicit.

## Freeze decision gates

Before declaring the substrate API sealed:

1. Implement or explicitly reject `store_stat()` / `ropendal_store_stat_aio()`.
2. Decide and document `store_mkdir()`, `store_copy()`, `store_rename()`, and
   structured `store_list_dir()` scope.
3. Complete at least two dogfood applications: Rarr-like byte-store backend and
   zarrs_zip-style read-only ZIP store proof.
4. Run full gates: `make rd`, `make test-fast`, `make test-ci`, `make test-webr`,
   `make test-rust`, and relevant opt-in network tests when credentials exist.
5. Update `design/STATUS.md`, `design/refinement-log.md`, `design/testing-plan.md`,
   `NEWS.md`, and generated docs for any public API decision.
