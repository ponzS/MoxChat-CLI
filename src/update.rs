use crate::{
    args::Command,
    crypto,
    runtime::{self, Request},
    store,
};
use anyhow::{ensure, Context, Result};
use fs2::FileExt;
use reqwest::{Client, Url};
use semver::Version;
use serde_json::{json, Value};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

fn repository() -> Result<String> {
    let repo = std::env::var("MOX_UPDATE_REPO").unwrap_or_else(|_| {
        option_env!("MOX_DEFAULT_UPDATE_REPO")
            .unwrap_or("ponzS/MoxChat-CLI")
            .into()
    });
    let parts = repo.split('/').collect::<Vec<_>>();
    ensure!(
        parts.len() == 2
            && parts.iter().all(|s| !s.is_empty()
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))),
        "INVALID_UPSTREAM: GitHub 上游必须是 owner/repo"
    );
    Ok(repo)
}
fn target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        _ => anyhow::bail!("UPDATE_UNSUPPORTED: 当前系统或架构没有自动更新产物"),
    }
}
fn client() -> Result<Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Ok(token) = std::env::var("MOX_GITHUB_TOKEN") {
        let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .context("INVALID_GITHUB_TOKEN: GitHub token 格式无效")?;
        value.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    Ok(Client::builder()
        .default_headers(headers)
        .user_agent(format!("mox/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let u = attempt.url();
            let host = u.host_str().unwrap_or("");
            if attempt.previous().len() >= 5
                || u.scheme() != "https"
                || !(host == "github.com"
                    || host == "api.github.com"
                    || host.ends_with(".githubusercontent.com"))
            {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()?)
}
async fn download(client: &Client, url: &str, max: usize) -> Result<Vec<u8>> {
    let accept = if url.contains("/releases/assets/") {
        "application/octet-stream"
    } else {
        "application/vnd.github+json"
    };
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, accept)
        .send()
        .await?;
    ensure!(response.status()!=reqwest::StatusCode::NOT_FOUND, "UPDATE_UPSTREAM_UNAVAILABLE: GitHub 返回 404，请检查上游地址；私有仓库可配置 MOX_GITHUB_TOKEN");
    let mut r = response.error_for_status()?;
    ensure!(r.url().scheme() == "https", "更新只允许 HTTPS");
    let mut bytes = Vec::new();
    while let Some(chunk) = r.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= max,
            "UPDATE_TOO_LARGE: 更新文件超出大小限制"
        );
        bytes.extend(chunk);
    }
    Ok(bytes)
}
fn asset_url(asset: &Value, repo: &str) -> Result<String> {
    let url = Url::parse(
        asset["browser_download_url"]
            .as_str()
            .context("缺少发布物地址")?,
    )?;
    ensure!(
        url.scheme() == "https"
            && url.host_str() == Some("github.com")
            && url
                .path()
                .starts_with(&format!("/{repo}/releases/download/")),
        "UPDATE_INVALID_ASSET: 发布物不属于当前上游"
    );
    if std::env::var_os("MOX_GITHUB_TOKEN").is_some() {
        let id = asset["id"]
            .as_u64()
            .context("UPDATE_INVALID_ASSET: 发布物缺少 ID")?;
        return Ok(format!(
            "https://api.github.com/repos/{repo}/releases/assets/{id}"
        ));
    }
    Ok(url.to_string())
}
struct Temporary {
    paths: Vec<PathBuf>,
}
impl Drop for Temporary {
    fn drop(&mut self) {
        for p in &self.paths {
            let _ = std::fs::remove_file(p);
        }
    }
}
pub async fn run(dir: &Path, check: bool) -> Result<Value> {
    let repo = repository()?;
    let client = client()?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let mut latest: Option<(Version, Value)> = None;
    for page in 1..=10 {
        let raw = download(
            &client,
            &format!("https://api.github.com/repos/{repo}/releases?per_page=100&page={page}"),
            8 * 1024 * 1024,
        )
        .await
        .map_err(|e| anyhow::anyhow!("UPDATE_CHECK_FAILED: 无法读取 GitHub Release：{e}"))?;
        let releases: Vec<Value> = serde_json::from_slice(&raw)?;
        let end = releases.len() < 100;
        for release in releases {
            if release["draft"] == true || release["prerelease"] == true {
                continue;
            }
            let Some(tag) = release["tag_name"]
                .as_str()
                .and_then(|s| s.strip_prefix("mox-v"))
            else {
                continue;
            };
            let Ok(version) = Version::parse(tag) else {
                continue;
            };
            if version.pre.is_empty() && latest.as_ref().is_none_or(|(v, _)| version > *v) {
                latest = Some((version, release));
            }
        }
        if end {
            break;
        }
    }
    let (version, release) =
        latest.context("UPDATE_NOT_PUBLISHED: 上游尚未发布 mox-v<版本> 的稳定版")?;
    if check || version <= current {
        return Ok(
            json!({"current":current.to_string(),"latest":version.to_string(),"update_available":version>current,"upstream":repo,"updated":false}),
        );
    }
    let name = format!("mox-{}.tar.gz", target()?);
    let assets = release["assets"].as_array().context("缺少发布物")?;
    let asset = assets
        .iter()
        .find(|a| a["name"] == name)
        .context("UPDATE_ASSET_MISSING: 此版本没有当前系统的二进制产物")?;
    let checksum = assets
        .iter()
        .find(|a| a["name"] == "SHA256SUMS")
        .context("UPDATE_CHECKSUM_MISSING: 发布版缺少 SHA256SUMS")?;
    let sums = download(&client, &asset_url(checksum, &repo)?, 1024 * 1024).await?;
    let expected = std::str::from_utf8(&sums)?
        .lines()
        .find_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            (fields.len() == 2 && fields[1].trim_start_matches('*') == name)
                .then(|| fields[0].to_owned())
        })
        .context("UPDATE_CHECKSUM_MISSING: 找不到当前产物的 SHA-256")?;
    ensure!(
        hex::decode(&expected).is_ok_and(|b| b.len() == 32),
        "无效 SHA-256"
    );
    let exe = std::env::current_exe()?.canonicalize()?;
    let parent = exe.parent().context("无效可执行文件路径")?;
    let lock_path = parent.join(".mox-update.lock");
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .context("UPDATE_PERMISSION_DENIED: 程序目录不可写，请使用有写权限的安装位置")?;
    lock.try_lock_exclusive()
        .context("UPDATE_RUNNING: 另一更新正在执行")?;
    let token = crypto::id();
    let staged = parent.join(format!(".mox-{token}.new"));
    let backup = parent.join(format!(".mox-{token}.previous"));
    let _cleanup = Temporary {
        paths: vec![staged.clone(), backup.clone()],
    };
    let bytes = download(&client, &asset_url(asset, &repo)?, 128 * 1024 * 1024).await?;
    ensure!(
        asset["size"].as_u64() == Some(bytes.len() as u64)
            && crypto::hash(&bytes) == expected.to_lowercase(),
        "UPDATE_CHECKSUM_MISMATCH: 发布物大小或 SHA-256 不匹配，现有版本保留"
    );
    if let Some(digest) = asset["digest"].as_str() {
        ensure!(
            digest == format!("sha256:{}", crypto::hash(&bytes)),
            "UPDATE_CHECKSUM_MISMATCH: GitHub 发布物摘要不匹配"
        );
    }
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
    let mut found = false;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        ensure!(
            path == Path::new("mox") && entry.header().entry_type().is_file() && !found,
            "UPDATE_INVALID_ARCHIVE: 压缩包只能包含一个 mox 常规文件"
        );
        ensure!(entry.size() <= 128 * 1024 * 1024, "UPDATE_TOO_LARGE");
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o700)
            .open(&staged)?;
        std::io::copy(&mut entry.by_ref().take(128 * 1024 * 1024 + 1), &mut output)?;
        output.flush()?;
        output.sync_all()?;
        found = true;
    }
    ensure!(found, "UPDATE_INVALID_ARCHIVE: 缺少 mox");
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(&staged)
            .args(["version", "--json"])
            .output(),
    )
    .await??;
    let reported: Value = serde_json::from_slice(&output.stdout)
        .context("UPDATE_INVALID_BINARY: 发布物无法报告版本")?;
    ensure!(
        output.status.success() && reported["version"] == version.to_string(),
        "UPDATE_VERSION_MISMATCH: 发布物版本与标签不一致"
    );
    std::fs::copy(&exe, &backup)?;
    File::open(&backup)?.sync_all()?;
    let state = runtime::call(dir, &Request::Command(Command::Status))
        .await
        .ok();
    if state.is_some() {
        runtime::call(dir, &Request::Command(Command::Stop)).await?;
        wait_stopped(dir).await?;
    }
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    if let Err(e) = std::fs::rename(&staged, &exe) {
        if let Some(state) = &state {
            let _ = restart(&exe, dir, state).await;
        }
        return Err(e.into());
    }
    File::open(parent)?.sync_all()?;
    if let Some(state) = &state {
        if let Err(e) = restart(&exe, dir, state).await {
            let _ = runtime::call(dir, &Request::Command(Command::Stop)).await;
            let _ = wait_stopped(dir).await;
            std::fs::rename(&backup, &exe)?;
            let recovered = restart(&exe, dir, state).await.is_ok();
            anyhow::bail!("UPDATE_RESTART_FAILED: 已恢复旧程序；旧运行时恢复={recovered}；{e}")
        }
    }
    Ok(
        json!({"previous":current.to_string(),"version":version.to_string(),"updated":true,"restarted":state.is_some(),"upstream":repo}),
    )
}
async fn wait_stopped(dir: &Path) -> Result<()> {
    for _ in 0..150 {
        if !store::socket_path(dir).exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("UPDATE_STOP_TIMEOUT: 运行时未停止，保留原程序")
}
async fn restart(exe: &Path, dir: &Path, state: &Value) -> Result<()> {
    let mut command = tokio::process::Command::new(exe);
    command
        .arg("--data-dir")
        .arg(dir)
        .arg("start")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if state["aiEnabled"] == false {
        command.arg("--no-ai");
    }
    let mut child = command.spawn()?;
    let pid = child.id().context("新运行时没有 PID")?;
    for _ in 0..600 {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("新运行时启动失败：{status}")
        }
        if let Ok(status) = runtime::call(dir, &Request::Command(Command::Status)).await {
            ensure!(
                status["pid"].as_u64() == Some(pid as u64),
                "另一进程占用了身份运行时"
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = child.kill().await;
    anyhow::bail!("新运行时启动超时")
}
