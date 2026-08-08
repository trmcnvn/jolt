//! Uploads — attachment staging and the content-addressed edge mirror.
//!
//! The UI streams raw binary chunks sized for the relay; legacy clients may
//! still send base64 chunks. Chunks stage on disk under `{data_dir}/uploads/tmp/
//! {uploadId}/{seq}.{bin,b64}` so they survive an engine restart mid-upload, and
//! `commit` incrementally assembles them into
//! `{data_dir}/uploads/{id8}-{name}` and returns the absolute path plus hash;
//! the composer appends the path to the prompt so the agent can read the file.
//!
//! On account-scope commit the assembled bytes are synchronously mirrored to
//! the edge: `PUT {edge}/attachments/{chatId}/{sha256}` (bearer auth,
//! chat-scoped R2 — `edge/src/index.ts`). The commit returns both the host path
//! and SHA-256 so clients can persist the local agent reference and fall back
//! to authenticated `GET {edge}/attachments/{chatId}/{sha256}`.
//!
//! `read_chunk` serves transcript images back in 45KB base64 chunks. Path jail:
//! only files under the uploads dir or a workspace-known chat cwd are readable
//! (the RPC layer supplies the cwd roots), and only supported image types.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::EngineError;
use crate::doc_host::EdgeConfig;
pub use jolt_api::{AttachmentChunk, CommittedAttachment};
use jolt_vcs::hex;

/// A pending upload must finish within this window (covers slow mesh links).
const STAGING_TTL: Duration = Duration::from_secs(10 * 60);
/// Hard cap on an assembled file (matches the edge's 32MB attachment cap).
const MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Multiple of 3 so independent base64 chunks concatenate losslessly.
const READ_CHUNK_BYTES: u64 = 45_000;

struct UploadsInner {
    /// Durable home for committed attachments (`{data_dir}/uploads`).
    dir: PathBuf,
    /// Chunk staging (`{data_dir}/uploads/tmp/{uploadId}/`).
    tmp: PathBuf,
    edge: Option<EdgeConfig>,
    http: reqwest::Client,
}

#[derive(Clone)]
pub struct Uploads {
    inner: Arc<UploadsInner>,
}

impl Uploads {
    pub fn new(data_dir: &Path, edge: Option<EdgeConfig>) -> Self {
        let dir = data_dir.join("uploads");
        Self {
            inner: Arc::new(UploadsInner {
                tmp: dir.join("tmp"),
                dir,
                edge,
                http: reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new()),
            }),
        }
    }

    /// The durable uploads dir (a path-jail root).
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Stage one base64 chunk. Positional (`seq`) writes are IDEMPOTENT: a client
    /// retrying a chunk whose ack was lost overwrites the same slot instead of
    /// double-appending. Callers without `seq` get append-only behavior.
    pub fn append(&self, upload_id: &str, data: &str, seq: Option<u64>) -> Result<(), EngineError> {
        let dir = self.staging_dir(upload_id)?;
        self.sweep();
        std::fs::create_dir_all(&dir)?;
        let at = match seq {
            Some(seq) => seq,
            None => next_free_seq(&dir)?,
        };
        if at > 1_000_000 {
            return Err(EngineError::Other("Invalid chunk index".into()));
        }
        // Base64 inflates by ~4/3; bound decoded bytes against the file cap.
        let staged = staged_bytes(&dir, Some(at))?;
        let decoded_bound = (data.len() as u64).div_ceil(4).saturating_mul(3);
        if staged.saturating_add(decoded_bound) > MAX_BYTES {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(EngineError::Other("Upload too large".into()));
        }
        std::fs::write(dir.join(format!("{at:06}.b64")), data)?;
        Ok(())
    }

    /// Stage one raw binary chunk without base64 expansion.
    pub fn append_bytes(
        &self,
        upload_id: &str,
        data: &[u8],
        seq: Option<u64>,
    ) -> Result<(), EngineError> {
        let dir = self.staging_dir(upload_id)?;
        self.sweep();
        std::fs::create_dir_all(&dir)?;
        let at = seq.unwrap_or(next_free_seq(&dir)?);
        if at > 1_000_000 {
            return Err(EngineError::Other("Invalid chunk index".into()));
        }
        let staged = staged_bytes(&dir, Some(at))?;
        if staged.saturating_add(data.len() as u64) > MAX_BYTES {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(EngineError::Other("Upload too large".into()));
        }
        std::fs::write(dir.join(format!("{at:06}.bin")), data)?;
        Ok(())
    }

    /// Assemble the staged chunks into a durable file and return its host path
    /// plus edge content address. Account-scope commits do not succeed until
    /// the content-addressed R2 mirror is durable.
    pub async fn commit(
        &self,
        upload_id: &str,
        file_name: &str,
        chat_id: &str,
    ) -> Result<CommittedAttachment, EngineError> {
        if !valid_chat_id(chat_id) {
            return Err(EngineError::Other("Invalid chat id".into()));
        }
        let dir = self.staging_dir(upload_id)?;
        let mut parts = chunk_files(&dir)?;
        if parts.is_empty() {
            return Err(EngineError::Other("Unknown or expired upload".into()));
        }
        parts.sort_by_key(|(seq, _)| *seq);
        std::fs::create_dir_all(&self.inner.dir)?;
        let name = sanitize(file_name);
        let id8: String = upload_id.chars().take(8).collect();
        let path = self.inner.dir.join(format!("{id8}-{name}"));
        let sha256 = match assemble_chunks(&parts, &path) {
            Ok(sha256) => sha256,
            Err(error) => {
                let _ = std::fs::remove_file(&path);
                return Err(error);
            }
        };
        self.mirror_to_edge(&path, chat_id, &sha256).await?;
        // Keep staged chunks until the edge write succeeds so a failed commit
        // can be retried without retransmitting the image.
        let _ = std::fs::remove_dir_all(&dir);
        Ok(CommittedAttachment {
            path: path.to_string_lossy().to_string(),
            sha256,
        })
    }

    /// Read one 45KB chunk of an attachment. `extra_roots` are the workspace's
    /// known chat cwds — together with the uploads dir they form the path jail.
    pub fn read_chunk(
        &self,
        path: &str,
        offset: u64,
        extra_roots: &[PathBuf],
    ) -> Result<AttachmentChunk, EngineError> {
        use std::io::Seek;
        let file = self.inspect(path, extra_roots)?;
        let size = file.size;
        let start = offset.min(size);
        let next_offset = (start + READ_CHUNK_BYTES).min(size);
        // Read ONLY this chunk's byte range — never the whole file per chunk.
        let mut buf = vec![0u8; (next_offset - start) as usize];
        let mut handle = std::fs::File::open(&file.resolved)?;
        handle.seek(std::io::SeekFrom::Start(start))?;
        let mut read = 0usize;
        while read < buf.len() {
            let n = handle.read(&mut buf[read..])?;
            if n == 0 {
                break;
            }
            read += n;
        }
        buf.truncate(read);
        Ok(AttachmentChunk {
            name: file.name,
            mime_type: file.mime_type,
            data: crate::simd_base64::encode(&buf),
            next_offset,
            done: next_offset >= size,
        })
    }

    // ── internals ───────────────────────────────────────────────────────────

    fn staging_dir(&self, upload_id: &str) -> Result<PathBuf, EngineError> {
        // The id becomes a directory name — jail it to a safe charset.
        let ok = !upload_id.is_empty()
            && upload_id.len() <= 64
            && upload_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
        if !ok {
            return Err(EngineError::Other("Invalid upload id".into()));
        }
        Ok(self.inner.tmp.join(upload_id))
    }

    /// Reclaim staging dirs whose newest chunk is older than the TTL (an upload
    /// abandoned mid-stream must not hold up to 32MB forever).
    fn sweep(&self) {
        let Ok(entries) = std::fs::read_dir(&self.inner.tmp) else {
            return;
        };
        for entry in entries.flatten() {
            let newest = std::fs::read_dir(entry.path())
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|f| f.metadata().ok()?.modified().ok())
                .max();
            let expired = match newest {
                Some(at) => at.elapsed().map(|age| age > STAGING_TTL).unwrap_or(false),
                None => true, // empty dir — reclaim
            };
            if expired {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    fn inspect(&self, path: &str, extra_roots: &[PathBuf]) -> Result<InspectedFile, EngineError> {
        let outside = || EngineError::Other("Attachment is outside the upload cache".into());
        // Canonicalize BOTH sides so `..` segments and symlinks can't escape.
        let resolved = std::fs::canonicalize(path).map_err(|_| outside())?;
        let allowed = std::iter::once(&self.inner.dir)
            .chain(extra_roots.iter())
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .any(|root| resolved.starts_with(&root) && resolved != root);
        if !allowed {
            return Err(outside());
        }
        let meta = std::fs::metadata(&resolved)?;
        if !meta.is_file() {
            return Err(EngineError::Other("Attachment is not a file".into()));
        }
        if meta.len() > MAX_BYTES {
            return Err(EngineError::Other("Attachment is too large".into()));
        }
        let mime_type = mime_by_ext(&resolved)
            .ok_or_else(|| EngineError::Other("Attachment is not a supported image".into()))?;
        Ok(InspectedFile {
            name: resolved
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "attachment".into()),
            mime_type: mime_type.to_string(),
            size: meta.len(),
            resolved,
        })
    }

    /// Chat-scoped mirror (`PUT /attachments/{chatId}/{sha256}`, bearer auth).
    /// Local scopes have no edge and remain local-only; account scopes fail the
    /// commit if durability cannot be established.
    async fn mirror_to_edge(
        &self,
        path: &Path,
        chat_id: &str,
        sha256: &str,
    ) -> Result<(), EngineError> {
        let Some(edge) = self.inner.edge.clone() else {
            return Ok(());
        };
        let bearer = edge
            .bearer()
            .await
            .ok_or_else(|| EngineError::Other("Attachment mirror failed: signed out".into()))?;
        let mime = mime_by_ext(path).unwrap_or("application/octet-stream");
        let url = format!(
            "{}/attachments/{chat_id}/{sha256}",
            edge.url.trim_end_matches('/')
        );
        let size = std::fs::metadata(path)?.len();
        let file = tokio::fs::File::open(path).await?;
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
        let response = self
            .inner
            .http
            .put(url)
            .bearer_auth(bearer)
            .header("content-type", mime)
            .header("content-length", size)
            .body(body)
            .send()
            .await
            .map_err(|error| {
                tracing::warn!(sha = %sha256, %error, "edge attachment mirror failed");
                EngineError::Other(format!("Attachment mirror failed: {error}"))
            })?;
        if !response.status().is_success() {
            tracing::warn!(sha = %sha256, status = %response.status(), "edge attachment mirror rejected");
            return Err(EngineError::Other(format!(
                "Attachment mirror failed with status {}",
                response.status()
            )));
        }
        tracing::debug!(sha = %sha256, "attachment mirrored to edge");
        Ok(())
    }
}

struct InspectedFile {
    resolved: PathBuf,
    name: String,
    mime_type: String,
    size: u64,
}

fn valid_chat_id(chat_id: &str) -> bool {
    !chat_id.is_empty()
        && chat_id.len() <= 64
        && chat_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn chunk_files(dir: &Path) -> Result<Vec<(u64, PathBuf)>, EngineError> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let seq = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok());
        let extension = path.extension().and_then(|extension| extension.to_str());
        if let Some(seq) = seq
            && matches!(extension, Some("b64" | "bin"))
        {
            files.push((seq, path));
        }
    }
    Ok(files)
}

fn staged_bytes(dir: &Path, excluding: Option<u64>) -> Result<u64, EngineError> {
    chunk_files(dir)?
        .into_iter()
        .try_fold(0u64, |total, (seq, path)| {
            if excluding == Some(seq) {
                return Ok(total);
            }
            let len = std::fs::metadata(&path)?.len();
            let bytes = if path.extension().and_then(|extension| extension.to_str()) == Some("b64")
            {
                len.div_ceil(4).saturating_mul(3)
            } else {
                len
            };
            Ok(total.saturating_add(bytes))
        })
}

fn assemble_chunks(parts: &[(u64, PathBuf)], output: &Path) -> Result<String, EngineError> {
    for (index, (seq, _)) in parts.iter().enumerate() {
        if *seq != index as u64 {
            return Err(EngineError::Other("Upload is missing a chunk".into()));
        }
    }
    let base64 = parts
        .iter()
        .all(|(_, path)| path.extension().and_then(|extension| extension.to_str()) == Some("b64"));
    let binary = parts
        .iter()
        .all(|(_, path)| path.extension().and_then(|extension| extension.to_str()) == Some("bin"));
    if !base64 && !binary {
        return Err(EngineError::Other(
            "Upload mixes incompatible chunk formats".into(),
        ));
    }

    let mut output = std::fs::File::create(output)?;
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    if base64 {
        let mut carry = Vec::new();
        for (index, (_, path)) in parts.iter().enumerate() {
            let encoded = std::fs::read(path)?;
            carry.extend_from_slice(encoded.trim_ascii());
            let last = index + 1 == parts.len();
            let decode_len = if last {
                carry.len()
            } else {
                carry.len() / 4 * 4
            };
            let tail = carry.split_off(decode_len);
            let decoded = crate::simd_base64::decode(&carry).map_err(|error| {
                EngineError::Other(format!("upload is not valid base64: {error}"))
            })?;
            written = written.saturating_add(decoded.len() as u64);
            if written > MAX_BYTES {
                return Err(EngineError::Other("Upload too large".into()));
            }
            hasher.update(&decoded);
            output.write_all(&decoded)?;
            carry = tail;
        }
    } else {
        let mut copy_buffer = vec![0u8; 64 * 1024];
        for (_, path) in parts {
            let mut input = std::fs::File::open(path)?;
            loop {
                let read = input.read(&mut copy_buffer)?;
                if read == 0 {
                    break;
                }
                written = written.saturating_add(read as u64);
                if written > MAX_BYTES {
                    return Err(EngineError::Other("Upload too large".into()));
                }
                hasher.update(&copy_buffer[..read]);
                output.write_all(&copy_buffer[..read])?;
            }
        }
    }
    output.flush()?;
    Ok(hex(&hasher.finalize()))
}

fn next_free_seq(dir: &Path) -> Result<u64, EngineError> {
    Ok(chunk_files(dir)?
        .iter()
        .map(|(seq, _)| seq + 1)
        .max()
        .unwrap_or(0))
}

fn sanitize(file_name: &str) -> String {
    let base = Path::new(file_name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let tail: String = cleaned
        .chars()
        .rev()
        .take(80)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail.is_empty() {
        "upload".into()
    } else {
        tail
    }
}

fn mime_by_ext(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        "bmp" => Some("image/bmp"),
        "tif" | "tiff" => Some("image/tiff"),
        "avif" => Some("image/avif"),
        "heic" => Some("image/heic"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("my photo (1).png"), "my_photo__1_.png");
        assert_eq!(sanitize(""), "upload");
    }
}
