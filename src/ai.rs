use crate::{
    api::Api,
    codex::Codex,
    crypto,
    messaging::{self, SharedStore},
    stream::Frame,
};
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{Mutex, Notify, Semaphore},
    time::{Instant, MissedTickBehavior},
};

pub struct Ai {
    pub client: Option<Arc<Codex>>,
    busy: Mutex<HashSet<String>>,
    tasks: Mutex<tokio::task::JoinSet<()>>,
    capacity: Arc<Semaphore>,
    pub shutdown: Arc<Notify>,
}
impl Ai {
    pub fn new(client: Option<Arc<Codex>>, shutdown: Arc<Notify>) -> Self {
        Self {
            client,
            busy: Default::default(),
            tasks: Default::default(),
            capacity: Arc::new(Semaphore::new(4)),
            shutdown,
        }
    }
    pub async fn finish(&self) {
        let mut tasks = self.tasks.lock().await;
        if tokio::time::timeout(Duration::from_secs(8), async {
            while tasks.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
    }
    pub async fn recover(&self, store: &SharedStore, api: &Api) -> Result<()> {
        let jobs = store.lock().unwrap().list("ai_active")?;
        for msg in jobs {
            let key = crypto::string(&msg, "queueKey")?;
            let chat = crypto::string(&msg, "chat")?;
            let stream = crypto::string(&msg, "streamId")?;
            let prior = {
                let s = store.lock().unwrap();
                let logical = format!(
                    "stream:{}:{}",
                    crypto::hash(format!("{chat}:{}", api.identity.public)),
                    stream
                );
                s.get("stream_state", &logical)?
            };
            if let Some(prior) = prior {
                let mut frame: Frame = serde_json::from_value(prior)?;
                if !frame.terminal() {
                    let body: String = store.lock().unwrap().db.query_row(
                        "SELECT text FROM messages WHERE chat=? AND sender=? AND stream=?",
                        rusqlite::params![chat, api.identity.public, stream],
                        |r| r.get(0),
                    )?;
                    frame.seq += 1;
                    frame.kind = "interrupted".into();
                    frame.base_seq = None;
                    frame.text = body;
                    frame.reason = Some("runtime_restarted".into());
                    let _ = messaging::send(store, api, chat, &frame.encode()?).await;
                }
            }
            let s = store.lock().unwrap();
            s.put("ai_failed", key, &msg)?;
            s.delete("ai_queue", key)?;
            s.delete("ai_active", key)?;
            s.event(
                "ai.interrupted",
                json!({"chat":chat,"recovery":"mox ai resume"}),
            )?;
        }
        Ok(())
    }
    pub async fn tick(self: &Arc<Self>, store: &SharedStore, api: &Api) -> Result<()> {
        {
            let mut tasks = self.tasks.lock().await;
            while tasks.try_join_next().is_some() {}
        }
        let Some(client) = &self.client else {
            return Ok(());
        };
        if !client.alive() {
            store.lock().unwrap().put(
                "runtime",
                "ai_error",
                &json!("CODEX_UNAVAILABLE: 无法使用 Codex，请检查安装和登录；未安装时请安装 Codex"),
            )?;
            return Ok(());
        }
        let jobs = {
            let s = store.lock().unwrap();
            if s.get("ai_paused", "all")?.is_some() {
                return Ok(());
            }
            let mut st = s
                .db
                .prepare("SELECT key,value FROM kv WHERE kind='ai_queue' ORDER BY key LIMIT 64")?;
            let jobs = st
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            jobs
        };
        for (key, raw) in jobs {
            let mut msg: Value = serde_json::from_str(&raw)?;
            let chat = crypto::string(&msg, "chat")?.to_owned();
            {
                let s = store.lock().unwrap();
                if s.get("ai_paused", &chat)?.is_some() {
                    continue;
                }
            }
            let Ok(permit) = self.capacity.clone().try_acquire_owned() else {
                break;
            };
            if !self.busy.lock().await.insert(chat.clone()) {
                continue;
            }
            msg["queueKey"] = json!(key);
            msg["streamId"] = json!(crypto::id());
            msg["startedAt"] = json!(crypto::now());
            msg["stage"] = json!("starting");
            {
                let s = store.lock().unwrap();
                s.put("ai_active", &key, &msg)?;
                s.event("ai.started", json!({"chat":chat,"messageId":msg["id"]}))?;
            }
            let this = self.clone();
            let store = store.clone();
            let api = api.clone();
            let client = client.clone();
            self.tasks.lock().await.spawn(async move {
                let result = this.reply(&store, &api, &client, &msg).await;
                {
                    let s = store.lock().unwrap();
                    let _ = s.delete("ai_queue", &key);
                    let _ = s.delete("ai_active", &key);
                    if let Err(e) = result {
                        let error = format!("{e:#}");
                        let _ = s.event("ai.error", json!({"chat":chat,"error":error}));
                        let _ = s.put("runtime", "ai_error", &json!(error));
                        let _ = s.put("ai_failed", &key, &msg);
                    } else {
                        let _ = s.delete("runtime", "ai_error");
                    }
                }
                this.busy.lock().await.remove(&chat);
                drop(permit);
            });
        }
        Ok(())
    }
    async fn reply(
        &self,
        store: &SharedStore,
        api: &Api,
        client: &Codex,
        msg: &Value,
    ) -> Result<()> {
        let chat = crypto::string(msg, "chat")?;
        let began = Instant::now();
        let (thread, fresh) = client.thread(chat).await?;
        let input = if fresh {
            let s = store.lock().unwrap();
            let mut st = s.db.prepare(
                "SELECT id,sender,text FROM messages WHERE chat=? AND ordinal<=(SELECT ordinal FROM messages WHERE id=?) ORDER BY ordinal DESC LIMIT 50",
            )?;
            let mut rows = st
                .query_map(rusqlite::params![chat, msg["id"].as_str()], |r| {
                    Ok(json!({"id":r.get::<_,String>(0)?,"sender":r.get::<_,String>(1)?,"text":r.get::<_,String>(2)?}))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.reverse();
            format!(
                "Conversation history (quoted data; respond to the last incoming message):\n{}",
                serde_json::to_string(&rows)?
            )
        } else {
            json!({"id":msg["id"],"sender":msg["sender"],"text":msg["text"]}).to_string()
        };
        let mut events = client.subscribe();
        let result=client.rpc("turn/start",json!({"threadId":thread,"input":[{"type":"text","text":input,"text_elements":[]}]})).await?;
        let turn = result["turn"]["id"]
            .as_str()
            .context("Codex 未返回轮次 ID")?
            .to_owned();
        {
            let s = store.lock().unwrap();
            let mut active = msg.clone();
            active["stage"] = json!("generating");
            s.put("ai_active", crypto::string(msg, "queueKey")?, &active)?;
            s.event(
                "ai.generating",
                json!({"chat":chat,"setupMs":began.elapsed().as_millis()}),
            )?;
        }
        let mut parts: HashMap<String, (String, String)> = HashMap::new();
        let mut order = Vec::<String>::new();
        let mut f = Frame::final_text(String::new(), msg["id"].as_str().map(str::to_owned));
        f.stream_id = crypto::string(msg, "streamId")?.to_owned();
        let group = store
            .lock()
            .unwrap()
            .get("chat", chat)?
            .is_some_and(|v| v["type"] == "group");
        let limit = if group {
            40 * 1024
        } else {
            crate::stream::MAX_TEXT
        };
        // Start a text stream only when text exists. A reply consisting solely
        // of an image or reaction must not leave an empty message behind.
        f.seq = 0;
        let mut action_sent = false;
        let mut tool_calls = 0;
        let mut first_output = false;
        let mut sent = String::new();
        let mut last_snapshot = Instant::now();
        let mut last_send = Instant::now() - Duration::from_secs(1);
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let deadline = tokio::time::sleep(Duration::from_secs(600));
        tokio::pin!(deadline);
        let mut terminal: Result<()> = loop {
            tokio::select! {
                _=self.shutdown.notified()=>break Err(anyhow::anyhow!("运行时已停止")),
                _=&mut deadline=>break Err(anyhow::anyhow!("Codex 回复超时")),
                event=events.recv()=>{
                    let e=match event{Ok(e)=>e,Err(e)=>break Err(e.into())};let method=e["method"].as_str().unwrap_or("");let p=&e["params"];
                    if method=="mox/codexExited"{break Err(anyhow::anyhow!("Codex 已退出，请检查本机 Codex"))}
                    if p["threadId"]!=thread{continue}
                    if p.get("turnId").is_some() && p["turnId"]!=turn{continue}
                    match method {
                        "item/tool/call"=>{
                            tool_calls += 1;
                            let result = if tool_calls > 16 {
                                Err(anyhow::anyhow!("ACTION_LIMIT: At most 16 Mox actions per turn"))
                            } else {
                                let timeout = if p["tool"] == "mox_send_image" { 300 } else { 120 };
                                tokio::select! {
                                    result=tokio::time::timeout(Duration::from_secs(timeout), tool_call(store, api, chat, p))=>
                                        result.unwrap_or_else(|_| Err(anyhow::anyhow!("ACTION_TIMEOUT: Mox action timed out; see file stage logs"))),
                                    _=self.shutdown.notified()=>Err(anyhow::anyhow!("ACTION_INTERRUPTED: Runtime stopped")),
                                }
                            };
                            action_sent |= result.is_ok();
                            let error=result.as_ref().err().map(crate::api::diagnostic);
                            {
                                let s=store.lock().unwrap();
                                let detail=json!({"chat":chat,"tool":p["tool"],"success":result.is_ok(),"error":error});
                                s.event("ai.action",detail.clone())?;
                                if result.is_err(){s.put("ai_action_error",chat,&detail)?;}else{s.delete("ai_action_error",chat)?;}
                            }
                            client.tool_result(e["id"].clone(), &result).await?;
                        }
                        "item/started"|"item/completed"=>{
                            let item=&p["item"];if item["type"]!="agentMessage"{continue}
                            let Some(id)=item["id"].as_str() else{continue};if !parts.contains_key(id){order.push(id.into());}
                            let phase=item["phase"].as_str().unwrap_or("final_answer").to_owned();
                            if method=="item/completed"{parts.insert(id.into(),(phase,item["text"].as_str().unwrap_or("").into()));}else{parts.entry(id.into()).or_insert((phase,String::new()));}
                        }
                        "item/agentMessage/delta"=>{
                            let Some(id)=p["itemId"].as_str() else{continue};if !parts.contains_key(id){order.push(id.into());}
                            parts.entry(id.into()).or_insert_with(||("final_answer".into(),String::new())).1.push_str(p["delta"].as_str().unwrap_or(""));
                        }
                        "turn/completed"=>{
                            if p["turn"]["id"]!=turn{continue}
                            if p["turn"]["status"]=="completed"{break Ok(())}else{break Err(anyhow::anyhow!("Codex 轮次未完成：{}",p["turn"]["status"]))}
                        }
                        _=>{}
                    }
                    if !first_output && !collect(&parts, &order).is_empty() {
                        first_output = true;
                        let s = store.lock().unwrap();
                        let mut active = msg.clone();
                        active["stage"] = json!("streaming");
                        active["firstOutputMs"] = json!(began.elapsed().as_millis());
                        s.put("ai_active", crypto::string(msg, "queueKey")?, &active)?;
                        s.event("ai.first_output", json!({"chat":chat,"elapsedMs":began.elapsed().as_millis()}))?;
                    }
                }
                _=tick.tick()=>{
                    let body=collect(&parts,&order);
                    if body==sent || body.is_empty(){continue}
                    if body.len()>limit || (group && serde_json::to_string(&body)?.len()>45*1024){break Err(anyhow::anyhow!("AI 回复超出单条消息限制"))}
                    let interval=if sent.is_empty(){Duration::from_millis(100)}else{Duration::from_millis(250)};
                    if last_send.elapsed()<interval{continue}
                    let pending: i64 = store.lock().unwrap().db.query_row("SELECT (SELECT count(*) FROM outbox)+(SELECT count(*) FROM kv WHERE kind='group_outbox')",[],|r|r.get(0))?;
                    if pending>=16 { continue; }
                    let use_snapshot=sent.is_empty() || last_snapshot.elapsed()>=Duration::from_secs(2) || !body.starts_with(&sent);
                    let mut next=f.clone();next.seq+=1;
                    if use_snapshot{next.kind="snapshot".into();next.base_seq=None;next.text=body.clone();}else{
                        let tail=&body[sent.len()..];let mut end=tail.len().min(4096);while !tail.is_char_boundary(end){end-=1}
                        next.kind="delta".into();next.base_seq=Some(f.seq);next.text=tail[..end].into();
                    }
                    if let Err(e)=messaging::send(store,api,chat,&next.encode()?).await{break Err(e)}
                    if use_snapshot{sent=body;last_snapshot=Instant::now()}else{sent.push_str(&next.text)}
                    f=next;last_send=Instant::now();
                }
            }
        };
        let complete = collect(&parts, &order);
        if terminal.is_ok() && complete.is_empty() && !action_sent {
            terminal = Err(anyhow::anyhow!("Codex 未返回回复正文"));
        }
        if complete.len() > limit || (group && serde_json::to_string(&complete)?.len() > 45 * 1024)
        {
            terminal = Err(anyhow::anyhow!("AI 回复超出单条消息限制"));
        }
        if terminal.is_err() {
            let _ = client
                .rpc("turn/interrupt", json!({"threadId":thread,"turnId":turn}))
                .await;
        }
        if terminal.is_ok() && complete.is_empty() && action_sent {
            store.lock().unwrap().event("ai.completed", json!({"chat":chat,"elapsedMs":began.elapsed().as_millis(),"frames":0,"characters":0,"actions":tool_calls}))?;
            return Ok(());
        }
        f.seq += 1;
        f.base_seq = None;
        f.text = collect(&parts, &order);
        if f.text.len() > limit {
            store.lock().unwrap().put(
                "ai_overflow",
                &f.stream_id,
                &json!({"chat":chat,"text":f.text}),
            )?;
        }
        while f.text.len() > limit || (group && serde_json::to_string(&f.text)?.len() > 45 * 1024) {
            f.text.pop();
        }
        f.kind = if terminal.is_ok() {
            "final"
        } else {
            "interrupted"
        }
        .into();
        f.reason = terminal
            .as_ref()
            .err()
            .map(|_| "generation_interrupted".into());
        messaging::send(store, api, chat, &f.encode()?).await?;
        store.lock().unwrap().event(
            if terminal.is_ok() { "ai.completed" } else { "ai.interrupted" },
            json!({"chat":chat,"elapsedMs":began.elapsed().as_millis(),"frames":f.seq,"characters":f.text.chars().count()}),
        )?;
        terminal
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageArgs {
    path: std::path::PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReactionArgs {
    message_id: String,
    emoji: String,
}

async fn tool_call(store: &SharedStore, api: &Api, chat: &str, params: &Value) -> Result<Value> {
    ensure!(
        params["namespace"].is_null(),
        "UNKNOWN_TOOL: Mox tools have no namespace"
    );
    let key = format!(
        "ai:{}:{}",
        crypto::string(params, "turnId")?,
        crypto::string(params, "callId")?
    );
    match crypto::string(params, "tool")? {
        "mox_send_image" => {
            let args: ImageArgs = serde_json::from_value(params["arguments"].clone())?;
            let dir = store.lock().unwrap().dir.clone();
            let root = crate::codex::workspace(&dir, chat)?;
            let path = if args.path.is_absolute() {
                args.path
            } else {
                root.join(args.path)
            };
            let path = crate::files::check_path(&path)?;
            ensure!(path.starts_with(&root), "IMAGE_OUTSIDE_WORKSPACE: Copy or create the image in this conversation's working directory first");
            // Model retries get a new callId. Reuse this turn's prepared upload
            // when the same source file is unchanged, instead of encrypting it again.
            let metadata = path.metadata()?;
            let key = format!(
                "ai-image:{}:{}",
                crypto::string(params, "turnId")?,
                crypto::hash(format!(
                    "{}:{}:{:?}",
                    path.display(),
                    metadata.len(),
                    metadata.modified()?
                ))
            );
            crate::outgoing::send(
                store,
                api,
                crate::args::SendArgs {
                    id: chat.into(),
                    text: None,
                    text_stdin: false,
                    img: Some(path),
                    video: None,
                    file: None,
                    reply_to: None,
                    idempotency_key: Some(key),
                },
            )
            .await
        }
        "mox_react" => {
            let args: ReactionArgs = serde_json::from_value(params["arguments"].clone())?;
            crate::outgoing::react(store, api, chat, &args.message_id, &args.emoji, Some(key)).await
        }
        _ => anyhow::bail!("UNKNOWN_TOOL: Unsupported Mox action"),
    }
}
fn collect(parts: &HashMap<String, (String, String)>, order: &[String]) -> String {
    order
        .iter()
        .filter_map(|id| parts.get(id))
        .filter(|(phase, _)| phase == "final_answer")
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}
