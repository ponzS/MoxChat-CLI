use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MARKER: &str = "__stream_message__:";
pub const MAX_TEXT: usize = 256 * 1024;
pub fn transport_id(text: &str, scope: &str) -> Result<String> {
    let Some(f) = Frame::parse(text)? else {
        return Ok(crate::crypto::id());
    };
    let h = crate::crypto::hash(format!("{scope}:{}:{}", f.stream_id, f.seq));
    let id = format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    );
    Ok(if f.seq > 1 {
        format!("mox.silent.{id}")
    } else {
        id
    })
}
pub fn prepared(store: &crate::store::Store, id: &str, text: &str) -> Result<Option<Value>> {
    if let Some(v) = store.get("prepared_message", id)? {
        ensure!(
            v["fingerprint"] == crate::crypto::hash(text),
            "STREAM_CONFLICT: 相同流式序号内容已准备，不能改写"
        );
        return Ok(Some(v["result"].clone()));
    }
    Ok(None)
}
pub fn save_prepared(
    store: &crate::store::Store,
    id: &str,
    text: &str,
    result: &Value,
) -> Result<()> {
    store.put(
        "prepared_message",
        id,
        &serde_json::json!({"fingerprint":crate::crypto::hash(text),"result":result}),
    )
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Frame {
    pub v: u8,
    pub stream_id: String,
    pub seq: u64,
    pub kind: String,
    pub format: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
impl Frame {
    pub fn parse(text: &str) -> Result<Option<Self>> {
        let Some(raw) = text.strip_prefix(MARKER) else {
            return Ok(None);
        };
        ensure!(raw.len() <= MAX_TEXT * 6 + 2048, "流式消息过大");
        let v: Value = crate::crypto::json_strict(raw)?;
        let f: Self = serde_json::from_value(v)?;
        f.validate()?;
        Ok(Some(f))
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.v == 1
                && self.seq > 0
                && self.seq <= 10000
                && !self.stream_id.is_empty()
                && self.stream_id.len() <= 128
                && self
                    .stream_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._~:-".contains(&c)),
            "无效流式消息标记"
        );
        ensure!(
            matches!(
                self.kind.as_str(),
                "snapshot" | "delta" | "final" | "interrupted"
            ) && matches!(self.format.as_str(), "markdown" | "plain"),
            "不支持的流式消息类型"
        );
        ensure!(
            self.reply_to.as_ref().is_none_or(|v| v.len() <= 256)
                && self.reason.as_ref().is_none_or(|v| v.len() <= 256),
            "流式元数据过长"
        );
        ensure!(self.text.len() <= MAX_TEXT, "流式消息正文过长");
        if self.kind == "delta" {
            ensure!(
                self.base_seq.context("delta 缺少 baseSeq")? + 1 == self.seq
                    && self.text.len() <= 4096,
                "无效 delta"
            )
        } else {
            ensure!(self.base_seq.is_none(), "非 delta 帧不允许 baseSeq")
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<String> {
        self.validate()?;
        Ok(format!("{MARKER}{}", serde_json::to_string(self)?))
    }
    pub fn terminal(&self) -> bool {
        matches!(self.kind.as_str(), "final" | "interrupted")
    }
    pub fn final_text(text: String, reply_to: Option<String>) -> Self {
        Self {
            v: 1,
            stream_id: crate::crypto::id(),
            seq: 1,
            kind: "final".into(),
            format: "markdown".into(),
            text,
            base_seq: None,
            reply_to,
            reason: None,
        }
    }
}
