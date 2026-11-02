// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::sync::Arc;

use bytes::{Buf, BytesMut};
use http::header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LOCATION, RANGE};
use http::{Method, Request, Response, StatusCode};
use opendal_core::raw::*;
use opendal_core::*;
use serde::{Deserialize, Serialize};

use super::core::GdriveCore;
use super::error::parse_error;

const RESUMABLE_CHUNK_SIZE: usize = 256 * 1024;
const MAX_RETRIES: usize = 3;
const X_UPLOAD_CONTENT_TYPE: &str = "x-upload-content-type";

pub struct GdriveWriter {
    core: Arc<GdriveCore>,
    path: String,
    file_id: Option<String>,
    session: Option<String>,
    state: UploadState,
    finished: bool,
    aborted: bool,
}

impl GdriveWriter {
    pub fn new(core: Arc<GdriveCore>, path: String, file_id: Option<String>) -> Self {
        Self {
            core,
            path,
            file_id,
            session: None,
            state: UploadState::default(),
            finished: false,
            aborted: false,
        }
    }

    async fn ensure_session(&mut self) -> Result<()> {
        if self.session.is_some() {
            return Ok(());
        }

        let mut refreshed_overwrite = false;
        'select_request: loop {
            let overwriting = self.file_id.is_some();
            let (method, url, body) = if let Some(file_id) = &self.file_id {
                (
                    Method::PATCH,
                    format!(
                        "https://www.googleapis.com/upload/drive/v3/files/{file_id}?uploadType=resumable&supportsAllDrives=true&fields=id"
                    ),
                    Vec::new(),
                )
            } else {
                let parent = get_parent(&self.path);
                let parent_id = self.core.ensure_dir(parent).await?;
                let name = get_basename(&self.path).to_string();
                let body = serde_json::to_vec(&UploadRequest {
                    name,
                    parents: vec![parent_id],
                })
                .map_err(new_json_serialize_error)?;

                (
                    Method::POST,
                    "https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable&supportsAllDrives=true&fields=id".to_string(),
                    body,
                )
            };

            for attempt in 0..=MAX_RETRIES {
                let mut request = Request::builder()
                    .method(method.clone())
                    .uri(&url)
                    .header(CONTENT_LENGTH, body.len())
                    .header(X_UPLOAD_CONTENT_TYPE, "application/octet-stream")
                    .extension(Operation::Write);
                if !body.is_empty() {
                    request = request.header(CONTENT_TYPE, "application/json; charset=UTF-8");
                }
                let req = request
                    .body(Buffer::from(body.clone()))
                    .map_err(|err| request_build_error("initiate resumable upload", err))?;

                match self.send(req).await {
                    Ok(resp) if matches!(resp.status(), StatusCode::OK | StatusCode::CREATED) => {
                        let location = resp
                            .headers()
                            .get(LOCATION)
                            .ok_or_else(|| {
                                unexpected("resumable upload response has no Location header")
                            })?
                            .to_str()
                            .map_err(|err| {
                                unexpected("resumable upload Location header is invalid")
                                    .set_source(err)
                            })?
                            .to_string();
                        self.session = Some(location);
                        return Ok(());
                    }
                    Ok(resp)
                        if resp.status() == StatusCode::NOT_FOUND
                            && overwriting
                            && !refreshed_overwrite =>
                    {
                        refreshed_overwrite = true;
                        self.core.refresh_path(&self.path).await;
                        self.file_id = self.core.resolve_path(&self.path).await?;
                        continue 'select_request;
                    }
                    Ok(resp) if is_retryable_status(resp.status()) && attempt < MAX_RETRIES => {}
                    Ok(resp) => return Err(parse_error(resp).set_persistent()),
                    Err(err) if err.is_temporary() && attempt < MAX_RETRIES => {}
                    Err(err) => return Err(err.set_persistent()),
                }
            }

            return Err(
                unexpected("resumable upload initiation retry limit exceeded").set_persistent(),
            );
        }
    }

    async fn send(&self, mut req: Request<Buffer>) -> Result<Response<Buffer>> {
        self.core.sign(&mut req).await?;
        self.core.info.http_client().send(req).await
    }

    fn invalidate(&mut self) {
        self.session = None;
        self.state = UploadState::default();
        self.aborted = true;
    }

    fn invalidate_with<T>(&mut self, err: Error) -> Result<T> {
        self.invalidate();
        Err(err)
    }

    fn build_upload_request(&self, final_payload: bool) -> Result<Request<Buffer>> {
        let session = self
            .session
            .as_deref()
            .ok_or_else(|| unexpected("resumable upload session is not initialized"))?;
        let payload_len = self.state.pending.len();

        if !final_payload && payload_len != RESUMABLE_CHUNK_SIZE {
            return Err(unexpected(
                "non-final resumable upload payload is not aligned to 256 KiB",
            ));
        }

        let content_range = upload_content_range(self.state.committed, payload_len, final_payload)?;
        let payload = self.state.pending.clone().freeze();
        let mut request = Request::builder()
            .method(Method::PUT)
            .uri(session)
            .header(CONTENT_TYPE, "application/octet-stream")
            .header(CONTENT_LENGTH, payload_len)
            .extension(Operation::Write);
        if let Some(content_range) = content_range {
            request = request.header(CONTENT_RANGE, content_range);
        }

        request
            .body(Buffer::from(payload))
            .map_err(|err| request_build_error("upload resumable payload", err))
    }

    fn build_status_request(&self, total: Option<u64>) -> Result<Request<Buffer>> {
        let session = self
            .session
            .as_deref()
            .ok_or_else(|| unexpected("resumable upload session is not initialized"))?;

        let content_range = match total {
            Some(total) => format!("bytes */{total}"),
            None => "bytes */*".to_string(),
        };

        Request::builder()
            .method(Method::PUT)
            .uri(session)
            .header(CONTENT_LENGTH, 0)
            .header(CONTENT_RANGE, content_range)
            .extension(Operation::Write)
            .body(Buffer::new())
            .map_err(|err| request_build_error("query resumable upload", err))
    }

    async fn query_session(&self, total: Option<u64>) -> Result<SessionStatus> {
        for attempt in 0..=MAX_RETRIES {
            let req = self.build_status_request(total)?;
            match self.send(req).await {
                Ok(resp) if resp.status().as_u16() == 308 => {
                    return Ok(SessionStatus::Incomplete(parse_308_committed(
                        resp.headers().get(RANGE),
                    )?));
                }
                Ok(resp) if matches!(resp.status(), StatusCode::OK | StatusCode::CREATED) => {
                    return Ok(SessionStatus::Complete(resp));
                }
                Ok(resp) if is_retryable_status(resp.status()) && attempt < MAX_RETRIES => {}
                Ok(resp) => return Err(parse_error(resp).set_persistent()),
                Err(err) if err.is_temporary() && attempt < MAX_RETRIES => {}
                Err(err) => return Err(err.set_persistent()),
            }
        }

        Err(unexpected("resumable upload status retry limit exceeded").set_persistent())
    }

    fn apply_incomplete(&mut self, resp: &Response<Buffer>) -> Result<()> {
        let committed = parse_308_committed(resp.headers().get(RANGE))?;
        self.state.acknowledge(committed)
    }

    async fn reconcile_after_failure(
        &mut self,
        final_payload: bool,
    ) -> Result<Option<Response<Buffer>>> {
        let total = final_payload.then_some(self.state.pending_end());
        let status = match self.query_session(total).await {
            Ok(status) => status,
            Err(err) => return self.invalidate_with(err),
        };
        match status {
            SessionStatus::Incomplete(committed) => {
                if let Err(err) = self.state.acknowledge(committed) {
                    return self.invalidate_with(err);
                }
                Ok(None)
            }
            SessionStatus::Complete(resp) if final_payload => {
                if let Err(err) = self.state.acknowledge(self.state.pending_end()) {
                    return self.invalidate_with(err);
                }
                Ok(Some(resp))
            }
            SessionStatus::Complete(_) => self.invalidate_with(
                unexpected("resumable upload completed before the final payload").set_persistent(),
            ),
        }
    }

    async fn upload_pending(&mut self, final_payload: bool) -> Result<Option<Response<Buffer>>> {
        self.ensure_session().await?;

        for attempt in 0..=MAX_RETRIES {
            let req = self.build_upload_request(final_payload)?;
            match self.send(req).await {
                Ok(resp) if matches!(resp.status(), StatusCode::OK | StatusCode::CREATED) => {
                    if !final_payload {
                        return self.invalidate_with(
                            unexpected("resumable upload completed before the final payload")
                                .set_persistent(),
                        );
                    }
                    if let Err(err) = self.state.acknowledge(self.state.pending_end()) {
                        return self.invalidate_with(err);
                    }
                    return Ok(Some(resp));
                }
                Ok(resp) if resp.status().as_u16() == 308 => {
                    let committed_before = self.state.committed;
                    if let Err(err) = self.apply_incomplete(&resp) {
                        return self.invalidate_with(err);
                    }
                    if !final_payload && self.state.committed > committed_before {
                        return Ok(None);
                    }
                    if attempt == MAX_RETRIES {
                        return self.invalidate_with(
                            unexpected(
                                "resumable upload made incomplete progress after retry limit",
                            )
                            .set_persistent(),
                        );
                    }
                }
                Ok(resp) if is_retryable_status(resp.status()) => {
                    if attempt == MAX_RETRIES {
                        let err = parse_error(resp).set_persistent();
                        return self.invalidate_with(err);
                    }
                    let committed_before = self.state.committed;
                    if let Some(resp) = self.reconcile_after_failure(final_payload).await? {
                        return Ok(Some(resp));
                    }
                    if !final_payload && self.state.committed > committed_before {
                        return Ok(None);
                    }
                }
                Ok(resp) => {
                    let err = parse_error(resp).set_persistent();
                    return self.invalidate_with(err);
                }
                Err(err) => {
                    if !err.is_temporary() || attempt == MAX_RETRIES {
                        return self.invalidate_with(err.set_persistent());
                    }
                    let committed_before = self.state.committed;
                    if let Some(resp) = self.reconcile_after_failure(final_payload).await? {
                        return Ok(Some(resp));
                    }
                    if !final_payload && self.state.committed > committed_before {
                        return Ok(None);
                    }
                }
            }
        }

        self.invalidate_with(unexpected("resumable upload retry limit exceeded").set_persistent())
    }

    async fn finish(&mut self, resp: Response<Buffer>) -> Result<()> {
        let body = resp.into_body();
        let file_id = if body.is_empty() {
            match &self.file_id {
                Some(file_id) => file_id.clone(),
                None => self
                    .core
                    .resolve_path_after_refresh(&self.path)
                    .await?
                    .ok_or_else(|| {
                        unexpected("completed resumable upload has no file identifier")
                    })?,
            }
        } else {
            let file: GdriveFile =
                serde_json::from_slice(&body.to_bytes()).map_err(new_json_deserialize_error)?;
            file.id
        };

        self.core.cache_file_id(&self.path, &file_id).await;
        self.core
            .record_recent_upsert(
                &self.path,
                Metadata::new(EntryMode::FILE).with_content_length(self.state.committed),
            )
            .await;
        self.file_id = Some(file_id);
        self.finished = true;
        self.session = None;
        Ok(())
    }
}

impl oio::Write for GdriveWriter {
    async fn write(&mut self, mut bs: Buffer) -> Result<()> {
        if self.finished || self.aborted {
            return Err(unexpected("resumable upload writer is not writable"));
        }

        while bs.has_remaining() {
            if self.state.pending.len() == RESUMABLE_CHUNK_SIZE {
                self.upload_pending(false).await?;
            }
            let accepted = self.state.accept(&bs);
            bs.advance(accepted);
        }

        Ok(())
    }

    async fn close(&mut self) -> Result<Metadata> {
        if self.finished {
            return Ok(Metadata::default());
        }
        if self.aborted {
            return Err(unexpected("resumable upload writer was aborted"));
        }

        let resp = self
            .upload_pending(true)
            .await?
            .ok_or_else(|| unexpected("final resumable upload did not complete"))?;
        self.finish(resp).await?;
        Ok(Metadata::default())
    }

    async fn abort(&mut self) -> Result<()> {
        self.invalidate();
        Ok(())
    }
}

#[derive(Default)]
struct UploadState {
    committed: u64,
    pending: BytesMut,
}

impl UploadState {
    fn pending_end(&self) -> u64 {
        self.committed + self.pending.len() as u64
    }

    fn accept(&mut self, bs: &Buffer) -> usize {
        let available = RESUMABLE_CHUNK_SIZE - self.pending.len();
        let accepted = available.min(bs.len());
        self.pending
            .extend_from_slice(&bs.slice(..accepted).to_bytes());
        accepted
    }

    fn acknowledge(&mut self, committed: u64) -> Result<()> {
        if committed < self.committed || committed > self.pending_end() {
            return Err(
                unexpected("resumable upload committed offset is out of range")
                    .with_context("current", self.committed)
                    .with_context("reported", committed)
                    .with_context("pending_end", self.pending_end()),
            );
        }

        let accepted = (committed - self.committed) as usize;
        self.pending.advance(accepted);
        self.committed = committed;
        Ok(())
    }
}

enum SessionStatus {
    Incomplete(u64),
    Complete(Response<Buffer>),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadRequest {
    name: String,
    parents: Vec<String>,
}

#[derive(Deserialize)]
struct GdriveFile {
    id: String,
}

fn upload_content_range(start: u64, len: usize, final_payload: bool) -> Result<Option<String>> {
    // Drive finalizes an empty file with an empty PUT; `bytes */0` is a status query.
    if final_payload && start == 0 && len == 0 {
        return Ok(None);
    }
    let total = final_payload.then_some(
        start
            .checked_add(len as u64)
            .ok_or_else(|| unexpected("resumable upload total size overflow"))?,
    );
    format_content_range(start, len, total).map(Some)
}

fn format_content_range(start: u64, len: usize, total: Option<u64>) -> Result<String> {
    if len == 0 {
        return Ok(match total {
            Some(total) => format!("bytes */{total}"),
            None => "bytes */*".to_string(),
        });
    }

    let end = start
        .checked_add(len as u64)
        .and_then(|value| value.checked_sub(1))
        .ok_or_else(|| unexpected("resumable upload Content-Range overflow"))?;

    Ok(match total {
        Some(total) => format!("bytes {start}-{end}/{total}"),
        None => format!("bytes {start}-{end}/*"),
    })
}

fn parse_308_committed(range: Option<&http::HeaderValue>) -> Result<u64> {
    let Some(range) = range else {
        return Ok(0);
    };
    let range = range
        .to_str()
        .map_err(|err| unexpected("resumable upload Range header is invalid").set_source(err))?;
    let value = range
        .strip_prefix("bytes=0-")
        .ok_or_else(|| unexpected("resumable upload Range header has invalid syntax"))?;
    let end = value
        .parse::<u64>()
        .map_err(|err| unexpected("resumable upload Range end is invalid").set_source(err))?;
    end.checked_add(1)
        .ok_or_else(|| unexpected("resumable upload committed offset overflow"))
}

fn is_retryable_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
    ) || status.is_server_error()
}

fn request_build_error(operation: &'static str, err: http::Error) -> Error {
    unexpected("failed to build Google Drive request")
        .with_operation(operation)
        .set_source(err)
}

fn unexpected(message: &'static str) -> Error {
    Error::new(ErrorKind::Unexpected, message).with_operation("GdriveWriter")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_resumable_content_ranges() {
        assert_eq!(
            format_content_range(0, RESUMABLE_CHUNK_SIZE, None).unwrap(),
            "bytes 0-262143/*"
        );
        assert_eq!(
            format_content_range(RESUMABLE_CHUNK_SIZE as u64, 17, Some(262_161)).unwrap(),
            "bytes 262144-262160/262161"
        );
        assert_eq!(format_content_range(0, 0, Some(0)).unwrap(), "bytes */0");
        assert_eq!(format_content_range(0, 0, None).unwrap(), "bytes */*");
        assert_eq!(upload_content_range(0, 0, true).unwrap(), None);
    }

    #[test]
    fn parses_resumable_308_range() {
        let range = http::HeaderValue::from_static("bytes=0-262143");
        assert_eq!(parse_308_committed(Some(&range)).unwrap(), 262_144);
        assert_eq!(parse_308_committed(None).unwrap(), 0);

        let malformed = http::HeaderValue::from_static("bytes=1-262143");
        assert!(parse_308_committed(Some(&malformed)).is_err());
    }

    #[test]
    fn state_retains_one_chunk_and_reconciles_partial_commits() {
        let mut state = UploadState::default();
        let input = Buffer::from(vec![7; RESUMABLE_CHUNK_SIZE + 17]);

        assert_eq!(state.accept(&input), RESUMABLE_CHUNK_SIZE);
        assert_eq!(state.pending.len(), RESUMABLE_CHUNK_SIZE);
        assert_eq!(state.pending_end(), RESUMABLE_CHUNK_SIZE as u64);

        state.acknowledge(131_072).unwrap();
        assert_eq!(state.committed, 131_072);
        assert_eq!(state.pending.len(), 131_072);
        assert_eq!(state.pending_end(), RESUMABLE_CHUNK_SIZE as u64);

        let refill = Buffer::from(vec![8; 131_072 + 17]);
        assert_eq!(state.accept(&refill), 131_072);
        assert_eq!(state.pending.len(), RESUMABLE_CHUNK_SIZE);
        assert_eq!(state.pending_end(), 393_216);

        state.acknowledge(393_216).unwrap();
        assert!(state.pending.is_empty());
        assert!(state.acknowledge(393_217).is_err());
    }
}
