Development benchmark: Ropendal vs paws on SeaweedFS
================

This is a development-only benchmark. It starts a local SeaweedFS service,
writes and reads one raw payload through Ropendal and `paws.storage`,
and reports `bench::mark()` results. The benchmark is intentionally
outside the package build.

``` r
library(bench)
library(processx)
library(paws.storage)
library(Ropendal)

free_port <- function() {
  code <- paste(
    "import socket",
    "s=socket.socket()",
    "s.bind(('127.0.0.1',0))",
    "print(s.getsockname()[1])",
    "s.close()",
    sep = ";"
  )
  as.integer(system2("python3", c("-c", shQuote(code)), stdout = TRUE))
}

start_seaweedfs <- function() {
  if (!requireNamespace("processx", quietly = TRUE)) {
    stop("processx is required for the SeaweedFS part of this benchmark", call. = FALSE)
  }
  weed_bin <- Sys.which("weed")
  if (!nzchar(weed_bin)) stop("weed binary is required", call. = FALSE)

  data_dir <- tempfile("ropendal-seaweedfs-bench-data-")
  dir.create(data_dir, recursive = TRUE)
  log_file <- tempfile("ropendal-seaweedfs-bench-log-")
  ports <- integer(0)
  while (length(ports) < 5L) {
    port <- free_port()
    if (!port %in% ports) ports <- c(ports, port)
  }
  endpoint <- sprintf("http://127.0.0.1:%d", ports[[1L]])
  bucket <- "ropendal-bench"
  access_key <- "ropendal-bench"
  secret_key <- "ropendal-bench-secret"

  config_file <- file.path(data_dir, "s3-identities.json")
  writeLines(sprintf(
    paste0(
      '{"identities":[{"name":"%s","credentials":[{"accessKey":"%s","secretKey":"%s"}],',
      '"actions":["Admin","Read","Write","List","Tagging"]}]}'
    ),
    access_key, access_key, secret_key
  ), config_file)

  proc <- processx::process$new(
    weed_bin,
    c(
      "mini",
      paste0("-dir=", data_dir),
      "-ip=127.0.0.1",
      "-ip.bind=127.0.0.1",
      paste0("-master.port=", ports[[2L]]),
      paste0("-filer.port=", ports[[3L]]),
      paste0("-volume.port=", ports[[4L]]),
      paste0("-admin.port=", ports[[5L]]),
      "-admin.port.legacy=0",
      paste0("-s3.port=", ports[[1L]]),
      paste0("-s3.config=", config_file),
      paste0("-bucket=", bucket)
    ),
    stdout = log_file,
    stderr = log_file
  )

  ready <- FALSE
  for (i in seq_len(120)) {
    ready <- tryCatch({
      close(socketConnection("127.0.0.1", ports[[1L]], open = "r", blocking = TRUE, timeout = 1))
      TRUE
    }, error = function(e) FALSE, warning = function(w) FALSE)
    if (ready) break
    if (!proc$is_alive()) stop("SeaweedFS exited before becoming ready; see ", log_file, call. = FALSE)
    Sys.sleep(0.25)
  }
  if (!ready) stop("SeaweedFS did not become ready; see ", log_file, call. = FALSE)

  list(
    proc = proc,
    data_dir = data_dir,
    log_file = log_file,
    endpoint = endpoint,
    bucket = bucket,
    access_key = access_key,
    secret_key = secret_key,
    region = "us-east-1"
  )
}

seaweedfs <- start_seaweedfs()
cleanup_seaweedfs <- function() {
  try(seaweedfs$proc$kill(), silent = TRUE)
  unlink(seaweedfs$data_dir, recursive = TRUE)
  unlink(seaweedfs$log_file)
}

root <- paste0("bench-", Sys.getpid())
key <- "payload.bin"
paws_key <- paste(root, key, sep = "/")
payload_size <- 8 * 1024 * 1024
set.seed(1)
payload <- as.raw(sample.int(256L, payload_size, replace = TRUE) - 1L)

auth <- credentials_s3(
  access_key_id = seaweedfs$access_key,
  secret_access_key = seaweedfs$secret_key,
  region = seaweedfs$region,
  source = "seaweedfs-benchmark"
)

fs <- opendal(
  "s3",
  endpoint = seaweedfs$endpoint,
  bucket = seaweedfs$bucket,
  root = root,
  region = seaweedfs$region,
  disable_config_load = TRUE,
  disable_ec2_metadata = TRUE,
  auth = auth
)

old_aws_env <- Sys.getenv(c("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_REGION"), unset = NA)
restore_aws_env <- function() {
  for (nm in names(old_aws_env)) {
    if (is.na(old_aws_env[[nm]])) {
      Sys.unsetenv(nm)
    } else {
      do.call(Sys.setenv, as.list(stats::setNames(old_aws_env[[nm]], nm)))
    }
  }
}
Sys.setenv(
  AWS_ACCESS_KEY_ID = seaweedfs$access_key,
  AWS_SECRET_ACCESS_KEY = seaweedfs$secret_key,
  AWS_REGION = seaweedfs$region
)

s3 <- paws.storage::s3(
  endpoint = seaweedfs$endpoint,
  region = seaweedfs$region,
  config = list(s3_force_path_style = TRUE)
)

# Warm up both clients and leave the payload available for download timings.
invisible(fs_replace(fs, key, payload, write_concurrency = 4, chunk_size = 5 * 1024 * 1024))
if (!seaweedfs$proc$is_alive()) {
  if (file.exists(seaweedfs$log_file)) cat(readLines(seaweedfs$log_file), sep = "\n")
  stop("seaweedfs exited during Ropendal warmup; see ", seaweedfs$log_file, call. = FALSE)
}
invisible(s3$put_object(Bucket = seaweedfs$bucket, Key = paws_key, Body = payload))

bench::mark(
  ropendal_replace = fs_replace(fs, key, payload, write_concurrency = 4, chunk_size = 5 * 1024 * 1024),
  paws_put = {
    s3$put_object(Bucket = seaweedfs$bucket, Key = paws_key, Body = payload)
    TRUE
  },
  iterations = 5,
  check = FALSE
)[, c("expression", "min", "median", "itr/sec", "mem_alloc", "n_gc")]
#> # A tibble: 2 × 5
#>   expression            min   median `itr/sec` mem_alloc
#>   <bch:expr>       <bch:tm> <bch:tm>     <dbl> <bch:byt>
#> 1 ropendal_replace   21.5ms   23.8ms      38.8        0B
#> 2 paws_put           75.9ms   79.4ms      12.5    8.16MB

bench::mark(
  ropendal_read = fs_read(fs, key, read_concurrency = 4, chunk_size = 1024 * 1024),
  ropendal_read_aio = collect_aio(fs_read_aio(fs, key, read_concurrency = 4, chunk_size = 1024 * 1024)),
  paws_get = s3$get_object(Bucket = seaweedfs$bucket, Key = paws_key)$Body,
  iterations = 5,
  check = FALSE
)[, c("expression", "min", "median", "itr/sec", "mem_alloc", "n_gc")]
#> # A tibble: 3 × 5
#>   expression             min   median `itr/sec` mem_alloc
#>   <bch:expr>        <bch:tm> <bch:tm>     <dbl> <bch:byt>
#> 1 ropendal_read       6.12ms   6.63ms     153.     8.01MB
#> 2 ropendal_read_aio   6.19ms   6.63ms     154.     8.06MB
#> 3 paws_get           11.52ms  12.16ms      83.5    8.29MB

restore_aws_env()
cleanup_seaweedfs()
```
