use crate::crypto::{self, string, Identity};
use anyhow::{bail, ensure, Context, Result};
use reqwest::{Client, Method, StatusCode, Url};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct Api {
    pub client: Client,
    pub file_client: Client,
    pub identity: Identity,
    sessions: Arc<Mutex<HashMap<String, (String, u64)>>>,
}
pub fn origin(s: &str) -> Result<String> {
    let u = Url::parse(s)?;
    ensure!(
        matches!(u.scheme(), "http" | "https")
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
            && matches!(u.path(), "" | "/"),
        "INVALID_RELAY: 中继必须是 HTTP(S) 源站地址"
    );
    Ok(u.origin().ascii_serialization())
}
impl Api {
    pub async fn ensure_session(&self, base: &str) -> Result<()> {
        if self
            .sessions
            .lock()
            .await
            .get(base)
            .is_some_and(|v| v.1 > crypto::now() + 5000)
        {
            return Ok(());
        }
        self.request(base, Method::POST, "/api/secure/session", Some(&json!({})))
            .await?;
        Ok(())
    }
    pub fn new(identity: Identity) -> Result<Self> {
        Ok(Self {
            identity,
            client: Client::builder()
                .http1_only()
                .timeout(Duration::from_secs(20))
                .connect_timeout(Duration::from_secs(8))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            file_client: Client::builder()
                .timeout(Duration::from_secs(60))
                .connect_timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            sessions: Default::default(),
        })
    }
    pub fn path(&self, suffix: &str) -> String {
        format!("/api/pub/{}/{}", self.identity.public, suffix)
    }
    async fn decode(&self, mut r: reqwest::Response) -> Result<(StatusCode, Value)> {
        let status = r.status();
        let mut data = Vec::new();
        while let Some(chunk) = r.chunk().await? {
            ensure!(
                data.len() + chunk.len() <= 8 * 1024 * 1024,
                "RELAY_RESPONSE_TOO_LARGE: Relay response exceeds 8 MiB"
            );
            data.extend(chunk);
        }
        let v = match std::str::from_utf8(&data)
            .map_err(anyhow::Error::from)
            .and_then(crypto::json_strict)
        {
            Ok(value) => value,
            Err(_) if !status.is_success() => {
                bail!("RELAY_ERROR: HTTP {} (non-JSON response)", status.as_u16())
            }
            Err(error) => {
                return Err(error.context(format!(
                    "RELAY_RESPONSE_INVALID: Invalid JSON (HTTP {status})"
                )))
            }
        };
        Ok((status, v))
    }
    pub async fn request(
        &self,
        base: &str,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let cached = self
            .sessions
            .lock()
            .await
            .get(base)
            .filter(|v| v.1 > crypto::now() + 5000)
            .cloned();
        let url = format!("{base}{path}");
        let build = || {
            let mut r = self
                .client
                .request(method.clone(), &url)
                .header("X-Mox-Pub", &self.identity.public);
            if let Some(body) = body {
                r = r.json(body)
            }
            r
        };
        let mut r = build();
        if let Some((token, _)) = cached {
            r = r.bearer_auth(token)
        }
        let (mut status, mut v) = self.decode(r.send().await?).await?;
        if status == StatusCode::UNAUTHORIZED {
            self.sessions.lock().await.remove(base);
            if v["type"] != "challenge" {
                (status, v) = self.decode(build().send().await?).await?;
            }
            if status == StatusCode::UNAUTHORIZED && v["type"] == "challenge" {
                let code = crypto::decrypt(
                    &v["challenge"],
                    &self.identity.private,
                    string(&v["challenge"], "sender")?,
                )?;
                let timestamp = v["challenge"]["timestamp"]
                    .as_u64()
                    .context("Invalid challenge timestamp")?;
                ensure!(
                    timestamp.abs_diff(crypto::now()) < 300_000,
                    "身份验证挑战已过期"
                );
                (status, v) = self
                    .decode(
                        build()
                            .header("X-Mox-Challenge-Id", string(&v, "challengeId")?)
                            .header("X-Mox-Code", code)
                            .send()
                            .await?,
                    )
                    .await?;
            }
        }
        ensure!(
            status.is_success(),
            "RELAY_ERROR: HTTP {} {}",
            status.as_u16(),
            v.get("message")
                .or(v.get("code"))
                .unwrap_or(&json!("Request failed"))
        );
        let session = if path == "/api/secure/session" {
            &v
        } else {
            &v["session"]
        };
        if let (Some(token), Some(exp)) = (session["token"].as_str(), session["expiresAt"].as_u64())
        {
            self.sessions
                .lock()
                .await
                .insert(base.into(), (token.into(), exp));
        }
        Ok(v)
    }
    pub async fn advertisement(&self, base: &str) -> Result<Value> {
        let (status, v) = self
            .decode(
                self.client
                    .get(format!("{base}/api/mesh/v1/identity"))
                    .send()
                    .await?,
            )
            .await?;
        ensure!(status.is_success(), "无法读取中继身份：HTTP {status}");
        crypto::require_fields(
            &v,
            &[
                "version",
                "scope",
                "nodeId",
                "serverPub",
                "endpoints",
                "capabilities",
                "seq",
                "issuedAt",
                "expiresAt",
                "signature",
            ],
            &[],
        )?;
        ensure!(v["version"] == 1, "不支持的中继身份版本");
        crypto::verify_document(
            "mox:mesh:relay-advertisement:v1",
            &v,
            string(&v, "serverPub")?,
        )?;
        ensure!(
            v["nodeId"] == crypto::hash(string(&v, "serverPub")?),
            "中继身份不匹配"
        );
        ensure!(
            v["expiresAt"].as_u64().unwrap_or(0) > crypto::now(),
            "中继身份已过期"
        );
        ensure!(
            v["endpoints"]
                .as_array()
                .is_some_and(|a| a.iter().any(|e| e["url"]
                    .as_str()
                    .is_some_and(|s| s.trim_end_matches('/') == base))),
            "中继身份未声明此地址"
        );
        Ok(v)
    }
    pub async fn resolve(&self, bases: &[String], target: &str) -> Result<Value> {
        crypto::parse_public(target)?;
        let mut errors = Vec::new();
        for base in bases {
            let result = async {
                let v = self
                    .request(
                        base,
                        Method::GET,
                        &self.path(&format!("mesh/users/{target}")),
                        None,
                    )
                    .await?;
                let profile = &v["profile"];
                let route = &v["route"];
                crypto::require_fields(
                    profile,
                    &[
                        "version",
                        "ownerPub",
                        "encryptionPub",
                        "name",
                        "link",
                        "avatar",
                        "updatedAt",
                        "signature",
                    ],
                    &[],
                )?;
                crypto::require_fields(
                    route,
                    &[
                        "version",
                        "ownerPub",
                        "routeEpoch",
                        "mailboxes",
                        "issuedAt",
                        "expiresAt",
                        "signature",
                    ],
                    &["profileCid"],
                )?;
                ensure!(
                    profile["version"] == 1
                        && route["version"] == 1
                        && profile["ownerPub"] == target
                        && route["ownerPub"] == target
                        && v["ownerPub"] == target,
                    "用户资料身份不匹配"
                );
                crypto::verify_document("mox:mesh:profile-document:v1", profile, target)?;
                crypto::verify_document("mox:mesh:user-relay-route:v1", route, target)?;
                let cid = hex::encode(crypto::digest("mox:mesh:profile-document-cid:v1", profile)?);
                ensure!(
                    v["profileCid"] == cid && route["profileCid"] == cid,
                    "用户资料 CID 不匹配"
                );
                ensure!(
                    route["expiresAt"].as_u64().unwrap_or(0) > crypto::now(),
                    "用户路由已过期"
                );
                crypto::parse_public(string(profile, "encryptionPub")?)?;
                Ok::<_, anyhow::Error>(v)
            }
            .await;
            match result {
                Ok(v) => return Ok(v),
                Err(e) => errors.push(e.to_string()),
            }
        }
        bail!("USER_UNREACHABLE: {}", errors.join("; "))
    }
}

// Keep the cause chain (timeouts, TLS and connection failures), without
// exposing capability tokens in URLs to logs or Codex tool results.
pub fn diagnostic(error: &anyhow::Error) -> String {
    let mut message = format!("{error:#}");
    for cause in error.chain() {
        if let Some(url) = cause.downcast_ref::<reqwest::Error>().and_then(|e| e.url()) {
            message = message.replace(url.as_str(), &url.origin().ascii_serialization());
        }
    }
    message
        .chars()
        .filter(|c| !c.is_control())
        .take(1200)
        .collect()
}
