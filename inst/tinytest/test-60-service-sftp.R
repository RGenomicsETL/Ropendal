library(Ropendal)

helper <- system.file("tinytest", "helper-ropendal.R", package = "Ropendal")
source(helper, local = TRUE)

if (.Platform$OS.type == "windows") exit_file("OpenDAL SFTP is supported on Unix only")
if (!ropendal_service_tests_enabled("sftp")) exit_file("set ROPENDAL_TEST_NETWORK=true and ROPENDAL_TEST_SFTP=true to run")

required <- c(
  "ROPENDAL_SFTP_ENDPOINT",
  "ROPENDAL_SFTP_ROOT",
  "ROPENDAL_SFTP_USER",
  "ROPENDAL_SFTP_KEY"
)
missing <- ropendal_missing_env(required)
if (length(missing)) exit_file(paste("missing env vars:", paste(missing, collapse = ", ")))

key <- Sys.getenv("ROPENDAL_SFTP_KEY")
if (!file.exists(key)) exit_file("ROPENDAL_SFTP_KEY does not exist")

fs <- opendal(
  "sftp",
  endpoint = Sys.getenv("ROPENDAL_SFTP_ENDPOINT"),
  root = Sys.getenv("ROPENDAL_SFTP_ROOT"),
  user = Sys.getenv("ROPENDAL_SFTP_USER"),
  key = key,
  known_hosts_strategy = Sys.getenv("ROPENDAL_SFTP_KNOWN_HOSTS_STRATEGY", unset = "strict")
)

expect_equal(fs_info(fs)$scheme, "sftp")
sftp_caps <- fs_capabilities(fs)
expect_true(inherits(sftp_caps, "opendalCapabilityValue"))
expect_true(sftp_caps$operations$stat$supported)
expect_true(sftp_caps$operations$read$supported)
expect_true(sftp_caps$operations$write$supported)
expect_true(sftp_caps$operations$mkdir$supported)
expect_true(sftp_caps$operations$delete$supported)
expect_true(sftp_caps$operations$copy$supported)
expect_true(sftp_caps$operations$rename$supported)
expect_true(sftp_caps$operations$ls$supported)

prefix <- basename(tempfile("ropendal-sftp-"))
sync_path <- paste0(prefix, "-sync.bin")
copy_path <- paste0(prefix, "-copy.bin")
renamed_path <- paste0(prefix, "-renamed.bin")
async_path <- paste0(prefix, "-async.bin")
async_copy_path <- paste0(prefix, "-async-copy.bin")
async_renamed_path <- paste0(prefix, "-async-renamed.bin")
dir_path <- paste0(prefix, "-empty")

bytes <- as.raw(c(1, 2, 3, 4, 5, 6))
expect_true(identical(fs_write(fs, sync_path, bytes), TRUE))
expect_equal(fs_read(fs, sync_path), bytes)
expect_equal(fs_read(fs, sync_path, offset = 2, size = 3), as.raw(c(3, 4, 5)))

again <- fs_write(fs, sync_path, as.raw(9))
expect_true(is_error_value(again))
expect_equal(error_kind(again), "AlreadyExists")

stat <- fs_stat(fs, sync_path)
expect_false(is_error_value(stat))
expect_equal(stat$type, "file")
expect_equal(stat$size, length(bytes))
expect_true(fs_exists(fs, sync_path))
expect_false(fs_exists(fs, paste0(prefix, "-missing.bin")))

expect_true(identical(fs_copy(fs, sync_path, copy_path), TRUE))
expect_equal(fs_read(fs, copy_path), bytes)
expect_true(identical(fs_rename(fs, copy_path, renamed_path), TRUE))
expect_equal(fs_read(fs, renamed_path), bytes)

entries <- fs_ls(fs)
expect_false(is_error_value(entries))
paths <- vapply(entries, `[[`, character(1), "path")
expect_true(sync_path %in% paths)
expect_true(renamed_path %in% paths)

expect_true(identical(fs_mkdir(fs, dir_path), TRUE))
expect_true(identical(fs_delete(fs, dir_path, recursive = TRUE), TRUE))

async_bytes <- as.raw(c(10, 11, 12, 13))
expect_true(identical(collect_aio(fs_write_aio(fs, async_path, async_bytes)), TRUE))
expect_equal(collect_aio(fs_read_aio(fs, async_path, offset = 1, size = 2)), as.raw(c(11, 12)))
async_stat <- collect_aio(fs_stat_aio(fs, async_path))
expect_false(is_error_value(async_stat))
expect_equal(async_stat$size, length(async_bytes))
expect_true(collect_aio(fs_exists_aio(fs, async_path)))
expect_true(identical(collect_aio(fs_copy_aio(fs, async_path, async_copy_path)), TRUE))
expect_true(identical(collect_aio(fs_rename_aio(fs, async_copy_path, async_renamed_path)), TRUE))
expect_equal(fs_read(fs, async_renamed_path), async_bytes)

expect_true(identical(collect_aio(fs_delete_aio(fs, async_renamed_path)), TRUE))
expect_true(identical(collect_aio(fs_delete_aio(fs, async_path)), TRUE))
expect_true(identical(fs_delete(fs, renamed_path), TRUE))
expect_true(identical(fs_delete(fs, sync_path), TRUE))
expect_false(fs_exists(fs, async_renamed_path))
expect_false(fs_exists(fs, async_path))
expect_false(fs_exists(fs, renamed_path))
expect_false(fs_exists(fs, sync_path))
