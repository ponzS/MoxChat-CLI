use crate::{
    args::HistoryArgs,
    crypto::{self, Identity},
};
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

pub struct Store {
    pub db: Connection,
    pub dir: PathBuf,
    _lock: File,
}
pub fn data_dir(given: Option<PathBuf>) -> Result<PathBuf> {
    let path = given
        .or_else(|| std::env::var_os("MOX_DATA_DIR").map(PathBuf::from))
        .unwrap_or_else(|| {
            directories::ProjectDirs::from("chat", "MoxChat", "moxchat-cli")
                .expect("Home directory")
                .data_local_dir()
                .to_path_buf()
        });
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let existed = absolute.exists();
    fs::create_dir_all(&absolute)?;
    let dir = absolute.canonicalize()?;
    ensure!(
        dir.parent().is_some()
            && Some(dir.as_path()) != directories::BaseDirs::new().as_ref().map(|v| v.home_dir()),
        "不能使用根目录或用户主目录作为身份目录"
    );
    if !existed || dir.join(".mox-data").is_file() {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}
pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join("runtime.sock")
}
fn key_entry(dir: &Path) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(
        "chat.mox.cli",
        &crypto::hash(dir.to_string_lossy().as_bytes()),
    )?)
}
fn master_key(dir: &Path, create: bool) -> Result<Zeroizing<String>> {
    if let Ok(value) = std::env::var("MOX_MASTER_KEY") {
        ensure!(
            hex::decode(&value).is_ok_and(|b| b.len() == 32),
            "MOX_MASTER_KEY 必须是 32 字节十六进制密钥"
        );
        return Ok(Zeroizing::new(value));
    }
    let entry = key_entry(dir)?;
    match entry.get_password() {
        Ok(value) => Ok(Zeroizing::new(value)),
        Err(keyring::Error::NoEntry) if create => {
            let value = Zeroizing::new(hex::encode(crypto::random::<32>()));
            entry.set_password(&value)?;
            Ok(value)
        }
        Err(e) => Err(e).context("无法读取系统密钥环；无桌面环境可使用 MOX_MASTER_KEY 注入密钥"),
    }
}
impl Store {
    pub fn open(dir: &Path, create: bool) -> Result<Self> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join("identity.lock"))?;
        lock.try_lock_exclusive()
            .context("RUNTIME_RUNNING: 此身份正在使用，请通过正在运行的 mox 服务操作")?;
        let path = dir.join("identity.db");
        let marker = dir.join(".mox-data");
        if create && !marker.exists() {
            ensure!(
                std::fs::read_dir(dir)?.all(|e| e.is_ok_and(|e| e.file_name() == "identity.lock"
                    || (e.file_name() == "ui-language"
                        && e.file_type().is_ok_and(|t| t.is_file())))),
                "DATA_DIR_NOT_EMPTY: 请使用专用的空数据目录"
            );
            use std::io::Write;
            let mut owner = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&marker)?;
            owner.write_all(b"moxchat-cli-data-v1\n")?;
            owner.sync_all()?;
        }
        ensure!(
            std::fs::read_to_string(&marker).ok().as_deref() == Some("moxchat-cli-data-v1\n"),
            "DATA_DIR_NOT_OWNED: 数据目录缺少 Mox 所有权标记"
        );
        if dir.join(".destroying").exists() {
            cleanup_identity(dir)?;
        }
        ensure!(create || path.exists(), "NOT_LOGGED_IN: 请运行 mox login");
        for name in ["identity.db", "identity.db-wal", "identity.db-shm"] {
            ensure!(
                !dir.join(name)
                    .symlink_metadata()
                    .is_ok_and(|m| m.file_type().is_symlink()),
                "身份数据不能是符号链接"
            );
        }
        let key = master_key(dir, create && !path.exists())?;
        let db = Connection::open(&path)?;
        db.pragma_update(None, "key", format!("x'{}'", key.as_str()))?;
        db.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i64>(0)
        })
        .context("无法解密身份数据库")?;
        let cipher: String = db
            .query_row("PRAGMA cipher_version", [], |r| r.get(0))
            .context("此构建缺少 SQLCipher")?;
        ensure!(!cipher.is_empty(), "此构建缺少 SQLCipher");
        db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA secure_delete=ON;
          CREATE TABLE IF NOT EXISTS kv (kind TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(kind,key));
          CREATE TABLE IF NOT EXISTS messages (ordinal INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, chat TEXT NOT NULL, sender TEXT NOT NULL, text TEXT NOT NULL, created INTEGER NOT NULL, status TEXT NOT NULL, stream TEXT, seq INTEGER NOT NULL DEFAULT 0, terminal INTEGER NOT NULL DEFAULT 1);
          CREATE INDEX IF NOT EXISTS messages_chat ON messages(chat,ordinal);
          CREATE UNIQUE INDEX IF NOT EXISTS messages_stream ON messages(chat,sender,stream) WHERE stream IS NOT NULL;
          CREATE TABLE IF NOT EXISTS events (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS outbox (id TEXT PRIMARY KEY, value TEXT NOT NULL, created INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS received (id TEXT PRIMARY KEY, created INTEGER NOT NULL);
          PRAGMA user_version=1;")?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            db,
            dir: dir.to_owned(),
            _lock: lock,
        })
    }
    pub fn get(&self, kind: &str, key: &str) -> Result<Option<Value>> {
        let v: Option<String> = self
            .db
            .query_row(
                "SELECT value FROM kv WHERE kind=? AND key=?",
                params![kind, key],
                |r| r.get(0),
            )
            .optional()?;
        v.map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    pub fn put(&self, kind: &str, key: &str, v: &Value) -> Result<()> {
        self.db.execute(
            "INSERT INTO kv VALUES(?,?,?) ON CONFLICT(kind,key) DO UPDATE SET value=excluded.value",
            params![kind, key, v.to_string()],
        )?;
        Ok(())
    }
    pub fn delete(&self, kind: &str, key: &str) -> Result<()> {
        self.db
            .execute("DELETE FROM kv WHERE kind=? AND key=?", params![kind, key])?;
        Ok(())
    }
    pub fn list(&self, kind: &str) -> Result<Vec<Value>> {
        let mut st = self
            .db
            .prepare("SELECT value FROM kv WHERE kind=? ORDER BY key")?;
        let rows = st
            .query_map([kind], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|s| Ok(serde_json::from_str(&s)?))
            .collect()
    }
    pub fn identity(&self) -> Result<Identity> {
        Ok(serde_json::from_value(
            self.get("identity", "current")?
                .context("NOT_LOGGED_IN: 请运行 mox login")?,
        )?)
    }
    pub fn login(&self, name: &str) -> Result<Value> {
        if let Some(value) = self.get("identity", "current")? {
            return Ok(serde_json::from_value::<Identity>(value)?.view());
        }
        let id = Identity::create(name)?;
        self.put("identity", "current", &serde_json::to_value(&id)?)?;
        self.put(
            "relay",
            "https://mox.ponzs.com",
            &json!({"url":"https://mox.ponzs.com"}),
        )?;
        Ok(id.view())
    }
    pub fn event(&self, kind: &str, value: Value) -> Result<u64> {
        self.db.execute(
            "INSERT INTO events(value) VALUES(?)",
            [json!({"type":kind,"at":crypto::now(),"data":value}).to_string()],
        )?;
        Ok(self.db.last_insert_rowid() as u64)
    }
    pub fn events(&self, after: u64) -> Result<Vec<Value>> {
        let mut st = self
            .db
            .prepare("SELECT id,value FROM events WHERE id>? ORDER BY id LIMIT 200")?;
        let rows = st
            .query_map([after], |r| {
                Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(id, s)| {
                let mut v: Value = serde_json::from_str(&s)?;
                v["id"] = json!(id);
                Ok(v)
            })
            .collect()
    }
    pub fn history(&self, args: &HistoryArgs) -> Result<Value> {
        ensure!(
            self.get("chat", &args.id)?.is_some(),
            "CHAT_NOT_FOUND: 会话不存在"
        );
        let (snapshot, offset, page_size) = if let Some(cursor) = &args.cursor {
            let v: Value = serde_json::from_slice(&crypto::unb64(cursor)?)?;
            let size = v["pageSize"]
                .as_u64()
                .filter(|s| (1..=200).contains(s))
                .context("INVALID_CURSOR")? as u32;
            ensure!(
                v["identity"] == self.identity()?.id
                    && v["chat"] == args.id
                    && args.page_size.is_none_or(|s| s == size),
                "INVALID_CURSOR: 游标与身份、会话或每页数量不匹配"
            );
            (
                v["snapshot"].as_u64().context("INVALID_CURSOR")?,
                v["offset"].as_u64().context("INVALID_CURSOR")?,
                size,
            )
        } else {
            let max: u64 =
                self.db
                    .query_row("SELECT coalesce(max(ordinal),0) FROM messages", [], |r| {
                        r.get(0)
                    })?;
            let size = args.page_size.unwrap_or(50);
            (
                max,
                u64::from(args.page.unwrap_or(1) - 1) * u64::from(size),
                size,
            )
        };
        let limit = args.pages * page_size;
        let mut st=self.db.prepare("SELECT id,sender,text,created,status,stream,seq,terminal FROM messages WHERE chat=? AND ordinal<=? ORDER BY ordinal DESC LIMIT ? OFFSET ?")?;
        let mut items=st.query_map(params![args.id,snapshot,limit+1,offset],|r|Ok(json!({"id":r.get::<_,String>(0)?,"sender":r.get::<_,String>(1)?,"text":r.get::<_,String>(2)?,"createdAt":r.get::<_,u64>(3)?,"status":r.get::<_,String>(4)?,"streamId":r.get::<_,Option<String>>(5)?,"seq":r.get::<_,u64>(6)?,"terminal":r.get::<_,bool>(7)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let more = items.len() > limit as usize;
        if more {
            items.pop();
        }
        for item in &mut items {
            item["type"] = json!("text");
            let meta = self.get("stream_state", item["id"].as_str().unwrap_or(""))?;
            item["format"] = meta
                .as_ref()
                .map(|v| v["format"].clone())
                .unwrap_or(json!("plain"));
            item["stream_id"] = item["streamId"].take();
            item["last_seq"] = item["seq"].take();
            item["stream_state"] = meta
                .as_ref()
                .map(|v| v["kind"].clone())
                .unwrap_or(Value::Null);
            item["reply_to"] = meta
                .as_ref()
                .map(|v| v["replyTo"].clone())
                .unwrap_or(Value::Null);
            if let Some(reaction) = item["text"].as_str().and_then(crate::outgoing::reaction) {
                item["type"] = json!("reaction");
                item["reaction"] = reaction;
                item["text"] = Value::Null;
            }
            if let Some(text) = item["text"].as_str() {
                for (prefix, kind) in [
                    ("__file_image__:", "image"),
                    ("__file_video__:", "video"),
                    ("__file_blob__:", "file"),
                ] {
                    if let Some(raw) = text.strip_prefix(prefix) {
                        if let Ok(v) = serde_json::from_str::<Value>(raw) {
                            item["attachment"] = v;
                            item["type"] = json!(kind);
                            item["text"] = Value::Null;
                        }
                        break;
                    }
                }
            }
        }
        let cursor=more.then(||crypto::b64(json!({"identity":self.identity().map(|i|i.id.clone()).unwrap_or_default(),"chat":args.id,"pageSize":page_size,"snapshot":snapshot,"offset":offset+items.len() as u64}).to_string()));
        Ok(
            json!({"messages":items,"pagination":{"start_page":offset/u64::from(page_size)+1,"pages_requested":args.pages,"pages_returned":(items.len() as u32).div_ceil(page_size),"page_size":page_size,"items_returned":items.len(),"has_more":more,"next_cursor":cursor},"order":"newest_first"}),
        )
    }
    pub fn destroy(self) -> Result<()> {
        // This handle holds the exclusive identity lock. Only fixed, owned files are removed.
        let dir = self.dir.clone();
        self.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        let marker = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(dir.join(".destroying"))?;
        marker.sync_all()?;
        drop(self.db);
        cleanup_identity(&dir)
    }
}
fn cleanup_identity(dir: &Path) -> Result<()> {
    for name in [
        "identity.db",
        "identity.db-wal",
        "identity.db-shm",
        "ui-language",
    ] {
        match fs::remove_file(dir.join(name)) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    for name in ["codex-work", "mls", "attachments"] {
        let work = dir.join(name);
        if work.exists() {
            ensure!(
                work.is_dir() && !work.symlink_metadata()?.file_type().is_symlink(),
                "无效的身份关联目录：{name}"
            );
            fs::remove_dir_all(work)?;
        }
    }
    if std::env::var_os("MOX_MASTER_KEY").is_none() {
        match key_entry(dir)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => (),
            Err(e) => bail!("身份文件已删除，密钥环条目清理失败：{e}"),
        }
    }
    fs::remove_file(dir.join(".destroying"))?;
    Ok(())
}
