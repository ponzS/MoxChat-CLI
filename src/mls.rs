//! Typed client for the bundled native MLS runtime. No MLS implementation is linked here.
use crate::crypto;
use anyhow::{ensure, Context, Result};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};
use zeroize::Zeroizing;

const BINARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/mox-mls-runtime"));
const HASH: &str = env!("MOX_MLS_SHA256");
const LIMIT: u64 = 8 * 1024 * 1024;

fn executable(db: &Path) -> Result<PathBuf> {
    let parent = db.parent().context("MLS_DATABASE_PATH")?.join("runtime");
    std::fs::create_dir_all(&parent)?;
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700))?;
    let path = parent.join(format!("mox-mls-{HASH}"));
    if !path.try_exists()? {
        let staged = parent.join(format!(".{}", crypto::id()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o700)
                .open(&staged)?;
            file.write_all(BINARY)?;
            file.sync_all()?;
            std::fs::rename(&staged, &path)?;
            File::open(&parent)?.sync_all()?;
            Ok::<_, anyhow::Error>(())
        })();
        let _ = std::fs::remove_file(staged);
        result?;
    }
    let metadata = path.symlink_metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.permissions().mode() & 0o077 == 0
            && metadata.len() == BINARY.len() as u64,
        "MLS_RUNTIME_INVALID: Invalid bundled runtime file"
    );
    ensure!(
        crypto::hash(std::fs::read(&path)?) == HASH,
        "MLS_RUNTIME_INVALID: Bundled runtime checksum mismatch"
    );
    Ok(path)
}

pub struct MlsRuntime {
    binary: PathBuf,
    db: PathBuf,
    device: String,
    key: Zeroizing<Vec<u8>>,
}

impl MlsRuntime {
    pub fn open(db: PathBuf, device: &str, key: &[u8]) -> Result<Self> {
        ensure!(
            db.is_absolute() && key.len() == 32,
            "MLS_INVALID_CONFIGURATION"
        );
        let runtime = Self {
            binary: executable(&db)?,
            db,
            device: device.into(),
            key: Zeroizing::new(key.to_vec()),
        };
        let _: Value = runtime.call(json!({"method":"open"}))?;
        Ok(runtime)
    }

    fn call<T: DeserializeOwned>(&self, operation: Value) -> Result<T> {
        let request = Zeroizing::new(serde_json::to_vec(
            &json!({"v":1,"dbPath":self.db,"deviceId":self.device,"storageKey":*self.key,"operation":operation}),
        )?);
        ensure!(request.len() as u64 <= LIMIT, "MLS_REQUEST_TOO_LARGE");
        let mut child = Command::new(&self.binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("MLS_RUNTIME_START_FAILED")?;
        let mut input = child.stdin.take().context("MLS_STDIN_UNAVAILABLE")?;
        let output = child.stdout.take().context("MLS_STDOUT_UNAVAILABLE")?;
        let (send, receive) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let result = (|| {
                input.write_all(&request)?;
                drop(input);
                let mut bytes = Zeroizing::new(Vec::new());
                output.take(LIMIT + 1).read_to_end(&mut bytes)?;
                ensure!(bytes.len() as u64 <= LIMIT, "MLS_RESPONSE_TOO_LARGE");
                Ok::<_, anyhow::Error>(bytes)
            })();
            let _ = send.send(result);
        });
        let response = receive.recv_timeout(Duration::from_secs(30));
        // stdout EOF implies the one-shot child has completed. Kill and reap
        // before joining on timeout, so failed calls cannot leak workers.
        if response.is_err() {
            let _ = child.kill();
        }
        let status = child.wait();
        let _ = worker.join();
        let bytes =
            response.context("MLS_RUNTIME_TIMEOUT: Native MLS operation did not finish")??;
        ensure!(status?.success(), "MLS_RUNTIME_EXIT_FAILED");
        let result: Value = serde_json::from_slice(&bytes).context("MLS_INVALID_RESPONSE")?;
        ensure!(result["v"] == 1, "MLS_PROTOCOL_VERSION");
        ensure!(
            result["ok"] == true,
            "MLS_ERROR: {}",
            result["error"]
                .as_str()
                .unwrap_or("Native MLS operation failed")
        );
        Ok(serde_json::from_value(result["result"].clone())?)
    }

    pub fn create_group(&mut self) -> Result<CreatedGroup> {
        self.call(json!({"method":"create_group"}))
    }
    pub fn generate_key_package(&mut self) -> Result<KeyPackage> {
        self.call(json!({"method":"generate_key_package"}))
    }
    pub fn join_group(&mut self, welcome: &[u8]) -> Result<JoinedGroup> {
        self.call(json!({"method":"join_group","welcome":welcome}))
    }
    pub fn group_diagnostics(&self, group: &[u8]) -> Result<GroupDiagnostics> {
        self.call(json!({"method":"group_diagnostics","group":group}))
    }
    pub fn prepare_add_member(
        &mut self,
        group: &[u8],
        task: &str,
        key_package: &[u8],
    ) -> Result<PreparedAdd> {
        self.call(json!({"method":"prepare_add_member","group":group,"task":task,"key_package":key_package}))
    }
    pub fn commit_prepared_add_member(
        &mut self,
        group: &[u8],
        task: &str,
        hash: &str,
    ) -> Result<u64> {
        self.call(
            json!({"method":"commit_prepared_add_member","group":group,"task":task,"hash":hash}),
        )
    }
    pub fn prepare_remove_members(
        &mut self,
        group: &[u8],
        task: &str,
        devices: &[String],
    ) -> Result<PreparedRemove> {
        self.call(
            json!({"method":"prepare_remove_members","group":group,"task":task,"devices":devices}),
        )
    }
    pub fn commit_prepared_remove_members(
        &mut self,
        group: &[u8],
        task: &str,
        hash: &str,
    ) -> Result<u64> {
        self.call(json!({"method":"commit_prepared_remove_members","group":group,"task":task,"hash":hash}))
    }
    pub fn prepare_application_message(
        &mut self,
        group: &[u8],
        task: &str,
        message: &str,
        plaintext: &[u8],
    ) -> Result<PreparedApplication> {
        self.call(json!({"method":"prepare_application_message","group":group,"task":task,"message":message,"plaintext":plaintext}))
    }
    pub fn process_message(&mut self, group: &[u8], bytes: &[u8]) -> Result<ProcessedMessage> {
        self.call(json!({"method":"process_message","group":group,"bytes":bytes}))
    }
    pub fn process_application(&mut self, group: &[u8], bytes: &[u8]) -> Result<ProcessedMessage> {
        self.call(json!({"method":"process_application","group":group,"bytes":bytes}))
    }
    pub fn process_remove_commit(
        &mut self,
        group: &[u8],
        bytes: &[u8],
        devices: &[String],
    ) -> Result<ProcessedMessage> {
        self.call(
            json!({"method":"process_remove_commit","group":group,"bytes":bytes,"devices":devices}),
        )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedGroup {
    pub group_id: Vec<u8>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyPackage {
    pub tls_bytes: Vec<u8>,
    pub key_package_ref: Vec<u8>,
    pub credential_hash: Vec<u8>,
}
#[derive(Deserialize)]
pub struct GroupDiagnostics {
    pub epoch: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinedGroup {
    pub group_id: Vec<u8>,
    pub epoch: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedAdd {
    pub commit_tls_bytes: Vec<u8>,
    pub commit_hash: String,
    pub welcome_tls_bytes: Vec<Vec<u8>>,
    pub epoch: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedRemove {
    pub commit_tls_bytes: Vec<u8>,
    pub commit_hash: String,
    pub epoch: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedApplication {
    pub ciphertext_tls_bytes: Vec<u8>,
    pub ciphertext_hash: String,
    pub epoch: u64,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessedMessage {
    Application { plaintext: Vec<u8>, epoch: u64 },
    Commit { epoch: u64 },
}
