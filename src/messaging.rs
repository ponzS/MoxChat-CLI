use crate::{
    api::Api,
    crypto::{self, string, Identity},
    store::Store,
    stream::Frame,
};
use anyhow::{ensure, Context, Result};
use reqwest::Method;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub type SharedStore = Arc<Mutex<Store>>;
pub fn bases(store: &SharedStore) -> Result<Vec<String>> {
    Ok(vec![crate::relays::selected(
        &store.lock().unwrap(),
        false,
    )?])
}
pub fn chat(store: &Store, peer: &str, name: &str) -> Result<String> {
    let key = format!("dm:{peer}");
    if store.get("chat", &key)?.is_none() {
        store.put(
            "chat",
            &key,
            &json!({"id":key,"type":"dm","peer":peer,"name":name}),
        )?;
    }
    Ok(key)
}
pub async fn publish(store: &SharedStore, api: &Api) -> Result<()> {
    let relays = bases(store)?;
    ensure!(!relays.is_empty(), "NO_RELAY: 请先使用 mox relay add <URL>");
    let mut mailboxes = vec![];
    for base in &relays {
        let a = api.advertisement(base).await?;
        let s = store.lock().unwrap();
        if let Some(pin) = s.get("pin", base)? {
            ensure!(
                pin["serverPub"] == a["serverPub"],
                "RELAY_IDENTITY_CHANGED: 中继身份已改变 {base}"
            );
        }
        s.put("pin", base, &a)?;
        mailboxes.push(json!({"nodeId":a["nodeId"],"serverPub":a["serverPub"],"baseUrl":base,"priority":mailboxes.len(),"validUntil":crypto::now()+7*86400000}));
    }
    let body = {
        let s = store.lock().unwrap();
        let i = s.identity()?;
        let previous = s.get("publication", "current")?;
        let now = crypto::now();
        let profile = crypto::sign_document(
            "mox:mesh:profile-document:v1",
            json!({"version":1,"ownerPub":i.public,"encryptionPub":i.epub,"name":i.name,"link":"","avatar":"","updatedAt":now}),
            &i.private,
        )?;
        let cid = hex::encode(crypto::digest(
            "mox:mesh:profile-document-cid:v1",
            &profile,
        )?);
        let route = crypto::sign_document(
            "mox:mesh:user-relay-route:v1",
            json!({"version":1,"ownerPub":i.public,"routeEpoch":previous.as_ref().and_then(|v|v["route"]["routeEpoch"].as_u64()).unwrap_or(0)+1,"mailboxes":mailboxes,"profileCid":cid,"issuedAt":now,"expiresAt":now+7*86400000}),
            &i.private,
        )?;
        let body = json!({"version":1,"profile":profile,"route":route});
        s.put("publication", "current", &body)?;
        body
    };
    for base in &relays {
        api.request(
            base,
            Method::PUT,
            &format!("{}?createSession=true", api.path("mesh/identity")),
            Some(&body),
        )
        .await?;
    }
    if !store.lock().unwrap().list("group")?.is_empty() {
        crate::group::publish_inbox(store, api).await?;
    }
    Ok(())
}
pub fn queue(s: &Store, envelope: &Value) -> Result<()> {
    s.db.execute(
        "INSERT OR IGNORE INTO outbox VALUES(?,?,?)",
        params![
            string(envelope, "envelopeId")?,
            envelope.to_string(),
            crypto::now()
        ],
    )?;
    Ok(())
}
pub async fn flush(store: &SharedStore, api: &Api) -> Result<()> {
    let pending = {
        let s = store.lock().unwrap();
        let mut st =
            s.db.prepare("SELECT id,value FROM outbox ORDER BY created LIMIT 16")?;
        let items = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        items
    };
    let relays = bases(store)?;
    for (id, text) in pending {
        let v: Value = serde_json::from_str(&text)?;
        if v["payload"]["sigAlg"] != crypto::CIPHERTEXT_SIGNATURE_ALG {
            let s = store.lock().unwrap();
            s.put(
                "failed_delivery",
                &id,
                &json!({"envelope":v,"error":"encrypted_payload_upgrade_required"}),
            )?;
            s.db.execute("DELETE FROM outbox WHERE id=?", [&id])?;
            s.event("delivery.rejected", json!({"envelopeId":id,"messageId":v["messageId"],"error":"encrypted_payload_upgrade_required"}))?;
            continue;
        }
        for base in &relays {
            match api
                .request(base, Method::POST, &api.path("mesh/envelopes"), Some(&v))
                .await
            {
                Ok(result) => {
                    ensure!(
                        result["type"] == "mesh_ingress"
                            && result["envelopeId"] == id
                            && (result["state"] == "queued" || result["state"] == "durable"),
                        "INVALID_RECEIPT: 中继未返回有效持久接收结果"
                    );
                    let s = store.lock().unwrap();
                    s.db.execute("DELETE FROM outbox WHERE id=?", [&id])?;
                    s.event(
                        "delivery.accepted",
                        json!({"envelopeId":id,"messageId":v["messageId"],"result":result}),
                    )?;
                    break;
                }
                Err(e) => {
                    store.lock().unwrap().put(
                        "runtime",
                        "relay_error",
                        &json!({"relay":base,"error":e.to_string(),"at":crypto::now()}),
                    )?;
                }
            }
        }
    }
    Ok(())
}
pub fn chat_signature_text(text: &str, message_id: &str, from: &str, to: &str) -> String {
    // Property order is part of the existing JSON.stringify signature contract.
    json!({"v":1,"text":text,"messageId":message_id,"fromPub":from,"toPub":to}).to_string()
}
pub fn record(
    s: &Store,
    chat_id: &str,
    sender: &str,
    transport_id: &str,
    text: &str,
    created: u64,
    status: &str,
) -> Result<Option<Value>> {
    let frame = Frame::parse(text)?;
    if let Some(f) = frame {
        let previous: Option<(String, String, u64, bool)> =
            s.db.query_row(
                "SELECT id,text,seq,terminal FROM messages WHERE chat=? AND sender=? AND stream=?",
                params![chat_id, sender, f.stream_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        if let Some((id, _, _, _)) = &previous {
            if let Some(meta) = s.get("stream_state", id)? {
                ensure!(
                    meta["format"] == f.format
                        && meta["replyTo"] == serde_json::to_value(&f.reply_to)?,
                    "STREAM_CONFLICT: 流式格式或引用目标改变"
                );
            }
        }
        let key = format!("{chat_id}:{sender}:{}:{}", f.stream_id, f.seq);
        let fingerprint = crypto::hash(text);
        if let Some(seen) = s.get("stream_frame", &key)? {
            ensure!(seen == fingerprint, "STREAM_CONFLICT: 相同序号的帧内容冲突");
            return Ok(None);
        }
        s.put("stream_frame", &key, &json!(fingerprint))?;
        let (logical, body) = if let Some((id, old, seq, terminal)) = previous {
            if terminal || f.seq <= seq {
                return Ok(None);
            }
            if f.kind == "delta" && f.base_seq != Some(seq) {
                return Ok(None);
            }
            (
                id,
                if f.kind == "delta" {
                    old + &f.text
                } else {
                    f.text.clone()
                },
            )
        } else {
            if f.kind == "delta" {
                return Ok(None);
            }
            (
                format!(
                    "stream:{}:{}",
                    crypto::hash(format!("{chat_id}:{sender}")),
                    f.stream_id
                ),
                f.text.clone(),
            )
        };
        ensure!(body.len() <= crate::stream::MAX_TEXT, "流式正文超出限制");
        let inserted=s.db.execute("INSERT OR IGNORE INTO messages(id,chat,sender,text,created,status,stream,seq,terminal) VALUES(?,?,?,?,?,?,?,?,?)",params![logical,chat_id,sender,body,created,status,f.stream_id,f.seq,f.terminal()])?>0;
        if !inserted {
            s.db.execute(
                "UPDATE messages SET text=?,seq=?,terminal=?,status=? WHERE id=?",
                params![body, f.seq, f.terminal(), status, logical],
            )?;
        }
        if inserted {
            s.put("message_transport", &logical, &json!(transport_id))?;
        }
        s.put("stream_state", &logical, &serde_json::to_value(&f)?)?;
        let v = json!({"id":logical,"chat":chat_id,"sender":sender,"text":body,"format":f.format,"streamId":f.stream_id,"streamState":f.kind,"lastSeq":f.seq,"replyTo":f.reply_to});
        s.event(
            if inserted {
                "message.created"
            } else {
                "message.updated"
            },
            v.clone(),
        )?;
        if f.kind == "final" {
            Ok(Some(v))
        } else {
            Ok(None)
        }
    } else {
        let reaction = crate::outgoing::reaction(text);
        if let Some(value) = &reaction {
            ensure!(value["from"] == sender, "REACTION_SENDER_MISMATCH: Reaction sender does not match its authenticated envelope");
        }
        let inserted=s.db.execute("INSERT OR IGNORE INTO messages(id,chat,sender,text,created,status) VALUES(?,?,?,?,?,?)",params![transport_id,chat_id,sender,text,created,status])?>0;
        if !inserted {
            return Ok(None);
        }
        if let Some(reaction) = reaction {
            s.event(
                "message.reaction",
                json!({"id":transport_id,"chat":chat_id,"sender":sender,"reaction":reaction}),
            )?;
            return Ok(None);
        }
        let v =
            json!({"id":transport_id,"chat":chat_id,"sender":sender,"text":text,"format":"plain"});
        s.event("message.created", v.clone())?;
        Ok(Some(v))
    }
}
pub async fn send(store: &SharedStore, api: &Api, chat_id: &str, text: &str) -> Result<Value> {
    let group = {
        store
            .lock()
            .unwrap()
            .get("chat", chat_id)?
            .is_some_and(|v| v["type"] == "group")
    };
    if group {
        return crate::group::send(store, api, chat_id, text).await;
    }
    send_dm(store, api, chat_id, text).await
}
pub async fn send_dm(store: &SharedStore, api: &Api, chat_id: &str, text: &str) -> Result<Value> {
    let (peer, recipient_epub) = {
        let s = store.lock().unwrap();
        let c = s
            .get("chat", chat_id)?
            .context("CHAT_NOT_FOUND: 会话不存在")?;
        ensure!(c["type"] == "dm", "该会话需要群 V2 发送通道");
        let peer = string(&c, "peer")?.to_owned();
        ensure!(s.get("blocked", &peer)?.is_none(), "好友已屏蔽");
        // A verified friend receipt/key rotation already supplies the current
        // encryption key. Streaming frames must not re-resolve the profile.
        let friend_epub = s
            .get("friend", &peer)?
            .and_then(|v| v["epub"].as_str().map(str::to_owned));
        let epub = match friend_epub {
            Some(epub) => Some(epub),
            None => s
                .get("profile", &peer)?
                .and_then(|v| v["encryptionPub"].as_str().map(str::to_owned)),
        };
        (peer, epub)
    };
    let mid = crate::stream::transport_id(text, &format!("{}:{chat_id}", api.identity.public))?;
    if let Some(result) = crate::stream::prepared(&store.lock().unwrap(), &mid, text)? {
        return Ok(result);
    }
    let recipient_epub = if let Some(epub) = recipient_epub {
        epub
    } else {
        let profile = api.resolve(&bases(store)?, &peer).await?["profile"].clone();
        let epub = string(&profile, "encryptionPub")?.to_owned();
        store.lock().unwrap().put("profile", &peer, &profile)?;
        epub
    };
    let now = crypto::now();
    let i = &api.identity;
    let sig = crypto::text_signature(
        &chat_signature_text(text, &mid, &i.public, &peer),
        &i.private,
    )?;
    let plain = format!(
        "__relay_meta__:{}",
        json!({"text":text,"relayUrls":bases(store)?,"messageId":mid,"signature":{"v":1,"sig":sig,"sigAlg":"p256-sha256"}})
    );
    let env = crypto::envelope(
        i,
        &peer,
        &mid,
        "dm",
        crypto::SignedBody { text: &plain },
        &recipient_epub,
        now,
    )?;
    let s = store.lock().unwrap();
    s.db.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        queue(&s, &env)?;
        if !text.starts_with("__group_v2_control__:") && !text.starts_with("__dm_key_rotate__:") {
            record(&s, chat_id, &i.public, &mid, text, now, "queued")?;
        }
        let result = json!({"id":mid,"chat":chat_id,"status":"queued"});
        crate::stream::save_prepared(&s, &mid, text, &result)?;
        Ok::<_, anyhow::Error>(result)
    })();
    match &result {
        Ok(_) => s.db.execute_batch("COMMIT")?,
        Err(_) => s.db.execute_batch("ROLLBACK")?,
    };
    result
}
pub async fn add_friend(
    store: &SharedStore,
    api: &Api,
    peer: &str,
    message: &str,
) -> Result<Value> {
    ensure!(peer != api.identity.public, "不能添加自己为好友");
    ensure!(message.chars().count() <= 120, "申请消息不能超过 120 字符");
    let resolved = api.resolve(&bases(store)?, peer).await?;
    let profile = &resolved["profile"];
    let i = &api.identity;
    let rid = crypto::id();
    let now = crypto::now();
    let doc = crypto::sign_document(
        "mox:mesh:friend-request-document:v1",
        json!({"version":1,"kind":"friend_request","requestId":rid,"fromPub":i.public,"toPub":peer,"senderEncryptionPub":i.epub,"message":message,"createdAt":now}),
        &i.private,
    )?;
    let env = crypto::envelope(
        i,
        peer,
        &rid,
        "friend_request",
        crypto::SignedBody {
            text: &serde_jcs::to_string(&doc)?,
        },
        string(profile, "encryptionPub")?,
        now,
    )?;
    let item = json!({"id":rid,"direction":"outgoing","state":"pending","peer":peer,"document":doc,"envelope":env});
    let s = store.lock().unwrap();
    s.put("profile", peer, profile)?;
    s.put("request", &rid, &item)?;
    queue(&s, &env)?;
    s.event("friend.requested", item.clone())?;
    Ok(item)
}
pub fn friend_decision(s: &Store, i: &Identity, rid: &str, kind: &str) -> Result<Value> {
    let mut request = s
        .get("request", rid)?
        .context("REQUEST_NOT_FOUND: 好友申请不存在")?;
    let outgoing = kind == "friend_cancel";
    ensure!(
        request["direction"] == if outgoing { "outgoing" } else { "incoming" },
        "申请方向错误"
    );
    let state = match kind {
        "friend_receipt" => "accepted",
        "friend_reject" => "rejected",
        _ => "cancelled",
    };
    if request["state"] == state {
        return Ok(request);
    }
    ensure!(request["state"] == "pending", "好友申请已结束");
    let peer = string(&request, "peer")?.to_owned();
    let now = crypto::now();
    let v = if kind == "friend_receipt" { 2 } else { 1 };
    let domain = format!("mox:mesh:{}-document:v{v}", kind.replace('_', "-"));
    let doc = crypto::sign_document(
        &domain,
        json!({"version":v,"kind":kind,"requestId":rid,"requestEnvelopeId":request["envelope"]["envelopeId"],"fromPub":i.public,"toPub":peer,"senderEncryptionPub":i.epub,"createdAt":now}),
        &i.private,
    )?;
    let epub = if outgoing {
        s.get("profile", &peer)?
            .context("Missing recipient profile")?["encryptionPub"]
            .as_str()
            .context("Missing encryption key")?
            .to_owned()
    } else {
        string(&request["document"], "senderEncryptionPub")?.to_owned()
    };
    let env = crypto::envelope(
        i,
        &peer,
        rid,
        kind,
        crypto::SignedBody {
            text: &serde_jcs::to_string(&doc)?,
        },
        &epub,
        now,
    )?;
    queue(s, &env)?;
    request["state"] = json!(state);
    s.put("request", rid, &request)?;
    if kind == "friend_receipt" {
        let name = s
            .get("profile", &peer)?
            .and_then(|p| p["name"].as_str().map(str::to_owned))
            .unwrap_or_else(|| peer.clone());
        let cid = chat(s, &peer, &name)?;
        s.put(
            "friend",
            &peer,
            &json!({"pub":peer,"name":name,"chat":cid,"epub":epub,"requestId":rid}),
        )?;
    }
    s.event("friend.updated", request.clone())?;
    Ok(request)
}
fn receive_control(s: &Store, i: &Identity, env: &Value) -> Result<()> {
    crypto::verify_envelope(env)?;
    ensure!(env["toPub"] == i.public, "好友控制消息目标错误");
    ensure!(
        env["expiresAt"].as_u64().unwrap_or(0) > crypto::now(),
        "好友控制消息过期"
    );
    let kind = string(env, "kind")?;
    let peer = string(env, "fromPub")?;
    let rid = string(env, "messageId")?;
    let text = crypto::decrypt(
        &env["payload"],
        &i.epriv,
        string(&env["payload"], "sender")?,
    )?;
    let doc = crypto::json_strict(&text)?;
    ensure!(
        doc["fromPub"] == peer
            && doc["toPub"] == i.public
            && doc["kind"] == kind
            && doc["requestId"] == rid
            && doc["senderEncryptionPub"] == env["payload"]["sender"]
            && (env["payload"]["sigAlg"] == crypto::CIPHERTEXT_SIGNATURE_ALG
                || doc["signature"] == env["payload"]["signature"])
            && doc["createdAt"] == env["createdAt"],
        "好友控制消息绑定错误"
    );
    let v = doc["version"].as_u64().context("Invalid friend document")?;
    ensure!(
        v == if kind == "friend_receipt" { 2 } else { 1 },
        "不支持的好友控制协议版本"
    );
    crypto::verify_document(
        &format!("mox:mesh:{}-document:v{v}", kind.replace('_', "-")),
        &doc,
        peer,
    )?;
    if s.get("blocked", peer)?.is_some() {
        return Ok(());
    }
    if kind == "friend_request" {
        if s.get("request", rid)?.is_some() {
            return Ok(());
        }
        let request = json!({"id":rid,"direction":"incoming","state":"pending","peer":peer,"document":doc,"envelope":env});
        s.put("request", rid, &request)?;
        s.event("friend.requested", request)?;
        for kind in ["friend_receipt", "friend_reject", "friend_cancel"] {
            let key = format!("{rid}:{kind}");
            if let Some(pending) = s.get("pending_friend_control", &key)? {
                receive_control(s, i, &pending)?;
                s.delete("pending_friend_control", &key)?;
            }
        }
    } else {
        let Some(mut request) = s.get("request", rid)? else {
            s.put("pending_friend_control", &format!("{rid}:{kind}"), env)?;
            return Ok(());
        };
        ensure!(
            request["peer"] == peer
                && doc["requestEnvelopeId"] == request["envelope"]["envelopeId"],
            "好友控制消息关联错误"
        );
        ensure!(
            request["direction"]
                == if kind == "friend_cancel" {
                    "incoming"
                } else {
                    "outgoing"
                },
            "好友控制消息方向错误"
        );
        let previous = request["state"].as_str().unwrap_or("pending");
        if previous == "cancelled" || (previous == "rejected" && kind == "friend_receipt") {
            return Ok(());
        }
        request["state"] = json!(match kind {
            "friend_receipt" => "accepted",
            "friend_reject" => "rejected",
            "friend_cancel" => "cancelled",
            _ => anyhow::bail!("无效好友控制消息"),
        });
        s.put("request", rid, &request)?;
        if kind == "friend_receipt" {
            let profile = s.get("profile", peer)?.context("Missing friend profile")?;
            let cid = chat(s, peer, string(&profile, "name")?)?;
            s.put("friend",peer,&json!({"pub":peer,"name":profile["name"],"chat":cid,"epub":doc["senderEncryptionPub"],"requestId":rid}))?;
        }
        if kind != "friend_receipt"
            && s.get("friend", peer)?
                .is_some_and(|v| v["requestId"] == rid)
        {
            s.delete("friend", peer)?;
        }
        s.event("friend.updated", request)?;
    }
    Ok(())
}
pub async fn sync(store: &SharedStore, api: &Api, base: &str) -> Result<()> {
    let controls = api
        .request(
            base,
            Method::GET,
            &api.path("mesh/friend-controls?limit=100"),
            None,
        )
        .await?;
    for item in controls["items"].as_array().into_iter().flatten() {
        let env = &item["envelope"];
        let dedupe = format!("control:{}", string(env, "envelopeId")?);
        {
            let s = store.lock().unwrap();
            let seen =
                s.db.query_row("SELECT 1 FROM received WHERE id=?", [&dedupe], |r| {
                    r.get::<_, i32>(0)
                })
                .optional()?
                .is_some();
            if !seen {
                s.db.execute_batch("BEGIN IMMEDIATE")?;
                let result = receive_control(&s, &api.identity, env);
                match result {
                    Ok(()) => {
                        s.db.execute(
                            "INSERT INTO received VALUES(?,?)",
                            params![dedupe, crypto::now()],
                        )?;
                        s.db.execute_batch("COMMIT")?
                    }
                    Err(e) => {
                        s.db.execute_batch("ROLLBACK")?;
                        s.put(
                            "quarantine",
                            &dedupe,
                            &json!({"reason":e.to_string(),"envelope":env}),
                        )?;
                    }
                }
            }
        }
        api.request(base,Method::POST,&api.path("mesh/friend-controls/acks"),Some(&json!({"items":[{"kind":env["kind"],"fromPub":env["fromPub"],"messageId":env["messageId"],"envelopeId":env["envelopeId"]}]}))).await?;
    }
    let response = api
        .request(base, Method::GET, &api.path("dm/messages?limit=100"), None)
        .await?;
    for item in response["items"].as_array().into_iter().flatten() {
        let result = receive_dm(store, api, item).await;
        if let Err(e) = result {
            store.lock().unwrap().put(
                "quarantine",
                string(item, "id")?,
                &json!({"reason":e.to_string(),"item":item}),
            )?;
        }
        api.request(
            base,
            Method::POST,
            &api.path("dm/acks"),
            Some(&json!({"ids":[item["id"]]})),
        )
        .await?;
    }
    Ok(())
}
async fn receive_dm(store: &SharedStore, api: &Api, item: &Value) -> Result<()> {
    let i = &api.identity;
    let peer = string(item, "fromPub")?;
    ensure!(item["toPub"] == i.public, "消息收件人不匹配");
    let (friend, blocked) = {
        let s = store.lock().unwrap();
        (s.get("friend", peer)?, s.get("blocked", peer)?.is_some())
    };
    if blocked {
        return Ok(());
    }
    ensure!(friend.is_some(), "NOT_FRIEND: 消息来自未接受的好友");
    let payload = &item["payload"];
    let plain = crypto::decrypt(payload, &i.epriv, string(payload, "sender")?)?;
    let meta = crypto::json_strict(
        plain
            .strip_prefix("__relay_meta__:")
            .context("缺少已签名消息包装")?,
    )?;
    let text = string(&meta, "text")?;
    let mid = string(&meta, "messageId")?;
    ensure!(
        payload["messageId"] == mid
            && (payload["sigAlg"] == "p256-sha256"
                || payload["sigAlg"] == crypto::CIPHERTEXT_SIGNATURE_ALG)
            && meta["signature"]["v"] == 1
            && meta["signature"]["sigAlg"] == "p256-sha256"
            && (payload["sigAlg"] == crypto::CIPHERTEXT_SIGNATURE_ALG
                || meta["signature"]["sig"] == payload["signature"]),
        "消息签名元数据不匹配"
    );
    if payload["sigAlg"] == crypto::CIPHERTEXT_SIGNATURE_ALG {
        crypto::verify_ciphertext_payload(payload, peer, &i.public)?;
    }
    crypto::verify_text(
        &chat_signature_text(text, mid, peer, &i.public),
        string(&meta["signature"], "sig")?,
        peer,
    )?;
    let s = store.lock().unwrap();
    let key = format!("dm:{peer}:{mid}");
    if s.db
        .query_row("SELECT 1 FROM received WHERE id=?", [&key], |r| {
            r.get::<_, i32>(0)
        })
        .optional()?
        .is_some()
    {
        return Ok(());
    }
    let cid = chat(
        &s,
        peer,
        friend
            .as_ref()
            .and_then(|v| v["name"].as_str())
            .unwrap_or(peer),
    )?;
    s.db.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        let rotation = if let Some(raw) = text.strip_prefix("__dm_key_rotate__:") {
            let doc = crypto::json_strict(raw)?;
            ensure!(
                doc["v"] == 1 && doc["type"] == "mox.dm.key.rotate",
                "无效私聊密钥轮换"
            );
            let epub = string(&doc, "epub")?;
            // Validate the curve point before accepting a remotely advertised key.
            crypto::encrypt("", &i.epriv, epub)?;
            let at = doc["createdAt"].as_u64().context("无效轮换时间")?;
            let old = s.get("remote_dm_key", peer)?;
            if old
                .as_ref()
                .and_then(|v| v["createdAt"].as_u64())
                .unwrap_or(0)
                < at
            {
                let mut friend = friend.clone().context("NOT_FRIEND")?;
                friend["epub"] = json!(epub);
                s.put("friend", peer, &friend)?;
                s.put("remote_dm_key", peer, &doc)?;
            }
            true
        } else {
            false
        };
        let control = rotation || crate::group::receive_control(&s, peer, text)?;
        let message = if control {
            None
        } else {
            record(
                &s,
                &cid,
                peer,
                mid,
                text,
                item["createdAt"].as_u64().unwrap_or_else(crypto::now),
                "received",
            )?
        };
        s.db.execute(
            "INSERT INTO received VALUES(?,?)",
            params![key, crypto::now()],
        )?;
        if let Some(message) = message {
            s.put(
                "ai_queue",
                &format!("{:016}:{}", crypto::now(), mid),
                &message,
            )?;
        }
        Ok::<_, anyhow::Error>(())
    })();
    match &result {
        Ok(_) => s.db.execute_batch("COMMIT")?,
        Err(_) => s.db.execute_batch("ROLLBACK")?,
    };
    result
}
