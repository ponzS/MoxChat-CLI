use crate::{
    api::Api,
    args::SendArgs,
    crypto, files,
    messaging::{self, SharedStore},
    stream::Frame,
};
use anyhow::{ensure, Context, Result};
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

pub fn reaction(text: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(text.strip_prefix("__emoji__:")?).ok()?;
    (value["targetId"].as_str().is_some_and(|v| !v.is_empty())
        && value["emoji"].as_str().is_some_and(|v| !v.is_empty())
        && value["from"].as_str().is_some_and(|v| !v.is_empty()))
    .then_some(value)
}

pub async fn react(
    store: &SharedStore,
    api: &Api,
    chat: &str,
    message: &str,
    emoji: &str,
    key: Option<String>,
) -> Result<Value> {
    ensure!(
        !emoji.trim().is_empty() && emoji.len() <= 128 && !emoji.chars().any(char::is_control),
        "INVALID_EMOJI: Supply a non-empty emoji of at most 128 bytes"
    );
    let task = key.unwrap_or_else(crypto::id);
    let fingerprint =
        crypto::hash(json!({"chat":chat,"message":message,"emoji":emoji}).to_string());
    let target = {
        let s = store.lock().unwrap();
        if let Some(cached) = s.get("sent_reaction", &task)? {
            ensure!(
                cached["fingerprint"] == fingerprint,
                "IDEMPOTENCY_CONFLICT: Retry key already used"
            );
            return Ok(cached["result"].clone());
        }
        let row: Option<(String, Option<String>)> =
            s.db.query_row(
                "SELECT id,stream FROM messages WHERE chat=? AND id=?",
                rusqlite::params![chat, message],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (id, stream) =
            row.context("MESSAGE_NOT_FOUND: Message not found in this conversation")?;
        if stream.is_some() {
            s.get("message_transport", &id)?
                .and_then(|v| v.as_str().map(str::to_owned))
                .context(
                    "REACTION_TARGET_UNAVAILABLE: This older stream has no recorded transport ID",
                )?
        } else {
            id
        }
    };
    let body = format!(
        "__emoji__:{}",
        json!({"targetId":target,"emoji":emoji,"from":api.identity.public})
    );
    let result = messaging::send(store, api, chat, &body).await?;
    store.lock().unwrap().put(
        "sent_reaction",
        &task,
        &json!({"fingerprint":fingerprint,"result":result}),
    )?;
    Ok(result)
}

pub async fn send(store: &SharedStore, api: &Api, args: SendArgs) -> Result<Value> {
    let fingerprint = crypto::hash(serde_json::to_vec(&args)?);
    let task = args.idempotency_key.clone().unwrap_or_else(crypto::id);
    let cached = store.lock().unwrap().get("sent_command", &task)?;
    if let Some(cached) = cached {
        ensure!(
            cached["fingerprint"] == fingerprint,
            "IDEMPOTENCY_CONFLICT: 重试键已用于其他消息"
        );
        return Ok(cached["result"].clone());
    }
    ensure!(
        !args.text_stdin,
        "STDIN_REQUIRED: stdin 必须由调用端发送流式帧"
    );
    let text = if let Some(text) = args.text {
        ensure!(!text.is_empty(), "EMPTY_MESSAGE: 消息不能为空");
        Frame::final_text(text, args.reply_to).encode()?
    } else {
        let (path, kind) = if let Some(p) = args.img {
            (p, "image")
        } else if let Some(p) = args.video {
            (p, "video")
        } else {
            (args.file.context("缺少消息内容")?, "file")
        };
        let c = {
            store
                .lock()
                .unwrap()
                .get("chat", &args.id)?
                .context("CHAT_NOT_FOUND: 会话不存在")?
        };
        let recipients = if c["type"] == "group" {
            crate::group::recipients(store, api, &args.id).await?
        } else {
            vec![crypto::string(&c, "peer")?.to_owned()]
        };
        files::upload(store, api, &path, kind, &recipients, &task).await?
    };
    let result = messaging::send(store, api, &args.id, &text).await?;
    store.lock().unwrap().put(
        "sent_command",
        &task,
        &json!({"fingerprint":fingerprint,"result":result}),
    )?;
    let upload = { store.lock().unwrap().get("file_task", &task)? };
    if let Some(task) = upload {
        if let Some(path) = task["path"].as_str() {
            let path = std::path::Path::new(path);
            let root = store.lock().unwrap().dir.join("attachments");
            if path.parent() == Some(root.as_path()) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    Ok(result)
}
