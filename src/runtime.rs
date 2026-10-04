use crate::{
    ai::Ai,
    api::Api,
    args::*,
    codex::Codex,
    crypto,
    language::Language,
    messaging::{self, SharedStore},
    store::{self, Store},
    stream::Frame,
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::Notify,
};

#[derive(Serialize, Deserialize)]
pub enum Request {
    Command(Command),
    Frame { id: String, frame: Frame },
}
pub struct Runtime {
    pub store: SharedStore,
    pub api: Api,
    ai: Arc<Ai>,
    ai_enabled: bool,
    stop: AtomicBool,
    notify: Arc<Notify>,
    mutations: tokio::sync::Mutex<()>,
}
pub async fn call(dir: &Path, request: &Request) -> Result<Value> {
    let mut socket = UnixStream::connect(store::socket_path(dir))
        .await
        .context("RUNTIME_NOT_RUNNING: 请先运行 mox start")?;
    socket
        .write_all(format!("{}\n", serde_json::to_string(request)?).as_bytes())
        .await?;
    let mut response = String::new();
    let timeout = if matches!(request, Request::Command(Command::Status | Command::Stop)) {
        5
    } else {
        600
    };
    tokio::time::timeout(
        Duration::from_secs(timeout),
        BufReader::new(socket)
            .take(8 * 1024 * 1024)
            .read_line(&mut response),
    )
    .await
    .context("RUNTIME_TIMEOUT: 运行时响应超时")??;
    ensure!(response.len() < 8 * 1024 * 1024, "运行时响应过大");
    let v: Value = serde_json::from_str(&response).context("运行时连接已关闭")?;
    if let Some(error) = v.get("error") {
        bail!(
            "{}: {}",
            error["code"].as_str().unwrap_or("MOX_ERROR"),
            error["message"].as_str().unwrap_or("操作失败")
        )
    }
    Ok(v["data"].clone())
}
pub fn failure(e: &anyhow::Error) -> Value {
    let text = crate::api::diagnostic(e);
    let (candidate, message) = text.split_once(": ").unwrap_or(("MOX_ERROR", &text));
    let code = if candidate
        .bytes()
        .all(|b| b.is_ascii_uppercase() || b == b'_')
    {
        candidate
    } else {
        "MOX_ERROR"
    };
    json!({"error":{"code":code,"message":if code==candidate{message}else{&text},"retryable":matches!(code,"RUNTIME_TIMEOUT"|"CODEX_TIMEOUT"|"UPDATE_CHECK_FAILED")}})
}
pub async fn run(store: Store, no_ai: bool) -> Result<()> {
    let log_after: u64 = store
        .db
        .query_row("SELECT COALESCE(MAX(id),0) FROM events", [], |r| r.get(0))?;
    let dir = store.dir.clone();
    let api = Api::new(store.identity()?)?;
    let store = Arc::new(Mutex::new(store));
    let socket = store::socket_path(&dir);
    // The Store lock proves no other runtime owns this identity.
    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    let notify = Arc::new(Notify::new());
    store.lock().unwrap().delete("runtime", "ai_error")?;
    let client = if no_ai {
        None
    } else {
        match Codex::start(&dir).await {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("{}", Language::load(&dir).error(&e.to_string()));
                store
                    .lock()
                    .unwrap()
                    .put("runtime", "ai_error", &json!(e.to_string()))?;
                None
            }
        }
    };
    let runtime = Arc::new(Runtime {
        store: store.clone(),
        api,
        ai: Arc::new(Ai::new(client, notify.clone())),
        ai_enabled: !no_ai,
        stop: AtomicBool::new(false),
        notify,
        mutations: Default::default(),
    });
    store.lock().unwrap().event(
        "runtime.started",
        json!({"pid":std::process::id(),"ai":runtime.ai.client.is_some()}),
    )?;
    let lang = Language::load(&dir);
    eprintln!(
        "{}",
        lang.text(
            "Mox is running. Press Ctrl-C to stop; your identity will be kept.",
            "Mox 正在运行；Ctrl-C 停止，身份会保留。"
        )
    );
    eprintln!(
        "Codex: {}; {}",
        if no_ai {
            lang.text("disabled", "未启用")
        } else if runtime.ai.client.is_some() {
            lang.text(
                "connected, using local defaults",
                "已连接，使用本机默认配置",
            )
        } else {
            lang.text(
                "unavailable; manual chat is still available",
                "不可用，可继续手动聊天",
            )
        },
        lang.text(
            "connecting to the relay and publishing your profile…",
            "正在连接通信中继并发布资料…"
        )
    );
    let logger = tokio::spawn(log_runtime(runtime.clone(), log_after));
    runtime.ai.recover(&store, &runtime.api).await?;
    let worker = runtime.clone();
    let sync = tokio::spawn(async move {
        let mut published = false;
        let mut last_publication = tokio::time::Instant::now();
        while !worker.stop.load(Ordering::Acquire) {
            let result = async {
                if !published || last_publication.elapsed() > Duration::from_secs(86400) {
                    messaging::publish(&worker.store, &worker.api).await?;
                    let relays = messaging::bases(&worker.store)?;
                    worker
                        .store
                        .lock()
                        .unwrap()
                        .event("profile.published", json!({"relays":relays}))?;
                    published = true;
                    last_publication = tokio::time::Instant::now();
                }
                for base in messaging::bases(&worker.store)? {
                    messaging::sync(&worker.store, &worker.api, &base).await?;
                }
                worker
                    .store
                    .lock()
                    .unwrap()
                    .delete("runtime", "network_error")?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(e) = result {
                let _ = worker.store.lock().unwrap().put(
                    "runtime",
                    "network_error",
                    &json!({"error":crate::api::diagnostic(&e),"at":crypto::now()}),
                );
            }
            tokio::select! {_=tokio::time::sleep(Duration::from_millis(500))=>{},_=worker.notify.notified()=>{}}
        }
    });
    let mut workers = tokio::task::JoinSet::new();
    for mode in 0..3 {
        let rt = runtime.clone();
        workers.spawn(async move {
            loop {
                if rt.stop.load(Ordering::Acquire) { break; }
                let result = match mode {
                    0 => async { messaging::flush(&rt.store,&rt.api).await?; crate::group::flush(&rt.store,&rt.api).await }.await,
                    1 => crate::group::tick(&rt.store,&rt.api).await,
                    _ => rt.ai.tick(&rt.store,&rt.api).await,
                };
                if let Err(e) = result { let _ = rt.store.lock().unwrap().put("runtime", if mode==2 {"ai_error"} else {"delivery_error"}, &json!(e.to_string())); }
                tokio::select! { _=rt.notify.notified()=>{}, _=tokio::time::sleep(Duration::from_millis(if mode==1{1000}else{50}))=>{} }
            }
        });
    }
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _=tokio::signal::ctrl_c()=>{break},
            _=runtime.notify.notified()=>{if runtime.stop.load(Ordering::Acquire){break}},
            _=connections.join_next(), if !connections.is_empty()=>{},
            accepted=listener.accept()=>{
                let (socket,_)=accepted?;let rt=runtime.clone();
                connections.spawn(async move {
                    let (reader,mut writer)=socket.into_split();let mut reader=BufReader::new(reader);let mut bytes=Vec::new();
                    // Limit IPC reads before allocating an unbounded request.
                    use tokio::io::AsyncReadExt;
                    let read=tokio::time::timeout(Duration::from_secs(30),(&mut reader).take(2*1024*1024).read_until(b'\n',&mut bytes)).await;
                    let response=if matches!(read,Ok(Ok(n)) if n>0) && bytes.last()==Some(&b'\n') {
                        match serde_json::from_slice::<Request>(&bytes){Ok(request)=>match rt.execute(request).await{Ok(v)=>json!({"data":v}),Err(e)=>failure(&e)},Err(e)=>failure(&e.into())}
                    }else{failure(&anyhow::anyhow!("INVALID_REQUEST: IPC 请求过大或超时"))};
                    let _=writer.write_all(format!("{response}\n").as_bytes()).await;
                });
            }
        }
    }
    runtime.stop.store(true, Ordering::Release);
    runtime.notify.notify_waiters();
    let _ = logger.await;
    workers.abort_all();
    while workers.join_next().await.is_some() {}
    runtime.ai.finish().await;
    if let Some(c) = &runtime.ai.client {
        let _ = c.stop().await;
    }
    sync.abort();
    let _ = sync.await;
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    drop(listener);
    std::fs::remove_file(socket)?;
    store
        .lock()
        .unwrap()
        .db
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    Ok(())
}

fn log_text(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(400)
        .collect()
}

async fn log_runtime(runtime: Arc<Runtime>, mut after: u64) {
    let mut previous_errors: HashMap<&str, String> = HashMap::new();
    let dir = runtime.store.lock().unwrap().dir.clone();
    while !runtime.stop.load(Ordering::Acquire) {
        let lang = Language::load(&dir);
        // Read only committed events after the message transaction releases
        // the Store lock; logging must never slow encryption or relay requests.
        let snapshot = {
            let s = runtime.store.lock().unwrap();
            let events = s.events(after);
            let errors = ["network_error", "delivery_error", "ai_error"]
                .map(|key| (key, s.get("runtime", key).ok().flatten()));
            (events, errors)
        };
        if let Ok(events) = snapshot.0 {
            for e in events {
                after = e["id"].as_u64().unwrap_or(after);
                let d = &e["data"];
                let tag = d["chat"]
                    .as_str()
                    .map(|s| format!(" [{}]", &crypto::hash(s)[..10]))
                    .unwrap_or_default();
                let elapsed = d["elapsedMs"].as_u64().unwrap_or(0) as f64 / 1000.0;
                let message = match e["type"].as_str().unwrap_or("") {
                    "profile.published" => {
                        Some(lang.text(
                            "[Relay] Profile published; your identity link is searchable and can receive messages",
                            "[中继] 身份资料已同步，可通过身份链接搜索并接收消息"
                        ).into())
                    }
                    "friend.requested" if d["direction"] == "incoming" => Some(format!(
                        "[{}] {} {}; {}",
                        lang.text("Friend", "好友"),
                        lang.text("Friend request received:", "收到好友申请："),
                        log_text(d["id"].as_str().unwrap_or("")),
                        lang.text("run mox friend requests to view", "使用 mox friend requests 查看")
                    )),
                    "friend.updated" => Some(format!(
                        "[{}] {}: {}",
                        lang.text("Friend", "好友"),
                        lang.text("Request status", "申请状态"),
                        log_text(d["state"].as_str().unwrap_or(""))
                    )),
                    "message.created" if d["sender"] != runtime.api.identity.public => {
                        Some(format!(
                            "[{}]{tag} {} ({} {})",
                            lang.text("Message", "消息"),
                            lang.text("Message received", "收到消息"),
                            d["text"].as_str().map(|s| s.chars().count()).unwrap_or(0),
                            lang.text("characters", "字符")
                        ))
                    }
                    "ai.started" => Some(format!("[AI]{tag} {}", lang.text("Processing message", "开始处理消息"))),
                    "ai.generating" => Some(format!("[AI]{tag} {}", lang.text("Waiting for Codex reply text", "正在等待 Codex 输出正文"))),
                    "ai.action" => Some(format!(
                        "[AI]{tag} {}: {}{}",
                        log_text(d["tool"].as_str().unwrap_or("")),
                        if d["success"] == true {
                            lang.text("queued", "已排队")
                        } else {
                            lang.text("failed; reported to Codex", "失败，已反馈给 Codex")
                        },
                        d["error"].as_str().map(|e|format!("; {}",log_text(e))).unwrap_or_default()
                    )),
                    "ai.first_output" => Some(format!(
                        "[AI]{tag} {} ({elapsed:.1} {}); {}",
                        lang.text("First reply text received", "收到首批正文"),
                        lang.text("s elapsed", "秒"),
                        lang.text("streaming", "开始流式发送")
                    )),
                    "ai.completed" => Some(format!(
                        "[AI]{tag} {} ({elapsed:.1} {}); {} {}",
                        lang.text("Generation complete", "生成完成"),
                        lang.text("s elapsed", "秒"),
                        d["frames"],
                        lang.text("encrypted frames queued", "个加密帧已排队")
                    )),
                    "ai.interrupted" => {
                        Some(format!("[AI]{tag} {}", lang.text("Reply interrupted; retry with mox ai resume", "回复已中断；可用 mox ai resume 重试")))
                    }
                    "ai.error" => Some(format!(
                        "[AI]{tag} {}: {}",
                        lang.text("Reply failed", "回复失败"),
                        log_text(&lang.error(d["error"].as_str().unwrap_or(lang.text("Unknown error", "未知错误"))))
                    )),
                    "delivery.accepted" => Some(format!(
                        "[{}] {} {}",
                        lang.text("Send", "发送"),
                        lang.text("Relay accepted", "中继已接收"),
                        log_text(d["messageId"].as_str().unwrap_or(lang.text("message", "消息")))
                    )),
                    "file.stage" => Some(format!("[{}] {}: {}",lang.text("File","文件"),log_text(d["taskId"].as_str().unwrap_or("")),log_text(d["stage"].as_str().unwrap_or("")))),
                    "file.progress" => Some(format!("[{}] {} / {} {}",lang.text("File","文件"),d["uploaded"],d["total"],lang.text("encrypted bytes uploaded","密文字节已上传"))),
                    "file.error" => Some(format!("[{}] {}: {}{}",lang.text("File","文件"),log_text(d["stage"].as_str().unwrap_or("")),log_text(d["error"].as_str().unwrap_or("")),if d["retrying"]==true {lang.text("; retrying from confirmed offset","；按已确认偏移重试")}else{""})),
                    _ => None,
                };
                if let Some(message) = message {
                    eprintln!("{message}");
                }
            }
        }
        for (key, error) in snapshot.1 {
            let message = error
                .as_ref()
                .map(|v| {
                    v.as_str()
                        .or_else(|| v["error"].as_str())
                        .unwrap_or(lang.text("Unknown runtime error", "未知运行错误"))
                })
                .unwrap_or("");
            if message.is_empty() {
                if previous_errors.remove(key).is_some() {
                    eprintln!(
                        "[{}] {key}: {}",
                        lang.text("Recovered", "恢复"),
                        lang.text("cleared", "已清除")
                    );
                }
            } else if previous_errors
                .get(key)
                .is_none_or(|previous| previous != message)
            {
                eprintln!(
                    "[{}] {key}: {}",
                    lang.text("Error", "错误"),
                    log_text(&lang.error(message))
                );
                previous_errors.insert(key, message.into());
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
impl Runtime {
    pub async fn execute(&self, request: Request) -> Result<Value> {
        match request {
            Request::Frame { id, frame } => {
                messaging::send(&self.store, &self.api, &id, &frame.encode()?).await
            }
            Request::Command(command) => self.command(command).await,
        }
    }
    async fn command(&self, command: Command) -> Result<Value> {
        match command {
            Command::Status => {
                let s = self.store.lock().unwrap();
                Ok(
                    json!({"running":true,"pid":std::process::id(),"version":env!("CARGO_PKG_VERSION"),"identity":s.identity()?.view(),"aiEnabled":self.ai_enabled,"ai":self.ai.client.as_ref().is_some_and(|c|c.alive()),"networkError":s.get("runtime","network_error")?,"deliveryError":s.get("runtime","delivery_error")?,"groupError":s.get("runtime","group_error")?,"aiError":s.get("runtime","ai_error")?}),
                )
            }
            Command::Stop => {
                self.stop.store(true, Ordering::Release);
                let notify = self.notify.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    notify.notify_waiters();
                });
                Ok(json!({"stopping":true}))
            }
            Command::Whoami | Command::Login { .. } => {
                Ok(self.store.lock().unwrap().identity()?.view())
            }
            Command::Qr | Command::Moxpub => {
                // Do not expose a relay selection that is still being validated
                // and may be rolled back by an in-flight relay switch.
                let _guard = self.mutations.lock().await;
                crate::share::identity(&self.store.lock().unwrap())
            }
            Command::Chat {
                command: ChatCommand::List,
            } => Ok(json!(self.store.lock().unwrap().list("chat")?)),
            Command::Events { after } => Ok(json!(self.store.lock().unwrap().events(after)?)),
            Command::Message {
                command: MessageCommand::List(args),
            } => self.store.lock().unwrap().history(&args),
            Command::Message {
                command: MessageCommand::Send(args),
            } => {
                let _guard = self.mutations.lock().await;
                crate::outgoing::send(&self.store, &self.api, args).await
            }
            Command::Lang { language } => {
                let dir = self.store.lock().unwrap().dir.clone();
                if let Some(value) = language {
                    value.save(&dir)?;
                }
                Ok(json!({"language": crate::language::Language::load(&dir)}))
            }
            Command::Message {
                command:
                    MessageCommand::React {
                        id,
                        message_id,
                        emoji,
                        idempotency_key,
                    },
            } => {
                let _guard = self.mutations.lock().await;
                crate::outgoing::react(
                    &self.store,
                    &self.api,
                    &id,
                    &message_id,
                    &emoji,
                    idempotency_key,
                )
                .await
            }
            Command::Friend {
                command:
                    FriendCommand::Add {
                        public_key,
                        message,
                    },
            } => messaging::add_friend(&self.store, &self.api, &public_key, &message).await,
            Command::Friend { command } => {
                let s = self.store.lock().unwrap();
                friend_local(&s, command)
            }
            Command::Relay { command } => {
                let _guard = self.mutations.lock().await;
                let switched =
                    matches!(command, RelayCommand::Use { .. } | RelayCommand::Set { .. });
                let previous = crate::relays::selected(&self.store.lock().unwrap(), false)?;
                let result = relay_local(&self.store.lock().unwrap(), command)?;
                if switched {
                    if let Err(e) = messaging::publish(&self.store, &self.api).await {
                        self.store.lock().unwrap().put(
                            "settings",
                            "selected_relay",
                            &json!(previous),
                        )?;
                        let _ = messaging::publish(&self.store, &self.api).await;
                        return Err(e);
                    }
                }
                Ok(result)
            }
            Command::FileRelay { command } => {
                crate::relays::command(Some(&self.store.lock().unwrap()), true, command)
            }
            Command::Profile {
                command: ProfileCommand::Set { name },
            } => {
                {
                    let s = self.store.lock().unwrap();
                    let mut id = s.identity()?;
                    ensure!(
                        !name.trim().is_empty() && name.chars().count() <= 256,
                        "昵称不能为空或超过 256 字符"
                    );
                    id.name = name.trim().into();
                    s.put("identity", "current", &serde_json::to_value(&id)?)?;
                }
                messaging::publish(&self.store, &self.api).await?;
                Ok(self.store.lock().unwrap().identity()?.view())
            }
            Command::Ai { command } => {
                let s = self.store.lock().unwrap();
                match command {
                    AiCommand::Workspace { id } => {
                        ensure!(
                            s.get("chat", &id)?.is_some(),
                            "CHAT_NOT_FOUND: Conversation not found"
                        );
                        Ok(json!({"chat":id,"path":crate::codex::workspace(&s.dir, &id)?}))
                    }
                    AiCommand::Status => Ok(
                        json!({"available":self.ai.client.as_ref().is_some_and(|c|c.alive()),"paused":s.list("ai_paused")?,"error":s.get("runtime","ai_error")?,
                        "queued":s.list("ai_queue")?.len(),"failed":s.list("ai_failed")?.len(),"actionErrors":s.list("ai_action_error")?,
                        "active":s.list("ai_active")?.into_iter().map(|v| json!({"chat":v["chat"],"messageId":v["id"],"stage":v["stage"],"elapsedMs":crypto::now().saturating_sub(v["startedAt"].as_u64().unwrap_or_else(crypto::now)),"firstOutputMs":v["firstOutputMs"]})).collect::<Vec<_>>()}),
                    ),
                    AiCommand::Pause { id } => {
                        let key = id.as_deref().unwrap_or("all");
                        s.put("ai_paused", key, &json!({"id":key}))?;
                        Ok(json!({"paused":key}))
                    }
                    AiCommand::Resume { id } => {
                        let key = id.as_deref().unwrap_or("all");
                        s.delete("ai_paused", key)?;
                        for mut msg in s.list("ai_failed")? {
                            if key == "all" || msg["chat"] == key {
                                let failed_key = msg["queueKey"].as_str().map(str::to_owned);
                                msg.as_object_mut().map(|v| v.remove("streamId"));
                                if let Some(failed_key) = failed_key {
                                    s.put("ai_queue", &failed_key, &msg)?;
                                    s.delete("ai_failed", &failed_key)?;
                                }
                            }
                        }
                        Ok(json!({"resumed":key}))
                    }
                }
            }
            Command::Group { command } => {
                crate::group::command(&self.store, &self.api, command).await
            }
            _ => bail!("该命令必须从 mox 主进程执行"),
        }
    }
}
pub fn friend_local(s: &Store, command: FriendCommand) -> Result<Value> {
    match command {
        FriendCommand::List => Ok(json!(s.list("friend")?)),
        FriendCommand::Requests => Ok(json!(s.list("request")?)),
        FriendCommand::Accept { request_id } => {
            messaging::friend_decision(s, &s.identity()?, &request_id, "friend_receipt")
        }
        FriendCommand::Reject { request_id } => {
            messaging::friend_decision(s, &s.identity()?, &request_id, "friend_reject")
        }
        FriendCommand::Cancel { request_id } => {
            messaging::friend_decision(s, &s.identity()?, &request_id, "friend_cancel")
        }
        FriendCommand::Remove { public_key } => {
            s.delete("friend", &public_key)?;
            Ok(json!({"removed":public_key}))
        }
        FriendCommand::Block { public_key } => {
            crypto::parse_public(&public_key)?;
            s.put("blocked", &public_key, &json!({"pub":public_key}))?;
            Ok(json!({"blocked":public_key}))
        }
        FriendCommand::Unblock { public_key } => {
            s.delete("blocked", &public_key)?;
            Ok(json!({"unblocked":public_key}))
        }
        FriendCommand::Add { .. } => bail!("RUNTIME_NOT_RUNNING: 请先运行 mox start"),
    }
}
pub fn relay_local(s: &Store, command: RelayCommand) -> Result<Value> {
    crate::relays::command(Some(s), false, command)
}
