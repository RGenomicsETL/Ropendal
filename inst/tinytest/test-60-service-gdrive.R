library(Ropendal)

helper <- system.file("tinytest", "helper-ropendal.R", package = "Ropendal")
source(helper, local = TRUE)

if (!ropendal_service_tests_enabled("gdrive")) exit_file("set ROPENDAL_TEST_NETWORK=true and ROPENDAL_TEST_GDRIVE=true to run")

secret_json <- Sys.getenv("ROPENDAL_GDRIVE_SECRET_JSON", unset = "")
tokens_json <- Sys.getenv("ROPENDAL_GDRIVE_TOKENS_JSON", unset = "")
gdrive_root <- Sys.getenv("ROPENDAL_GDRIVE_ROOT", unset = Sys.getenv("ROPENDAL_TEST_GDRIVE_ROOT", unset = ""))
gdrive_file <- Sys.getenv("ROPENDAL_GDRIVE_FILE", unset = "map_catalog.txt")

missing <- ropendal_missing_env(c("ROPENDAL_GDRIVE_SECRET_JSON", "ROPENDAL_GDRIVE_ROOT"))
if (length(missing) > 0L) exit_file(paste("missing env vars:", paste(missing, collapse = ", ")))
if (!file.exists(secret_json)) exit_file("ROPENDAL_GDRIVE_SECRET_JSON does not exist")
if (!nzchar(tokens_json)) tokens_json <- file.path(dirname(secret_json), "tokens.json")
if (!file.exists(tokens_json)) exit_file("ROPENDAL_GDRIVE_TOKENS_JSON does not exist")

auth <- credentials_gdrive3(secret_json = secret_json, tokens_json = tokens_json)
fs <- opendal("gdrive", root = gdrive_root, auth = auth)

entries <- fs_ls(fs)
expect_false(is_error_value(entries))
paths <- vapply(entries, `[[`, character(1), "path")
expect_true(gdrive_file %in% paths)

stat <- fs_stat(fs, gdrive_file)
expect_false(is_error_value(stat))
expect_equal(stat$type, "file")
expect_true(stat$size > 0)

head <- fs_read(fs, gdrive_file, offset = 0, size = min(64, stat$size))
expect_true(is.raw(head))
expect_true(length(head) > 0)

mib <- 1024L * 1024L
resumable_path <- paste0("ropendal-resumable-", Sys.getpid(), ".bin")
block <- as.raw(rep.int(0:255, mib / 256L))
writer <- fs_write_iter(fs, resumable_path, chunk_size = 256 * 1024)
for (i in seq_len(64L)) {
  expect_true(identical(write_iter_write(writer, block), TRUE))
}
expect_true(identical(write_iter_close(writer), TRUE))
expect_equal(fs_stat(fs, resumable_path)$size, 64 * mib)
expect_equal(
  fs_read(fs, resumable_path, offset = 256L * 1024L - 2L, size = 4L),
  as.raw(c(254, 255, 0, 1))
)
expect_equal(
  fs_read(fs, resumable_path, offset = mib - 2L, size = 4L),
  as.raw(c(254, 255, 0, 1))
)
expect_equal(
  fs_read(fs, resumable_path, offset = 64 * mib - 2L, size = 2L),
  as.raw(c(254, 255))
)
create_again <- fs_write(fs, resumable_path, as.raw(1))
expect_true(is_error_value(create_again))
expect_equal(error_kind(create_again), "AlreadyExists")
expect_true(identical(fs_replace(fs, resumable_path, as.raw(c(9, 8))), TRUE))
expect_equal(fs_read(fs, resumable_path), as.raw(c(9, 8)))
expect_true(identical(fs_delete(fs, resumable_path), TRUE))
expect_false(fs_exists(fs, resumable_path))

empty_path <- paste0("ropendal-resumable-empty-", Sys.getpid(), ".bin")
empty_writer <- fs_write_iter(fs, empty_path)
expect_true(identical(write_iter_close(empty_writer), TRUE))
expect_equal(fs_stat(fs, empty_path)$size, 0)
expect_true(identical(fs_delete(fs, empty_path), TRUE))
