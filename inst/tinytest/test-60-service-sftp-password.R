library(Ropendal)

helper <- system.file("tinytest", "helper-ropendal.R", package = "Ropendal")
source(helper, local = TRUE)

if (!ropendal_service_tests_enabled("sftp_password")) exit_file("set ROPENDAL_TEST_NETWORK=true and ROPENDAL_TEST_SFTP_PASSWORD=true to run")

required <- c(
  "ROPENDAL_SFTP_ENDPOINT",
  "ROPENDAL_SFTP_ROOT",
  "ROPENDAL_SFTP_PASSWORD_USER",
  "ROPENDAL_SFTP_PASSWORD"
)
missing <- ropendal_missing_env(required)
if (length(missing)) exit_file(paste("missing env vars:", paste(missing, collapse = ", ")))

fs <- opendal(
  "sftp",
  endpoint = Sys.getenv("ROPENDAL_SFTP_ENDPOINT"),
  root = Sys.getenv("ROPENDAL_SFTP_ROOT"),
  user = Sys.getenv("ROPENDAL_SFTP_PASSWORD_USER"),
  password = Sys.getenv("ROPENDAL_SFTP_PASSWORD"),
  known_hosts_strategy = Sys.getenv(
    "ROPENDAL_SFTP_PASSWORD_KNOWN_HOSTS_STRATEGY",
    unset = Sys.getenv("ROPENDAL_SFTP_KNOWN_HOSTS_STRATEGY", unset = "strict")
  )
)

expect_equal(fs_info(fs)$scheme, "sftp")
caps <- fs_capabilities(fs)
expect_true(caps$operations$read$supported)
expect_true(caps$operations$write$supported)
expect_true(caps$operations$append$supported)
expect_true(caps$operations$mkdir$supported)
expect_true(caps$operations$delete$supported)
expect_true(caps$operations$copy$supported)
expect_true(caps$operations$rename$supported)
expect_true(caps$operations$ls$supported)

prefix <- basename(tempfile("ropendal-sftp-password-"))
path <- paste0(prefix, "-source.bin")
copy_path <- paste0(prefix, "-copy.bin")
renamed_path <- paste0(prefix, "-renamed.bin")
async_path <- paste0(prefix, "-async.bin")
dir_path <- paste0(prefix, "-empty")
bytes <- as.raw(c(1, 2, 3, 4, 5, 6))

expect_true(identical(fs_write(fs, path, bytes), TRUE))
expect_equal(fs_read(fs, path), bytes)
expect_equal(fs_read(fs, path, offset = 2, size = 3), as.raw(c(3, 4, 5)))
expect_true(identical(fs_append(fs, path, as.raw(c(7, 8))), TRUE))
expect_equal(fs_read(fs, path), as.raw(c(1, 2, 3, 4, 5, 6, 7, 8)))

existing <- fs_write(fs, path, as.raw(9))
expect_true(is_error_value(existing))
expect_equal(error_kind(existing), "AlreadyExists")

expect_true(identical(fs_copy(fs, path, copy_path), TRUE))
expect_true(identical(fs_rename(fs, copy_path, renamed_path), TRUE))
expect_equal(fs_read(fs, renamed_path), fs_read(fs, path))

entries <- fs_ls(fs)
expect_false(is_error_value(entries))
paths <- vapply(entries, `[[`, character(1), "path")
expect_true(path %in% paths)
expect_true(renamed_path %in% paths)

expect_true(identical(fs_mkdir(fs, dir_path), TRUE))
expect_true(identical(fs_delete(fs, dir_path, recursive = TRUE), TRUE))

async_bytes <- as.raw(c(10, 11, 12, 13))
expect_true(identical(collect_aio(fs_write_aio(fs, async_path, async_bytes)), TRUE))
expect_equal(collect_aio(fs_read_aio(fs, async_path, offset = 1, size = 2)), as.raw(c(11, 12)))
expect_true(collect_aio(fs_exists_aio(fs, async_path)))
expect_true(identical(collect_aio(fs_delete_aio(fs, async_path)), TRUE))
expect_true(identical(fs_delete(fs, renamed_path), TRUE))
expect_true(identical(fs_delete(fs, path), TRUE))
expect_false(fs_exists(fs, async_path))
expect_false(fs_exists(fs, renamed_path))
expect_false(fs_exists(fs, path))
