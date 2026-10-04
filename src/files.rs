use crate::{
    api::{self, Api},
    crypto::{self, string},
    messaging::SharedStore,
};
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use anyhow::{anyhow, ensure, Context, Result};
use reqwest::{Method, Url};
use serde_json::{json, Value};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub fn check_path(path: &Path) -> Result<PathBuf> {
    let meta = path.metadata().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!("FILE_NOT_FOUND: 文件不存在：{}", path.display())
        } else {
            anyhow!("FILE_NOT_READABLE: 无法读取文件：{}", path.display())
        }
    })?;
    ensure!(
        meta.is_file(),
        "PATH_NOT_FILE: 路径不是文件：{}",
        path.display()
    );
    File::open(path)
        .with_context(|| format!("FILE_NOT_READABLE: 无法读取文件：{}", path.display()))?;
    Ok(path.canonicalize()?)
}
fn media_type(head: &[u8], path: &Path, kind: &str) -> Result<String> {
    let mime = if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if head.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        "image/gif"
    } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else if head.get(4..8) == Some(b"ftyp") {
        match head.get(8..12) {
            Some(b"avif" | b"avis") => "image/avif",
            Some(b"heic" | b"heix" | b"mif1") => "image/heic",
            Some(b"qt  ") => "video/quicktime",
            _ => "video/mp4",
        }
    } else if head.starts_with(b"\x1a\x45\xdf\xa3") {
        "video/webm"
    } else {
        "application/octet-stream"
    };
    if kind == "image" {
        ensure!(
            mime.starts_with("image/"),
            "MEDIA_TYPE_MISMATCH: --img 路径不是支持的图片"
        )
    }
    if kind == "video" {
        ensure!(
            mime.starts_with("video/"),
            "MEDIA_TYPE_MISMATCH: --video 路径不是支持的视频"
        )
    }
    Ok(if kind == "file" {
        mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string()
    } else {
        mime.into()
    })
}
fn prepare(source: &Path, destination: &Path, kind: &str) -> Result<Value> {
    let mut file = File::open(source)?;
    let before = file.metadata()?;
    let size = before.len();
    let mut head = [0; 64];
    let count = file.read(&mut head)?;
    let mime = media_type(&head[..count], source, kind)?;
    std::io::Seek::rewind(&mut file)?;
    let key = zeroize::Zeroizing::new(crypto::random::<32>());
    let context = crypto::random::<32>();
    let mut header = crypto::random::<40>();
    header[0] = 40;
    let mut derived = zeroize::Zeroizing::new([0u8; 32]);
    hkdf::Hkdf::<sha2::Sha256>::new(Some(&header[1..33]), key.as_ref())
        .expand(&context, derived.as_mut())
        .map_err(|_| anyhow!("文件密钥派生失败"))?;
    let cipher =
        Aes256Gcm::new_from_slice(derived.as_ref()).map_err(|_| anyhow!("无效文件密钥"))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)?;
    output.write_all(&header)?;
    const PAYLOAD: u64 = 1048576 - 16;
    let segments = (size + 40).div_ceil(PAYLOAD).max(1);
    ensure!(segments <= u32::MAX as u64, "文件过大");
    let mut consumed = 0;
    for index in 0..segments {
        let length = (size - consumed).min(PAYLOAD - if index == 0 { 40 } else { 0 }) as usize;
        let mut bytes = zeroize::Zeroizing::new(vec![0; length]);
        file.read_exact(&mut bytes)?;
        let mut nonce = [0; 12];
        nonce[..7].copy_from_slice(&header[33..]);
        nonce[7..11].copy_from_slice(&(index as u32).to_be_bytes());
        nonce[11] = u8::from(index + 1 == segments);
        let ciphertext = cipher
            .encrypt(&Nonce::from(nonce), bytes.as_slice())
            .map_err(|_| anyhow!("文件加密失败"))?;
        output.write_all(&ciphertext)?;
        consumed += length as u64;
    }
    let after = file.metadata()?;
    let source_after = source.metadata()?;
    use std::os::unix::fs::MetadataExt;
    ensure!(
        before.len() == after.len()
            && before.modified()? == after.modified()?
            && before.ino() == source_after.ino()
            && before.dev() == source_after.dev()
            && before.modified()? == source_after.modified()?,
        "FILE_CHANGED: 文件在准备过程中发生变化"
    );
    output.sync_all()?;
    Ok(
        json!({"fileName":source.file_name().context("无效文件名")?.to_string_lossy(),"mime":mime,"sizeBytes":size,"encryptedSizeBytes":size+40+segments*16,"fileCipher":{"v":1,"profile":"AES256_GCM_HKDF_1MB","key":crypto::b64(key.as_ref()),"context":crypto::b64(context),"plaintextSize":size}}),
    )
}
fn same_origin(base: &str, url: &str) -> Result<Url> {
    let u = Url::parse(base)?.join(url)?;
    ensure!(
        u.origin() == Url::parse(base)?.origin(),
        "文件中继返回了不同源的上传地址"
    );
    Ok(u)
}
pub async fn upload(
    store: &SharedStore,
    api: &Api,
    source: &Path,
    kind: &str,
    recipients: &[String],
    task_id: &str,
) -> Result<String> {
    let existing = store.lock().unwrap().get("file_task", task_id)?;
    let mut task = if let Some(v) = existing {
        v
    } else {
        let source = check_path(source)?;
        let path = {
            let s = store.lock().unwrap();
            let dir = s.dir.join("attachments");
            std::fs::create_dir_all(&dir)?;
            dir.join(format!("{}.cipher", crypto::id()))
        };
        let destination = path.clone();
        let kind = kind.to_owned();
        let result =
            tokio::task::spawn_blocking(move || prepare(&source, &destination, &kind)).await?;
        let payload = match result {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_file(path);
                return Err(e);
            }
        };
        let relay = crate::relays::selected(&store.lock().unwrap(), true)?;
        let v = json!({"path":path,"payload":payload,"recipients":recipients,"relay":relay});
        store.lock().unwrap().put("file_task", task_id, &v)?;
        v
    };
    ensure!(
        task["recipients"] == json!(recipients),
        "FILE_RECIPIENT_CHANGED: 附件重试的收件人发生变化"
    );
    // File traffic has its own connection pool and upload-sized deadline.
    let mut file_api = api.clone();
    file_api.client = api.file_client.clone();
    for attempt in 0..5 {
        match upload_attempt(store, &file_api, &mut task, kind, recipients, task_id).await {
            Ok(template) => return Ok(template),
            Err(error) => {
                let retry = attempt < 4 && retryable(&error);
                let detail = api::diagnostic(&error);
                task["lastError"] = json!(detail);
                store.lock().unwrap().put("file_task", task_id, &task)?;
                store.lock().unwrap().event("file.error", json!({"taskId":task_id,"stage":task["stage"],"error":detail,"retrying":retry,"attempt":attempt+1}))?;
                if !retry {
                    return Err(error.context(format!(
                        "FILE_UPLOAD_FAILED: {}",
                        task["stage"].as_str().unwrap_or("upload")
                    )));
                }
                tokio::time::sleep(std::time::Duration::from_millis(500 * (1 << attempt))).await;
            }
        }
    }
    unreachable!()
}

#[derive(Debug)]
struct UploadHttpError(reqwest::StatusCode);
impl std::fmt::Display for UploadHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "File relay returned HTTP {}", self.0.as_u16())
    }
}
impl std::error::Error for UploadHttpError {}
fn retryable(error: &anyhow::Error) -> bool {
    if let Some(http) = error.downcast_ref::<UploadHttpError>() {
        return http.0.is_server_error() || matches!(http.0.as_u16(), 408 | 409 | 429);
    }
    if let Some(network) = error.downcast_ref::<reqwest::Error>() {
        return network.is_timeout()
            || network.is_connect()
            || network.is_body()
            || network.is_request();
    }
    error.chain().any(|cause| {
        let text = cause.to_string();
        text.starts_with("RELAY_ERROR: HTTP 5")
            || text.starts_with("RELAY_ERROR: HTTP 429")
            || text.starts_with("RELAY_ERROR: HTTP 408")
    })
}
fn stage(store: &SharedStore, task: &mut Value, id: &str, name: &str) -> Result<()> {
    if task["stage"] != name {
        task["stage"] = json!(name);
        let s = store.lock().unwrap();
        s.put("file_task", id, task)?;
        s.event("file.stage", json!({"taskId":id,"stage":name}))?;
    }
    Ok(())
}
fn uploaded(status: &Value, size: u64) -> Result<bool> {
    if status["item"]["status"] != "uploaded" {
        return Ok(false);
    }
    ensure!(
        status["item"]["sizeBytes"].as_u64() == Some(size),
        "FILE_SIZE_MISMATCH: Relay confirmation does not match the ciphertext length"
    );
    Ok(true)
}
async fn upload_attempt(
    store: &SharedStore,
    api: &Api,
    task: &mut Value,
    kind: &str,
    recipients: &[String],
    task_id: &str,
) -> Result<String> {
    let base = api::origin(string(task, "relay")?)?;
    if task["session"].is_null() {
        stage(store, task, task_id, "creating_session")?;
        let session=api.request(&base,Method::POST,"/api/secure/files/tus-sessions",Some(&json!({"kind":"file","mime":"application/octet-stream","fileName":"attachment.bin","sizeBytes":task["payload"]["encryptedSizeBytes"],"sha256":"","recipientPubs":recipients}))).await.context("Creating encrypted file session")?;
        task["session"] = session;
        store.lock().unwrap().put("file_task", task_id, task)?;
    }
    let session = task["session"].clone();
    let size = task["payload"]["encryptedSizeBytes"]
        .as_u64()
        .context("Invalid ciphertext size")?;
    let token = string(&session, "uploadToken")?;
    let file_id = string(&session, "fileId")?;
    let status_path = format!("/api/secure/files/{file_id}/status");
    stage(store, task, task_id, "checking_upload")?;
    let status = api
        .request(&base, Method::GET, &status_path, None)
        .await
        .context("Checking encrypted file upload")?;
    let mut confirmed = uploaded(&status, size)?;
    let request = |method, url: Url| {
        api.client
            .request(method, url)
            .header("Tus-Resumable", "1.0.0")
            .header("X-Mox-File-Id", file_id)
            .header("X-Mox-Upload-Token", token)
    };
    if !confirmed {
        stage(store, task, task_id, "opening_upload")?;
        if task["uploadUrl"].is_null() {
            let endpoint = session["tusEndpoint"]
                .as_str()
                .or(session["uploadUrl"].as_str())
                .context("Missing TUS endpoint")?;
            let endpoint = same_origin(&base, endpoint)?;
            // A previous POST may have succeeded even when its response was lost.
            let candidate = same_origin(
                &base,
                &format!("{}/{}", endpoint.as_str().trim_end_matches('/'), file_id),
            )?;
            let probe = request(Method::HEAD, candidate.clone())
                .send()
                .await
                .context("Looking up resumable upload")?;
            let url = if probe.status().is_success() {
                candidate
            } else {
                if probe.status() != 404 {
                    return Err(UploadHttpError(probe.status()).into());
                }
                let response = request(Method::POST, endpoint.clone())
                    .header("Upload-Length", size)
                    .send()
                    .await
                    .context("Opening resumable upload")?;
                if response.status() == 409 {
                    candidate
                } else {
                    if response.status() != 201 {
                        return Err(UploadHttpError(response.status()).into());
                    }
                    let location = response
                        .headers()
                        .get("Location")
                        .context("Missing TUS Location")?
                        .to_str()?;
                    let url = endpoint.join(location)?;
                    same_origin(&base, url.as_str())?
                }
            };
            task["uploadUrl"] = json!(url.as_str());
            store.lock().unwrap().put("file_task", task_id, task)?;
        }
        stage(store, task, task_id, "uploading")?;
        let url = same_origin(&base, string(task, "uploadUrl")?)?;
        // Re-read the server offset on every retry, including lost PATCH acknowledgements.
        let response = request(Method::HEAD, url.clone())
            .send()
            .await
            .context("Reading upload offset")?;
        if !response.status().is_success() {
            return Err(UploadHttpError(response.status()).into());
        }
        let mut offset: u64 = response
            .headers()
            .get("Upload-Offset")
            .context("Missing upload offset")?
            .to_str()?
            .parse()?;
        ensure!(
            offset <= size,
            "INVALID_UPLOAD_OFFSET: Relay offset exceeds ciphertext size"
        );
        while offset < size {
            let length = (size - offset).min(1024 * 1024);
            let mut file = tokio::fs::File::open(string(task, "path")?).await?;
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::with_capacity(
                file.take(length),
                16 * 1024,
            ));
            let next = offset + length;
            let response = request(Method::PATCH, url.clone())
                .header("Upload-Offset", offset)
                .header("Content-Type", "application/offset+octet-stream")
                .header(reqwest::header::CONTENT_LENGTH, length)
                .body(body)
                .send()
                .await
                .context("Uploading encrypted file bytes")?;
            if response.status() != 204 {
                return Err(UploadHttpError(response.status()).into());
            }
            ensure!(
                response
                    .headers()
                    .get("Upload-Offset")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    == Some(next),
                "INVALID_UPLOAD_OFFSET: Relay did not acknowledge the uploaded chunk"
            );
            offset = next;
            store.lock().unwrap().event(
                "file.progress",
                json!({"taskId":task_id,"uploaded":offset,"total":size}),
            )?;
        }
        stage(store, task, task_id, "confirming_upload")?;
        for delay in [0, 300, 1000, 2000, 4000] {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            let status = api
                .request(&base, Method::GET, &status_path, None)
                .await
                .context("Confirming uploaded ciphertext")?;
            if uploaded(&status, size)? {
                confirmed = true;
                break;
            }
        }
    }
    ensure!(confirmed,"FILE_UPLOAD_UNCONFIRMED: The relay has not confirmed the complete encrypted file; retry to resume");
    let mut payload = task["payload"].clone();
    for (k,v) in json!({"v":6,"scheme":"moxfile-tus","encrypted":true,"fileId":file_id,"relayUrl":base,"downloadUrl":session["downloadUrl"].as_str().filter(|s|!s.is_empty()).or(session["downloads"][0]["downloadUrl"].as_str()).context("Missing attachment download URL")?,"recipientDownloads":session["downloads"],"state":"remote","syncMode":"final","transferPhase":"done"}).as_object().unwrap(){payload[k]=v.clone();}
    task["lastError"] = Value::Null;
    stage(store, task, task_id, "uploaded")?;
    let prefix = match kind {
        "image" => "__file_image__:",
        "video" => "__file_video__:",
        _ => "__file_blob__:",
    };
    Ok(format!("{prefix}{payload}"))
}
