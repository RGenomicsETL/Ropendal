# Opinionated abstract filesystem access for R

Ropendal is an opinionated abstract filesystem access layer for R, backed by
Apache OpenDAL. It should borrow the useful storage-abstraction idea from
systems like fsspec, but not copy Python ergonomics. The package should remain
R-native: verb-first, byte-first, async-aware, capability-aware, and friendly to
native R package authors through both ALTREP raw vectors and a stable pure C API.

## Positioning

Ropendal is not:

- a Python fsspec clone;
- a cloud SDK wrapper;
- an R6 object store with `$read()` / `$write()` as the primary surface;
- a format library for Zarr, Arrow, BAM, ZIP, or Parquet.

Ropendal is:

- a byte-oriented filesystem substrate;
- a common operation contract across local, HTTP, object-store, and adapter
  filesystems;
- an async substrate modeled closer to nanonext than to Python event-loop
  ergonomics;
- a C-facing integration layer through both R vectors/ALTREP and a pure C ABI;
- a foundation on which compatibility layers such as R connections, ALTREP raw
  vectors, ZIP filesystems, byte stores, caches, and domain-specific readers can
  be layered.

A useful one-line description:

> Opinionated abstract filesystem access for R: byte-first, async-capable,
> capability-aware, and native-package friendly.

## API style: prefer `function(object, ...)`

The primary API should remain:

```r
fs_read(fs, "x.bin")
fs_write(fs, "x.bin", raw)
fs_ls(fs, "prefix/")
fs_stat(fs, "x.bin")
fs_read_aio(fs, "x.bin")
```

not:

```r
fs$read("x.bin")
fs$write("x.bin", raw)
fs$list("prefix/")
```

Rationale:

1. **R dispatch works on verbs.** `fs_read(object, ...)` can become or remain a
   generic over `OpendalFs`, ZIP adapters, cache adapters, byte stores, and
   future S7 interfaces.
2. **Pipes work naturally.**

   ```r
   opendal("s3", bucket = "lab-data", root = "project", auth = credentials_s3(...)) |>
     fs_read("samples/HG001.bam", offset = offsets, size = sizes, result = "flat")
   ```

3. **Vectorization is clearer.** Ropendal's core verbs are shape-preserving
   vectorized operations over paths, ranges, and payloads. This is more natural
   in R as `fs_read(fs, path, offset, size, ...)`.
4. **Capabilities are runtime properties.** A handle may support different
   operations depending on backend, adapter, auth, object state, or declared
   semantics. `fs_capabilities(fs)` should report the effective contract.
5. **Adapters compose.** `fs_zip(fs, "archive.zip")`, `fs_cache(fs, ...)`, and
   `byte_store(fs, prefix = ...)` can all feed the same verb surface.

## Core layers

Keep the architecture layered:

```text
Core:
  OpendalFs
  OpendalAio
  OpendalBytes
  fs_* verbs
  monitor/CV primitives
  ALTREP raw facade
  pure C API

Compatibility adapters:
  R connections
  byte/chunk store
  cache layers
  ZIP/archive filesystem adapters

Domain adapters:
  Zarr arrays
  Arrow/Parquet readers
  Bioconductor range readers
  geospatial/image readers
```

The core must remain byte-first. R objects, R closures, and SEXP values should
not enter background Tokio/OpenDAL work. Background operations should operate on
owned bytes, metadata, entries, paths, options, booleans, errors, and C-owned
buffers.

## Bytes, ALTREP, and C consumers

The goal is to avoid materializing an ordinary R `raw` vector until a caller
actually requires one. `OpendalBytes` remains the storage/lifetime owner and the
ALTREP raw vector is a first-class R-vector facade over it.

```r
bytes <- fs_read_bytes(fs, "x.bin")
length(bytes)
fs_write(fs, "copy.bin", bytes)
raw <- as.raw(bytes)              # raw ALTREP facade; materializes on pointer access
```

The right distinction is not "ALTREP for R, C API for true C". The right
distinction is:

1. **ALTREP raw path:** for C consumers that accept a `SEXP` raw vector and use
   the R vector API. These consumers can iterate without forcing a full raw
   materialization if they use `RAW_GET_REGION()`, element access, or compact-
   representation-friendly APIs. Ropendal's ALTREP `data1` stores/protects a Rust byte holder;
   `Get_region` memcpy's only the requested byte window into the caller's
   buffer. Full materialization is delayed until a pointer-requesting or
   mutation-requiring access path makes it necessary.
2. **Pure C ABI path:** for downstream packages that explicitly depend on
   Ropendal and want direct async, monitors, readv/read-into, caller-owned
   buffers, or an R-independent ABI boundary. This path is not "more true" than
   ALTREP; it is a different integration contract.

Therefore, ALTREP is treated as a core C-facing compatibility feature, not a
thin R-only wrapper.

### ALTREP behavior

The ALTREP raw class stores an external pointer in `data1` that owns a Rust byte
holder derived from `OpendalBytes`. `data2` stores the materialized R raw cache
after pointer access.

Current behavior:

```text
Length(x):
  return ropendal_bytes_len(data1)

Elt(x, i):
  copy one byte from OpendalBytes; no full materialization

Get_region(x, i, n, buf):
  copy requested region from data2 if materialized, otherwise from OpendalBytes;
  no full materialization

Dataptr_or_null(x):
  return NULL until the object has already materialized into `data2`

Dataptr(x, writable):
  materialize into an R-owned RAWSXP stored in `data2` and return that pointer
```

The important distinction is `Dataptr(writable = TRUE)`: writable raw-vector
access must never point into OpenDAL/Rust `Buffer` storage. The current
implementation is conservative and materializes for any `DATAPTR()` request;
future profiling can decide whether read-only contiguous pointers are worth
exposing. `RAW_GET_REGION()`-style consumers already get efficient block
iteration with bounded copying and no full raw-vector allocation, while code
that requests pointer memory crosses the materialization boundary.

### Pure C ABI still matters

The pure C API remains important for packages that do not want to pass through R
vectors at all:

```c
ropendal_read_into_aio(fs, &opts, dst, dst_len, &aio, &err);
ropendal_aio_wait(aio, -1, &err);
ropendal_aio_result_nread(aio, &nread, &err);
```

That is complementary to ALTREP, not a replacement for it. A native package that
already consumes raw `SEXP` values should be able to benefit from the ALTREP
facade. A native package that wants async scheduling, monitors, readv, or
caller-owned buffers can use the C ABI.

Possible C-side additions only if needed after profiling:

```c
ropendal_status_t ropendal_bytes_copy_region(
  const ropendal_bytes_t *bytes,
  size_t offset,
  size_t len,
  uint8_t *dst,
  size_t *nread,
  ropendal_error_t **err
);

ropendal_status_t ropendal_bytes_slice(
  const ropendal_bytes_t *bytes,
  size_t offset,
  size_t len,
  ropendal_bytes_t **out,
  ropendal_error_t **err
);
```

## R connection adapter

R connections are important for compatibility, but they are not the core
abstraction. `readBin()` and `writeBin()` necessarily interact with R's
connection machinery and R vectors. They should be offered as adapters over
Ropendal iterators.

Proposed shape:

```r
con <- fs_connection(
  fs,
  "x.bin",
  open = "rb",
  block_size = 8 * 1024^2,
  max_blocks = 4,
  readahead = 2,
  read_concurrency = NULL,
  cache = c("none", "memory", "file")
)

x <- readBin(con, "raw", n = 65536)
close(con)
```

Writing:

```r
con <- fs_connection(
  fs,
  "out.bin",
  open = "wb",
  chunk_size = 8 * 1024^2,
  write_concurrency = 4
)

writeBin(payload, con)
close(con) # finalize write sink / multipart upload
```

Primitive Ropendal functions should keep returning `opendalErrorValue` for
backend failures. Connection callbacks should signal R conditions, because base
R connection consumers expect bytes or errors, not structured value objects.

## Buffering model

Keep three concepts separate.

### 1. Transfer tuning

Already part of the core operation API:

```r
fs_read(
  fs,
  "big.bin",
  offset = offsets,
  size = sizes,
  batch_concurrency = 32,
  read_concurrency = 4,
  chunk_size = 8 * 1024^2,
  coalesce_gap = 64 * 1024
)
```

This controls execution of one operation. It is not a persistent cache.

### 2. Iterator / connection buffering

Used for sequential reads, `readBin()`, and `seek()` on connection adapters.

Suggested connection options:

```r
fs_connection(
  fs,
  "x.bin",
  open = "rb",
  block_size = 8 * 1024^2,
  max_blocks = 4,
  readahead = 2,
  cache = "memory"
)
```

### 3. Persistent block cache

A future filesystem adapter/layer, with explicit invalidation semantics:

```r
cached <- fs_cache(
  fs,
  cache_dir = tools::R_user_dir("Ropendal", "cache"),
  block_size = 8 * 1024^2,
  validate = c("etag", "version", "last_modified_size", "none")
)
```

Do not add implicit full-object downloads for huge remote objects. Hidden
downloads should require explicit cache settings.

## Monitoring system

The monitoring system is a core Ropendal differentiator and should be documented
as such. Ropendal should model this more on nanonext-style Aio, condition
variables, and monitors than on Python fsspec.

The separation should be:

```text
Aio      owns one operation and its eventual result
CV       wakes waiters
Monitor  queues completion events
collect  explicitly materializes or returns the resolved payload
```

Example:

```r
aios <- list(
  a = fs_read_aio(fs, "a.bin"),
  b = fs_read_aio(fs, "b.bin"),
  c = fs_stat_aio(fs, "c.bin")
)

gate <- cv()
mon <- aio_monitor(aios, cv = gate)

repeat {
  cv_until(gate, 100)
  events <- read_monitor(mon)

  if (length(events)) {
    # update progress, schedule follow-up work, inspect state
  }

  if (all(vapply(aios, function(x) x$resolved, logical(1)))) break
}

values <- lapply(aios, collect_aio)
```

Important rule: monitor events should not materialize large payloads. The Aio
owns the result. The user or native package explicitly collects.

## Range reads and bioinformatics

The current `fs_read()` range design is correct and should remain central.

Examples:

```r
fs_read(fs, "x.bin", offset = 100, size = 50)

fs_read(
  fs,
  "x.bin",
  offset = c(0, 4096, 8192),
  size = c(512, 512, 512),
  result = "flat"
)

fs_read(
  fs,
  path = c("a.bin", "b.bin"),
  offset = list(c(0, 4096), c(0)),
  size = list(c(100, 100), c(200)),
  result = "nested"
)
```

Add a small request object for range-heavy formats:

```r
req <- byte_ranges(
  path = index$path,
  offset = index$offset,
  size = index$size,
  id = index$chunk_id
)

chunks <- fs_read(fs, req, mode = "bytes", result = "flat")
```

This is especially useful for BAM/BAI, CRAM/CRAI, Tabix-indexed VCF/BED/GFF,
BigWig/BigBed, BGZF, FASTA + FAI, tiled arrays, and custom binary indexes.

The C API should remain explicit for range vectors:

```c
ropendal_readv_into_aio(fs, requests, nrequests, buffers, nbuffers, &aio, &err);
```

## ZIP as a filesystem adapter

ZIP support should be first-class, including ZIP over HTTP/S3/GCS/Azure/GDrive,
but ZIP should be modeled as a filesystem adapter over a parent filesystem, not
as a codec.

Preferred R shape:

```r
s3 <- opendal("s3", bucket = "datasets", root = "project", auth = credentials_s3(...))
zfs <- fs_zip(s3, "runs/run-001.zip")

fs_ls(zfs)
fs_stat(zfs, "qc/summary.tsv")
fs_read(zfs, "qc/summary.tsv")
```

Pipe-friendly:

```r
opendal("s3", bucket = "datasets", root = "project", auth = credentials_s3(...)) |>
  fs_zip("runs/run-001.zip") |>
  fs_read("qc/summary.tsv")
```

Do not make the primary interface a Python-like URL chain. URI sugar can come
later, but object composition should be primary.

### ZIP index

Remote ZIP needs an index loaded from the end-of-central-directory and central
directory records. Opening should be lazy by default, with explicit async index
loading when desired.

```r
zfs <- fs_zip(fs, "archive.zip", index = "lazy")

idx <- fs_zip_index(fs, "archive.zip")
zfs <- fs_zip(fs, "archive.zip", index = idx)

idx_aio <- fs_zip_index_aio(fs, "archive.zip")
idx <- collect_aio(idx_aio)
zfs <- fs_zip(fs, "archive.zip", index = idx)
```

Index cache keys should include archive path, size, etag/version if available,
last-modified if available, central-directory offset/size, ZIP64 status, and
entry table digest.

### ZIP capabilities

Start read-only.

| Operation | Initial support | Semantics |
|---|---:|---|
| `fs_ls()` | yes | central-directory index |
| `fs_stat()` | yes | central-directory metadata |
| `fs_exists()` | yes | index lookup |
| `fs_read()` | yes | stored direct range or deflate decode |
| `fs_read_bytes()` | yes | immutable bytes |
| `fs_read_iter()` | partial | direct for stored entries; streaming decode for deflate |
| `fs_write()` | no | archive rewrite required |
| `fs_replace()` | no | archive rewrite required |
| `fs_append()` | no | unsafe or backend-dependent |
| `fs_delete()` | no | archive rewrite required |
| `fs_rename()` | no | archive rewrite required |

Unsupported compression methods, encrypted entries, multi-disk archives, and
central-directory encryption should return `opendalUnsupportedValue`.

### ZIP C API

Expose ZIP as another filesystem-compatible adapter:

```c
typedef struct ropendal_zip_options {
  size_t struct_size;
  const char *archive_path;
  const char *root;
  size_t index_cache_bytes;
  size_t block_size;
  int strict_crc;
} ropendal_zip_options_t;

ropendal_status_t ropendal_fs_zip(
  ropendal_fs_t *parent,
  const ropendal_zip_options_t *opts,
  ropendal_fs_t **out,
  ropendal_error_t **err
);
```

Native consumers then use existing read/stat/list/Aio/monitor APIs against the
returned `ropendal_fs_t`.

## Byte store / chunk store

Ropendal exposes a R-native key-to-bytes store for Zarr-like layouts, but
avoids Python mapper ergonomics as the primary API. The current R API is a
prefix adapter over an existing `OpendalFs`: it is byte-only and delegates to
`fs_read()`, `fs_read_bytes()`, `fs_write()`, `fs_replace()`, `fs_exists()`,
`fs_ls()`, and `fs_delete()` for sync and Aio variants. The native C API also
exposes an opaque prefix-scoped `ropendal_store_t` with async read/write,
read-into, existence, listing, and delete operations for downstream packages
that need caller-owned buffers without R raw vectors. Native callers can also
wrap parent/cache stores with `ropendal_store_cache_open()` for explicit
full-object caching or `ropendal_store_block_cache_open()` for explicit fixed-size
range block caching. R wrappers expose `store_cache()` for local full-object
chunk-key reads and `store_block_cache()` for local fixed-size scalar
`result = "auto"` store-range reads; vectorized/non-auto shapes bypass the block
cache. Eviction/readahead policy remains intentionally unsettled and explicit.
Store reads are bytes-first and return `OpendalBytes` by default so
ALTREP-compatible materialization and C consumers can stay below R raw-vector
copies.

Current shape:

```r
store <- byte_store(fs, prefix = "array.zarr")

store_read(store, "zarr.json")        # OpendalBytes by default
as.raw(store_read(store, "zarr.json")) # explicit materialization
store_write(store, "c/0/0", chunk_raw)
store_exists(store, "c/0/0")
store_list(store, "c/", recursive = TRUE)
store_delete(store, "c/0/0")
```

Vectorized:

```r
keys <- sprintf("c/0/%d", 0:99)

chunks <- store_read(
  store,
  keys,
  batch_concurrency = 32
)

store_write(
  store,
  keys,
  chunks,
  batch_concurrency = 32
)
```

### Rarr-style Zarr consumers

A package in the style of Rarr can use `byte_store()` as its storage backend
without waiting for `fs_connection()`. For directory or object-store Zarr array
layouts, the required storage primitives are present: key normalization under a
root prefix, existence checks, byte reads, create-only writes, overwrite writes,
listing, deletion, vectorized/Aio operations, explicit full-object cache,
explicit block/range cache, `OpendalBytes`/ALTREP raw materialization, and a
native async C store API with caller-owned `read_into` buffers.

The consumer still owns format semantics above the store layer:

- Zarr v2/v3 metadata parsing and validation (`.zarray`, `zarr.json`, `.zattrs`,
  `.zmetadata`);
- chunk coordinate to key mapping and fill-value behavior for missing chunks;
- dtype conversion, endianness, dimensions, and R array materialization;
- Zarr codec/filter pipelines and any metadata-aware compressor configuration;
- partial chunk updates (`read existing or fill`, decode, patch, encode,
  `store_replace()`);
- multi-key consistency policy for metadata/chunk writes.

This separation is intentional. Ropendal is the portable async byte substrate;
a Rarr-like package is the Zarr engine. Empty hierarchy/group markers that must
exist without metadata objects may justify a future `store_mkdir()` convenience,
but array reads/writes normally only require metadata and chunk keys.

### Codec extension boundary

The codec layer is intentionally explicit and byte-first. Current built-in
native codecs are `identity`, `gzip`, and `zlib`; adding more raw-byte codecs
fits the current architecture by extending the Rust codec registry, R
`codec_config()`/`mode = "codec"` plumbing, C `ropendal_codec_encode()` /
`ropendal_codec_decode()`, docs, and roundtrip tests. Store operations should
remain byte-only and must not auto-select or interpret codecs.

Zarr codecs that are pure bytes-to-bytes transforms can either live in the
consumer package or be promoted into Ropendal when they are generally useful.
Array-to-bytes and array-to-array filters that need dtype, shape, fill, or
chunk metadata belong in the Zarr/Rarr-like consumer unless a separate typed
array layer is deliberately introduced.

ZIP-backed stores are not a free consequence of `byte_store()` alone. A read-only
ZIP adapter can map archive entries into filesystem paths and then feed
`byte_store()`:

```r
store <- opendal("s3", bucket = "datasets", root = "arrays", auth = credentials_s3(...)) |>
  fs_zip("array.zarr.zip") |>
  byte_store(prefix = "array.zarr")

meta <- store_read(store, "zarr.json") # OpendalBytes; parse explicitly above store layer
chunk <- store_read(store, "c/0/0")
```

Writable ZIP stores need archive-level transaction/finalization semantics and
are a separate adapter concern, not something the key-to-bytes store can provide
by itself.

Core Ropendal should not implement full Zarr array semantics. It should provide
the filesystem/byte-store substrate; Zarr, Arrow, Bioconductor, or geospatial
adapters can live on top.

## Suggested milestones

1. Document the opinionated architecture and keep `function(object, ...)`.
2. Stabilize `OpendalBytes` lifetime, ALTREP ownership, and C-level byte rules
   (`as.raw.OpendalBytes()` now returns a first-class raw ALTREP facade with
   region access and conservative pointer materialization).
3. Profile whether read-only contiguous `DATAPTR()` should avoid materializing
   after the current safer ALTREP path has downstream coverage.
4. Add `byte_ranges()` request objects feeding `fs_read()` (implemented for R read wrappers).
5. Add `fs_connection()` backed by read/write iterators.
6. Add read-only `fs_zip()` over any range-readable parent filesystem.
7. Add `ropendal_fs_zip()` to the C API.
8. Add `byte_store()` / `chunk_store()` with vectorized store operations (`byte_store()` implemented for R sync and Aio byte operations; `ropendal_store_t` implemented for native C byte operations).
9. Add explicit memory/file block cache adapter once invalidation is designed (`store_cache()` and `ropendal_store_cache_open()` implement explicit full-object caches for byte stores; `ropendal_store_block_cache_open()` and `store_block_cache()` implement fixed-size range block caches, with eviction/readahead policy still future work).
10. Build targeted integrations: BioC range readers, Arrow/Parquet, and Zarr-like
    chunk stores (pure-R Zarr-like and indexed VCF-like vignettes now demonstrate the substrate boundaries).
