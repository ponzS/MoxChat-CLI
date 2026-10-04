use crate::mls::{MlsRuntime, ProcessedMessage};
use crate::{
    api::Api,
    args::GroupCommand,
    crypto::{self, string, Identity},
    messaging::{self, SharedStore},
    store::Store,
};
use anyhow::{ensure, Context, Result};
use p256::SecretKey;
use reqwest::Method;
use serde_json::{json, Value};
use std::sync::OnceLock;
use tokio::sync::Mutex;

const DEVICE: &str = "mox:group:device-credential:v1";
const CONTROL: &str = "mox:group:control-record:v1";
const DESCRIPTOR: &str = "mox:group:descriptor:v2";
const RELAY: &str = "mox:group:relay-set:v1";
const KEY_PACKAGE: &str = "mox:group:key-package-record:v1";
fn serial() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}
fn unsigned(value: &Value) -> Value {
    let mut v = value.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("signature");
        o.remove("signatures");
    }
    v
}
fn record_hash(domain: &str, v: &Value) -> Result<String> {
    Ok(hex::encode(crypto::digest(domain, &unsigned(v))?))
}
fn multi_sign(domain: &str, mut value: Value, i: &Identity) -> Result<Value> {
    value["signatures"] = json!([{"signerPub":i.public,"signature":crypto::sign_digest(&i.private,&crypto::digest(domain,&value)?)?}]);
    Ok(value)
}
fn verify_multi(domain: &str, v: &Value, owner: &str) -> Result<()> {
    let entries = v["signatures"].as_array().context("缺少群签名")?;
    let signature = entries
        .iter()
        .find(|s| s["signerPub"] == owner)
        .context("缺少群主签名")?;
    crypto::verify_digest(
        owner,
        string(signature, "signature")?,
        &crypto::digest(domain, &unsigned(v))?,
        true,
    )
}
fn group_bytes(id: &str) -> Result<Vec<u8>> {
    let bytes = if id.len() == 64 {
        hex::decode(id)?
    } else {
        crypto::unb64(id)?
    };
    ensure!(bytes.len() == 32, "INVALID_GROUP_ID: 无效群 ID");
    Ok(bytes)
}
fn open(s: &Store) -> Result<(MlsRuntime, Value)> {
    let mut device = if let Some(v) = s.get("group_device", "current")? {
        v
    } else {
        let key = SecretKey::random(&mut rand::rngs::OsRng);
        let v = json!({"id":crypto::id(),"private":crypto::b64(key.to_bytes()),"public":crypto::public(&key),"storageKey":crypto::b64(crypto::random::<32>())});
        s.put("group_device", "current", &v)?;
        v
    };
    let mls_dir = s.dir.join("mls");
    std::fs::create_dir_all(&mls_dir)?;
    let mut mls = MlsRuntime::open(
        mls_dir.join("groups.db"),
        string(&device, "id")?,
        &crypto::unb64(string(&device, "storageKey")?)?,
    )?;
    if device["credential"].is_null()
        || device["credential"]["expiresAt"].as_u64().unwrap_or(0) < crypto::now() + 7 * 86400000
    {
        let seed = mls.generate_key_package()?;
        let i = s.identity()?;
        let now = crypto::now();
        let credential = crypto::sign_document(
            DEVICE,
            json!({"version":1,"ownerPub":i.public,"deviceId":device["id"],"credentialEpoch":device["credential"]["credentialEpoch"].as_u64().unwrap_or(0)+1,"deviceSigningPub":device["public"],"mlsCredentialHash":hex::encode(seed.credential_hash),"issuedAt":now,"expiresAt":now+30*86400000}),
            &i.private,
        )?;
        device["hash"] = json!(record_hash(DEVICE, &credential)?);
        device["credential"] = credential;
        s.put("group_device", "current", &device)?;
    }
    Ok((mls, device))
}
async fn operation(
    store: &SharedStore,
    api: &Api,
    target: &Value,
    method: &str,
    path: &str,
    body: Option<&Value>,
    device: Option<&Value>,
) -> Result<Value> {
    let base = messaging::bases(store)?
        .into_iter()
        .next()
        .context("NO_RELAY: 尚未设置中继")?;
    let pin = store
        .lock()
        .unwrap()
        .get("pin", &base)?
        .context("请等待中继身份发布完成")?;
    let now = crypto::now();
    let payload = json!({"version":1,"ownerPub":api.identity.public,"ingressServerPub":pin["serverPub"],"target":target,"requestId":crypto::id(),"method":method,"path":path,"body":body.map(Value::to_string).unwrap_or_default(),"deviceId":device.map(|v|v["id"].clone()).unwrap_or(json!("")),"credentialHash":device.map(|v|v["hash"].clone()).unwrap_or(json!("")),"issuedAt":now,"expiresAt":now+120000});
    let digest = crypto::digest("mox:group:client-operation:v1", &payload)?;
    let hash = hex::encode(digest);
    let proof = if let Some(device) = device {
        crypto::sign_digest(
            string(device, "private")?,
            &crypto::digest(
                "mox:group:device-session-proof:v1",
                &json!({"version":1,"ownerPub":api.identity.public,"deviceId":device["id"],"credentialHash":device["hash"],"challengeId":hash,"challengeCodeHash":crypto::hash(&hash)}),
            )?,
        )?
    } else {
        String::new()
    };
    api.ensure_session(&base).await?;
    let response=api.request(&base,Method::POST,&api.path("groups/v2/operations"),Some(&json!({"payload":payload,"signature":crypto::sign_digest(&api.identity.private,&digest)?,"deviceProof":proof}))).await?;
    let status = response["status"].as_u64().context("无效群操作响应")?;
    let value = crypto::json_strict(string(&response, "body")?)?;
    ensure!(
        (200..300).contains(&status),
        "GROUP_ERROR: HTTP {status} {}",
        value["message"]
            .as_str()
            .or(value["code"].as_str())
            .unwrap_or("群操作失败")
    );
    Ok(value)
}
async fn publish_device(
    store: &SharedStore,
    api: &Api,
    target: &Value,
    device: &Value,
) -> Result<()> {
    operation(
        store,
        api,
        target,
        "PUT",
        &format!("device-credentials/{}", string(device, "id")?),
        Some(&device["credential"]),
        None,
    )
    .await?;
    Ok(())
}
pub async fn publish_inbox(store: &SharedStore, api: &Api) -> Result<()> {
    let publication = store
        .lock()
        .unwrap()
        .get("publication", "current")?
        .context("请等待中继身份发布完成")?;
    let route = &publication["route"];
    let record = crypto::sign_document(
        "mox:group:delivery-route:v1",
        json!({"version":1,"ownerPub":api.identity.public,"routeEpoch":route["routeEpoch"],"inboxes":route["mailboxes"],"issuedAt":route["issuedAt"],"expiresAt":route["expiresAt"]}),
        &api.identity.private,
    )?;
    for base in messaging::bases(store)? {
        api.ensure_session(&base).await?;
        api.request(
            &base,
            Method::PUT,
            &api.path("groups/v2/delivery-route"),
            Some(&record),
        )
        .await?;
    }
    Ok(())
}
pub fn receive_control(s: &Store, peer: &str, text: &str) -> Result<bool> {
    let (payload, name) = if let Some(raw) = text.strip_prefix("__group_invite__:") {
        let v = crypto::json_strict(raw)?;
        (v["groupV2Invitation"].clone(), v["groupName"].clone())
    } else if let Some(raw) = text.strip_prefix("__group_v2_control__:") {
        (crypto::json_strict(raw)?, Value::Null)
    } else {
        return Ok(false);
    };
    let i = s.identity()?;
    ensure!(payload["version"] == 1, "无效群控制版本");
    let rid = string(&payload, "invitationId")?;
    group_bytes(string(&payload, "groupId")?)?;
    match string(&payload, "type")? {
        "mox.group-v2.invitation" => {
            ensure!(
                payload["inviterPub"] == peer && payload["targetPub"] == i.public,
                "群邀请身份不匹配"
            );
            verify_multi(
                RELAY,
                &payload["relaySet"],
                string(&payload, "groupOwnerPub")?,
            )?;
            ensure!(
                payload["relaySet"]["groupId"] == payload["groupId"],
                "群邀请路由不匹配"
            );
            if s.get("group_invitation", rid)?.is_none() {
                s.put("group_invitation",rid,&json!({"id":rid,"direction":"incoming","state":"invited","name":name,"invitation":payload}))?;
            }
        }
        "mox.group-v2.key-package-ready" => {
            ensure!(
                payload["targetPub"] == peer && payload["inviterPub"] == i.public,
                "群申请身份不匹配"
            );
            let mut invite = s.get("group_invitation", rid)?.context("群邀请不存在")?;
            ensure!(
                invite["direction"] == "outgoing"
                    && invite["invitation"]["targetPub"] == peer
                    && invite["invitation"]["groupId"] == payload["groupId"],
                "群申请与邀请不匹配"
            );
            invite["response"] = payload.clone();
            invite["state"] = json!("approval_pending");
            s.put("group_invitation", rid, &invite)?;
        }
        "mox.group-v2.join-decision" => {
            ensure!(
                payload["inviterPub"] == peer && payload["targetPub"] == i.public,
                "群决定身份不匹配"
            );
            let mut invite = s.get("group_invitation", rid)?.context("群邀请不存在")?;
            ensure!(
                invite["invitation"]["inviterPub"] == peer
                    && invite["invitation"]["groupId"] == payload["groupId"],
                "群决定与邀请不匹配"
            );
            if payload["decision"] == "rejected" {
                invite["state"] = json!("rejected");
                s.put("group_invitation", rid, &invite)?;
            }
        }
        _ => anyhow::bail!("未知群控制消息"),
    }
    s.event("group.invitation", payload)?;
    Ok(true)
}
pub async fn command(store: &SharedStore, api: &Api, cmd: GroupCommand) -> Result<Value> {
    let _guard = serial().lock().await;
    match cmd {
        GroupCommand::Invitations => Ok(json!(store.lock().unwrap().list("group_invitation")?)),
        GroupCommand::Create { name } => {
            ensure!(!name.trim().is_empty(), "群名不能为空");
            let (mut mls, device) = { open(&store.lock().unwrap())? };
            let base = messaging::bases(store)?
                .into_iter()
                .next()
                .context("NO_RELAY")?;
            let pin = store
                .lock()
                .unwrap()
                .get("pin", &base)?
                .context("请等待中继身份发布完成")?;
            ensure!(
                pin["capabilities"]["groupHome"] == true,
                "中继不支持群 Home"
            );
            let target =
                json!({"nodeId":pin["nodeId"],"serverPub":pin["serverPub"],"baseUrl":base});
            let id = crypto::b64(mls.create_group()?.group_id);
            let now = crypto::now();
            let relays = multi_sign(
                RELAY,
                json!({"version":1,"groupId":id,"relayEpoch":1,"primary":target,"replicas":[],"validUntil":now+30*86400000,"previousStateHeadHash":"0".repeat(64)}),
                &api.identity,
            )?;
            let descriptor = multi_sign(
                DESCRIPTOR,
                json!({"version":2,"groupId":id,"controlEpoch":0,"ownerPub":api.identity.public,"adminPubs":[],"mlsCipherSuite":"MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519","mlsEpoch":0,"relaySet":{"relayEpoch":1,"primaryNodeId":pin["nodeId"],"replicaNodeIds":[]},"stateHeadHash":"0".repeat(64),"createdAt":now}),
                &api.identity,
            )?;
            let mut group = json!({"id":id,"type":"group","name":name,"descriptor":descriptor,"descriptorHash":record_hash(DESCRIPTOR,&descriptor)?,"relaySet":relays,"controlSeq":0,"mlsEpoch":0,"stateHeadHash":"0".repeat(64),"ready":false,"joinedControlSeq":0,"devices":{string(&device,"id")?:api.identity.public},"roles":{&api.identity.public:"owner"},"afterSeq":0});
            store.lock().unwrap().put("group", &id, &group)?;
            publish_device(store, api, &target, &device).await?;
            publish_inbox(store, api).await?;
            let response=operation(store,api,&target,"PUT",&id,Some(&json!({"version":1,"descriptor":descriptor,"relaySet":relays,"ownerDeviceId":device["id"]})),Some(&device)).await?;
            ensure!(
                response["descriptorHash"] == group["descriptorHash"]
                    && response["head"]["groupId"] == id,
                "群创建响应不匹配"
            );
            group["ready"] = json!(true);
            save_group(store, &group)?;
            Ok(group)
        }
        GroupCommand::Invite { id, public_key } => {
            let g = load_group(store, &id)?;
            ensure!(
                g["ready"] == true
                    && matches!(
                        g["roles"][&api.identity.public].as_str(),
                        Some("owner" | "admin")
                    ),
                "GROUP_FORBIDDEN: 需要群主或管理员权限"
            );
            let rid = crypto::id();
            let invitation = json!({"version":1,"type":"mox.group-v2.invitation","invitationId":rid,"groupId":id,"groupOwnerPub":g["descriptor"]["ownerPub"],"inviterPub":api.identity.public,"targetPub":public_key,"descriptorHash":g["descriptorHash"],"relaySet":g["relaySet"],"createdAt":crypto::now()});
            let text = format!(
                "__group_invite__:{}",
                json!({"groupName":g["name"],"groupPub":id,"inviterPub":api.identity.public,"inviterLabel":api.identity.name,"groupV2Invitation":invitation})
            );
            store.lock().unwrap().put(
                "group_invitation",
                &rid,
                &json!({"id":rid,"direction":"outgoing","state":"pending","invitation":invitation}),
            )?;
            messaging::send_dm(store, api, &format!("dm:{public_key}"), &text).await?;
            Ok(json!({"invitationId":rid}))
        }
        GroupCommand::Join { invitation } => join(store, api, &invitation).await,
        GroupCommand::Members { id } => {
            let g = sync_state(store, api, load_group(store, &id)?).await?;
            Ok(g["members"].clone())
        }
        GroupCommand::Requests { id } => Ok(json!(store
            .lock()
            .unwrap()
            .list("group_invitation")?
            .into_iter()
            .filter(|v| v["invitation"]["groupId"] == id)
            .collect::<Vec<_>>())),
        GroupCommand::Approve { id, request_id } => approve(store, api, &id, &request_id).await,
        GroupCommand::Reject { id, request_id } => {
            let mut invite = store
                .lock()
                .unwrap()
                .get("group_invitation", &request_id)?
                .context("群申请不存在")?;
            ensure!(
                invite["invitation"]["groupId"] == id && invite["direction"] == "outgoing",
                "群申请不匹配"
            );
            let peer = string(&invite["invitation"], "targetPub")?.to_owned();
            let response = json!({"version":1,"type":"mox.group-v2.join-decision","invitationId":request_id,"groupId":id,"inviterPub":api.identity.public,"targetPub":peer,"decision":"rejected","createdAt":crypto::now()});
            messaging::send_dm(
                store,
                api,
                &format!("dm:{peer}"),
                &format!("__group_v2_control__:{response}"),
            )
            .await?;
            invite["state"] = json!("rejected");
            store
                .lock()
                .unwrap()
                .put("group_invitation", &request_id, &invite)?;
            Ok(invite)
        }
        GroupCommand::Remove { id, public_key } => remove(store, api, &id, &public_key).await,
        GroupCommand::Leave { id } => {
            let mut g = sync_state(store, api, load_group(store, &id)?).await?;
            ensure!(
                g["descriptor"]["ownerPub"] != api.identity.public,
                "GROUP_OWNER_CANNOT_LEAVE: 群主需先转让或解散群聊"
            );
            let leave = crypto::sign_document(
                "mox:group:leave-request:v1",
                json!({"version":1,"groupId":id,"userPub":api.identity.public,"joinedControlSeq":g["joinedControlSeq"],"createdAt":crypto::now()}),
                &api.identity.private,
            )?;
            operation(
                store,
                api,
                &g["relaySet"]["primary"],
                "PUT",
                &format!("{id}/leave"),
                Some(&leave),
                None,
            )
            .await?;
            g["ready"] = json!(false);
            g["leavePending"] = json!(true);
            save_group(store, &g)?;
            Ok(json!({"groupId":id,"state":"leave_pending"}))
        }
    }
}
fn load_group(store: &SharedStore, id: &str) -> Result<Value> {
    store
        .lock()
        .unwrap()
        .get("group", id)?
        .context("GROUP_NOT_FOUND: 群不存在")
}
fn save_group(store: &SharedStore, g: &Value) -> Result<()> {
    let s = store.lock().unwrap();
    let id = string(g, "id")?;
    s.put("group", id, g)?;
    s.put(
        "chat",
        id,
        &json!({"id":id,"type":"group","name":g["name"],"ready":g["ready"]}),
    )
}
async fn join(store: &SharedStore, api: &Api, rid: &str) -> Result<Value> {
    let mut invite = store
        .lock()
        .unwrap()
        .get("group_invitation", rid)?
        .context("INVITATION_NOT_FOUND: 请使用收到的群邀请 ID")?;
    ensure!(
        invite["direction"] == "incoming" && invite["state"] != "rejected",
        "群邀请不可用"
    );
    let (mut mls, device) = { open(&store.lock().unwrap())? };
    let invitation = invite["invitation"].clone();
    let id = string(&invitation, "groupId")?;
    let target = &invitation["relaySet"]["primary"];
    if invite["keyPackage"].is_null() {
        let kp = mls.generate_key_package()?;
        let now = crypto::now();
        invite["keyPackage"] = crypto::sign_document(
            KEY_PACKAGE,
            json!({"version":1,"ownerPub":api.identity.public,"deviceId":device["id"],"deviceCredentialHash":device["hash"],"keyPackageRef":crypto::b64(kp.key_package_ref),"keyPackage":crypto::b64(&kp.tls_bytes),"keyPackageHash":crypto::hash(kp.tls_bytes),"issuedAt":now,"expiresAt":now+7*86400000}),
            &api.identity.private,
        )?;
        store
            .lock()
            .unwrap()
            .put("group_invitation", rid, &invite)?;
    }
    publish_device(store, api, target, &device).await?;
    publish_inbox(store, api).await?;
    operation(store,api,target,"POST",&format!("{id}/key-packages"),Some(&json!({"version":1,"credential":device["credential"],"keyPackage":invite["keyPackage"]})),Some(&device)).await?;
    let response = json!({"version":1,"type":"mox.group-v2.key-package-ready","invitationId":rid,"groupId":id,"groupOwnerPub":invitation["groupOwnerPub"],"inviterPub":invitation["inviterPub"],"targetPub":api.identity.public,"targetDeviceId":device["id"],"keyPackageRef":invite["keyPackage"]["keyPackageRef"],"createdAt":crypto::now()});
    messaging::send_dm(
        store,
        api,
        &format!("dm:{}", string(&invitation, "inviterPub")?),
        &format!("__group_v2_control__:{response}"),
    )
    .await?;
    invite["state"] = json!("awaiting_welcome");
    store
        .lock()
        .unwrap()
        .put("group_invitation", rid, &invite)?;
    Ok(invite)
}
async fn approve(store: &SharedStore, api: &Api, id: &str, rid: &str) -> Result<Value> {
    let mut invite = store
        .lock()
        .unwrap()
        .get("group_invitation", rid)?
        .context("群申请不存在")?;
    ensure!(
        invite["direction"] == "outgoing" && invite["invitation"]["groupId"] == id,
        "群申请不匹配"
    );
    let response = &invite["response"];
    let peer = string(response, "targetPub")?.to_owned();
    let did = string(response, "targetDeviceId")?.to_owned();
    let keyref = string(response, "keyPackageRef")?.to_owned();
    let mut g = load_group(store, id)?;
    let (mut mls, _) = open(&store.lock().unwrap())?;
    let cached = { store.lock().unwrap().get("group_operation", rid)? };
    let prepared = if let Some(v) = cached {
        v
    } else {
        g = sync_state(store, api, g).await?;
        ensure!(
            matches!(
                g["roles"][&api.identity.public].as_str(),
                Some("owner" | "admin")
            ),
            "GROUP_FORBIDDEN"
        );
        let packages = operation(
            store,
            api,
            &g["relaySet"]["primary"],
            "GET",
            &format!("{id}/key-packages?targetPub={peer}&deviceId={did}"),
            None,
            None,
        )
        .await?;
        let kp = packages["keyPackages"]
            .as_array()
            .context("缺少 KeyPackage")?
            .iter()
            .find(|k| k["keyPackageRef"] == keyref)
            .context("KeyPackage 不存在")?;
        ensure!(
            kp["ownerPub"] == peer
                && kp["deviceId"] == did
                && kp["expiresAt"].as_u64().unwrap_or(0) > crypto::now(),
            "KeyPackage 身份错误或过期"
        );
        crypto::verify_document(KEY_PACKAGE, kp, &peer)?;
        let bytes = crypto::unb64(string(kp, "keyPackage")?)?;
        ensure!(
            crypto::hash(&bytes) == kp["keyPackageHash"],
            "KeyPackage 摘要错误"
        );
        let added = mls.prepare_add_member(&group_bytes(id)?, rid, &bytes)?;
        let now = crypto::now();
        let control = crypto::sign_document(
            CONTROL,
            json!({"version":1,"groupId":id,"controlSeq":g["controlSeq"].as_u64().unwrap_or(0)+1,"previousHash":g["stateHeadHash"],"action":"member_add","actorPub":api.identity.public,"targetUserPub":peer,"targetDeviceIds":[did],"mlsCommitHash":added.commit_hash,"resultingMlsEpoch":added.epoch,"createdAt":now}),
            &api.identity.private,
        )?;
        let hash = record_hash(CONTROL, &control)?;
        let welcome = added
            .welcome_tls_bytes
            .first()
            .context("Missing MLS Welcome")?;
        let request = json!({"version":1,"control":{"version":1,"record":control,"mlsCommit":crypto::b64(&added.commit_tls_bytes)},"welcomes":[{"envelope":{"version":1,"groupId":id,"recipientPub":peer,"recipientDeviceId":did,"keyPackageRef":keyref,"controlRecordHash":hash,"mlsEpoch":added.epoch,"stateHeadHash":hash,"welcome":crypto::b64(welcome),"welcomeHash":crypto::hash(welcome),"createdAt":now,"expiresAt":kp["expiresAt"]}}]});
        store
            .lock()
            .unwrap()
            .put("group_operation", rid, &request)?;
        request
    };
    let response = operation(
        store,
        api,
        &g["relaySet"]["primary"],
        "POST",
        &format!("{id}/joins"),
        Some(&prepared),
        None,
    )
    .await?;
    let record = &prepared["control"]["record"];
    let hash = record_hash(CONTROL, record)?;
    ensure!(
        response["control"]["recordHash"] == hash,
        "群批准响应不匹配"
    );
    mls.commit_prepared_add_member(&group_bytes(id)?, rid, string(record, "mlsCommitHash")?)?;
    g["controlSeq"] = record["controlSeq"].clone();
    g["mlsEpoch"] = record["resultingMlsEpoch"].clone();
    g["stateHeadHash"] = json!(hash);
    g["devices"][&did] = json!(peer);
    g["roles"][&peer] = json!("member");
    save_group(store, &g)?;
    invite["state"] = json!("approved");
    store
        .lock()
        .unwrap()
        .put("group_invitation", rid, &invite)?;
    Ok(invite)
}
async fn sync_state(store: &SharedStore, api: &Api, mut g: Value) -> Result<Value> {
    let id = string(&g, "id")?.to_owned();
    let (mut mls, device) = open(&store.lock().unwrap())?;
    loop {
        let state = operation(
            store,
            api,
            &g["relaySet"]["primary"],
            "GET",
            &format!("{id}/state?afterControlSeq={}&limit=50", g["controlSeq"]),
            None,
            None,
        )
        .await?;
        ensure!(
            state["descriptorHash"] == g["descriptorHash"]
                && state["head"]["groupId"] == id
                && state["head"]["forked"] != true,
            "群状态冲突"
        );
        for item in state["controls"].as_array().into_iter().flatten() {
            let r = &item["record"];
            let actor = string(r, "actorPub")?;
            ensure!(
                r["groupId"] == id
                    && r["controlSeq"].as_u64() == Some(g["controlSeq"].as_u64().unwrap_or(0) + 1)
                    && r["previousHash"] == g["stateHeadHash"],
                "群控制链不连续"
            );
            crypto::verify_document(CONTROL, r, actor)?;
            ensure!(
                record_hash(CONTROL, r)? == item["recordHash"],
                "群控制摘要不匹配"
            );
            ensure!(
                matches!(g["roles"][actor].as_str(), Some("owner" | "admin")),
                "群控制者权限无效"
            );
            if let Some(commit) = item["mlsCommit"].as_str() {
                let bytes = crypto::unb64(commit)?;
                ensure!(
                    crypto::hash(&bytes) == r["mlsCommitHash"],
                    "MLS Commit 摘要错误"
                );
                let epoch = mls.group_diagnostics(&group_bytes(&id)?)?.epoch;
                let expected = r["resultingMlsEpoch"]
                    .as_u64()
                    .context("Missing MLS epoch")?;
                if epoch < expected {
                    ensure!(epoch + 1 == expected, "MLS epoch 不连续");
                    let processed = if matches!(
                        r["action"].as_str(),
                        Some("member_remove" | "member_leave" | "device_remove")
                    ) {
                        let ids = r["targetDeviceIds"]
                            .as_array()
                            .context("缺少移除设备列表")?
                            .iter()
                            .map(|v| Ok(v.as_str().context("无效设备 ID")?.to_owned()))
                            .collect::<Result<Vec<_>>>()?;
                        mls.process_remove_commit(&group_bytes(&id)?, &bytes, &ids)?
                    } else {
                        mls.process_message(&group_bytes(&id)?, &bytes)?
                    };
                    ensure!(
                        matches!(processed,ProcessedMessage::Commit{epoch,..} if epoch==expected),
                        "MLS Commit epoch 不匹配"
                    );
                }
                g["mlsEpoch"] = json!(expected);
            }
            let target = r["targetUserPub"].as_str().unwrap_or("");
            match r["action"].as_str().unwrap_or("") {
                "member_add" | "device_add" => {
                    for did in r["targetDeviceIds"].as_array().into_iter().flatten() {
                        if let Some(did) = did.as_str() {
                            g["devices"][did] = json!(target);
                        }
                    }
                    if r["action"] == "member_add" {
                        g["roles"][target] = json!("member");
                    }
                }
                "member_remove" | "member_leave" | "device_remove" => {
                    for did in r["targetDeviceIds"].as_array().into_iter().flatten() {
                        if did == &device["id"] {
                            g["ready"] = json!(false);
                        }
                        if let Some(did) = did.as_str() {
                            g["devices"].as_object_mut().unwrap().remove(did);
                        }
                    }
                    if r["action"] != "device_remove" {
                        g["roles"].as_object_mut().unwrap().remove(target);
                    }
                }
                "role_set" => g["roles"][target] = r["targetRole"].clone(),
                "group_close" => g["ready"] = json!(false),
                _ => {}
            }
            g["controlSeq"] = r["controlSeq"].clone();
            g["stateHeadHash"] = item["recordHash"].clone();
            save_group(store, &g)?;
        }
        if state["hasMore"] != true {
            ensure!(
                state["head"]["controlSeq"] == g["controlSeq"]
                    && state["head"]["stateHeadHash"] == g["stateHeadHash"],
                "群控制链缺少记录"
            );
            g["members"] = state["members"].clone();
            save_group(store, &g)?;
            break;
        }
    }
    Ok(g)
}
async fn remove(store: &SharedStore, api: &Api, id: &str, peer: &str) -> Result<Value> {
    let mut g = sync_state(store, api, load_group(store, id)?).await?;
    ensure!(
        matches!(
            g["roles"][&api.identity.public].as_str(),
            Some("owner" | "admin")
        ) && g["descriptor"]["ownerPub"] != peer,
        "GROUP_FORBIDDEN"
    );
    let task = format!(
        "remove:{}",
        crypto::hash(format!("{id}:{peer}:{}", g["stateHeadHash"]))
    );
    let (mut mls, _) = open(&store.lock().unwrap())?;
    let cached = { store.lock().unwrap().get("group_operation", &task)? };
    let prepared = if let Some(p) = cached {
        p
    } else {
        let ids = g["devices"]
            .as_object()
            .context("缺少群设备状态")?
            .iter()
            .filter(|(_, p)| p.as_str() == Some(peer))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        ensure!(!ids.is_empty(), "群成员设备不存在");
        let removed = mls.prepare_remove_members(&group_bytes(id)?, &task, &ids)?;
        let record = crypto::sign_document(
            CONTROL,
            json!({"version":1,"groupId":id,"controlSeq":g["controlSeq"].as_u64().unwrap_or(0)+1,"previousHash":g["stateHeadHash"],"action":"member_remove","actorPub":api.identity.public,"targetUserPub":peer,"targetDeviceIds":ids,"mlsCommitHash":removed.commit_hash,"resultingMlsEpoch":removed.epoch,"createdAt":crypto::now()}),
            &api.identity.private,
        )?;
        let request =
            json!({"version":1,"record":record,"mlsCommit":crypto::b64(removed.commit_tls_bytes)});
        store
            .lock()
            .unwrap()
            .put("group_operation", &task, &request)?;
        request
    };
    let result = operation(
        store,
        api,
        &g["relaySet"]["primary"],
        "POST",
        &format!("{id}/control"),
        Some(&prepared),
        None,
    )
    .await?;
    let r = &prepared["record"];
    ensure!(
        result["recordHash"] == record_hash(CONTROL, r)?,
        "成员移除响应不匹配"
    );
    mls.commit_prepared_remove_members(&group_bytes(id)?, &task, string(r, "mlsCommitHash")?)?;
    g["controlSeq"] = r["controlSeq"].clone();
    g["mlsEpoch"] = r["resultingMlsEpoch"].clone();
    g["stateHeadHash"] = result["recordHash"].clone();
    g["roles"].as_object_mut().unwrap().remove(peer);
    for did in r["targetDeviceIds"].as_array().into_iter().flatten() {
        g["devices"]
            .as_object_mut()
            .unwrap()
            .remove(did.as_str().unwrap_or(""));
    }
    save_group(store, &g)?;
    Ok(result)
}
pub async fn send(store: &SharedStore, api: &Api, id: &str, text: &str) -> Result<Value> {
    let _guard = serial().lock().await;
    let g = load_group(store, id)?;
    ensure!(g["ready"] == true, "GROUP_NOT_READY: 尚未完成入群或已离开");
    // Keep every checkpoint within the existing 64 KiB MLS envelope limit.
    ensure!(
        text.len() <= 48 * 1024,
        "MESSAGE_TOO_LARGE: 群消息不能超过 48 KiB"
    );
    let (mut mls, device) = open(&store.lock().unwrap())?;
    let mid = crate::stream::transport_id(text, &format!("{}:{id}", api.identity.public))?;
    if let Some(result) = crate::stream::prepared(&store.lock().unwrap(), &mid, text)? {
        return Ok(result);
    }
    let now = crypto::now();
    if let Some(frame) = crate::stream::Frame::parse(text)? {
        let key = format!("{id}:{}", frame.stream_id);
        let s = store.lock().unwrap();
        if let Some(epoch) = s.get("group_stream_epoch", &key)? {
            ensure!(
                epoch == g["mlsEpoch"],
                "STREAM_MEMBERSHIP_CHANGED: 群成员发生变化，已停止向新成员重发此前正文"
            );
        } else {
            s.put("group_stream_epoch", &key, &g["mlsEpoch"])?;
        }
    }
    let plaintext=json!({"version":1,"type":"mox.group-v2.message","groupId":id,"messageId":mid,"senderPub":api.identity.public,"text":text,"createdAt":now}).to_string();
    let prepared =
        mls.prepare_application_message(&group_bytes(id)?, &mid, &mid, plaintext.as_bytes())?;
    let mut envelope = json!({"version":2,"groupId":id,"messageId":mid,"senderPub":api.identity.public,"senderDeviceId":device["id"],"mlsEpoch":prepared.epoch,"stateHeadHash":g["stateHeadHash"],"contentType":"application/mls","ciphertext":crypto::b64(&prepared.ciphertext_tls_bytes),"ciphertextHash":prepared.ciphertext_hash,"createdAt":now,"expiresAt":now+30*86400000});
    envelope["senderSignature"] = json!(crypto::sign_digest(
        string(&device, "private")?,
        &crypto::digest("mox:group:application-envelope:v1", &envelope)?
    )?);
    let s = store.lock().unwrap();
    s.db.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        s.put("group_outbox",&mid,&json!({"version":1,"envelope":envelope,"knownRelayEpoch":g["relaySet"]["relayEpoch"],"knownHomeNodeId":g["relaySet"]["primary"]["nodeId"],"relaySetHint":g["relaySet"],"descriptorHint":g["descriptor"]}))?;
        messaging::record(&s, id, &api.identity.public, &mid, text, now, "queued")?;
        let result = json!({"id":mid,"chat":id,"status":"queued"});
        crate::stream::save_prepared(&s, &mid, text, &result)?;
        Ok::<_, anyhow::Error>(result)
    })();
    match &result {
        Ok(_) => s.db.execute_batch("COMMIT")?,
        Err(_) => s.db.execute_batch("ROLLBACK")?,
    }
    result
}
pub async fn recipients(store: &SharedStore, api: &Api, id: &str) -> Result<Vec<String>> {
    let _guard = serial().lock().await;
    let group = sync_state(store, api, load_group(store, id)?).await?;
    ensure!(group["ready"] == true, "GROUP_NOT_READY");
    let recipients = group["roles"]
        .as_object()
        .context("缺少群成员状态")?
        .keys()
        .filter(|p| **p != api.identity.public)
        .cloned()
        .collect::<Vec<_>>();
    ensure!(
        !recipients.is_empty(),
        "GROUP_NO_RECIPIENT: 群中没有其他成员"
    );
    Ok(recipients)
}
pub async fn tick(store: &SharedStore, api: &Api) -> Result<()> {
    let _guard = serial().lock().await;
    let invitations = store.lock().unwrap().list("group_invitation")?;
    for invite in invitations
        .iter()
        .filter(|v| v["state"] == "awaiting_welcome")
    {
        if let Err(e) = receive_welcome(store, api, invite.clone()).await {
            store.lock().unwrap().put(
                "runtime",
                "group_error",
                &json!({"error":e.to_string(),"at":crypto::now()}),
            )?;
        }
    }
    let groups = store.lock().unwrap().list("group")?;
    for group in groups.into_iter().filter(|g| g["ready"] == true) {
        let result = sync_group(store, api, group).await;
        if let Err(e) = result {
            store.lock().unwrap().put(
                "runtime",
                "group_error",
                &json!({"error":e.to_string(),"at":crypto::now()}),
            )?;
        }
    }
    Ok(())
}
pub async fn flush(store: &SharedStore, api: &Api) -> Result<()> {
    let outbox = store.lock().unwrap().list("group_outbox")?;
    for item in outbox.into_iter().take(16) {
        let envelope = &item["envelope"];
        let gid = string(envelope, "groupId")?;
        let mid = string(envelope, "messageId")?;
        for base in messaging::bases(store)? {
            api.ensure_session(&base).await?;
            let response = api
                .request(
                    &base,
                    Method::POST,
                    &api.path(&format!("groups/v2/{gid}/messages")),
                    Some(&item),
                )
                .await?;
            ensure!(
                response["messageId"] == mid
                    && response["ciphertextHash"] == envelope["ciphertextHash"],
                "群消息投递响应不匹配"
            );
            if response["disposition"] == "home_durable" || response["disposition"] == "queued" {
                store.lock().unwrap().delete("group_outbox", mid)?;
                break;
            }
        }
    }
    Ok(())
}
async fn receive_welcome(store: &SharedStore, api: &Api, mut invite: Value) -> Result<()> {
    let invitation = &invite["invitation"];
    let gid = string(invitation, "groupId")?.to_owned();
    let rid = string(&invite, "id")?.to_owned();
    let (mut mls, device) = open(&store.lock().unwrap())?;
    let kp = &invite["keyPackage"];
    let keyref = string(kp, "keyPackageRef")?;
    let target = &invitation["relaySet"]["primary"];
    let response = operation(
        store,
        api,
        target,
        "GET",
        &format!("{gid}/welcomes/{keyref}"),
        None,
        Some(&device),
    )
    .await?;
    let welcome = &response["envelope"];
    let control = &response["control"]["record"];
    let descriptor = &response["descriptor"];
    verify_multi(DESCRIPTOR, descriptor, string(invitation, "groupOwnerPub")?)?;
    ensure!(
        record_hash(DESCRIPTOR, descriptor)? == invitation["descriptorHash"]
            && descriptor["groupId"] == gid,
        "Welcome 群描述不匹配"
    );
    crypto::verify_document(CONTROL, control, string(invitation, "inviterPub")?)?;
    let hash = record_hash(CONTROL, control)?;
    ensure!(
        welcome["recipientPub"] == api.identity.public
            && welcome["recipientDeviceId"] == device["id"]
            && welcome["groupId"] == gid
            && welcome["keyPackageRef"] == kp["keyPackageRef"]
            && welcome["controlRecordHash"] == hash
            && welcome["stateHeadHash"] == hash
            && welcome["mlsEpoch"] == control["resultingMlsEpoch"]
            && control["targetUserPub"] == api.identity.public
            && control["targetDeviceIds"]
                .as_array()
                .is_some_and(|ids| ids.contains(&device["id"]))
            && welcome["expiresAt"].as_u64().unwrap_or(0) > crypto::now(),
        "Welcome 绑定不匹配"
    );
    let bytes = crypto::unb64(string(welcome, "welcome")?)?;
    ensure!(
        crypto::hash(&bytes) == welcome["welcomeHash"],
        "Welcome 摘要不匹配"
    );
    let expected = welcome["mlsEpoch"].as_u64().context("缺少 MLS epoch")?;
    match mls.group_diagnostics(&group_bytes(&gid)?) {
        Ok(d) if d.epoch == expected => {}
        _ => {
            let joined = mls.join_group(&bytes)?;
            ensure!(
                joined.group_id == group_bytes(&gid)? && joined.epoch == expected,
                "MLS Welcome 结果不匹配"
            );
        }
    }
    let owner = string(descriptor, "ownerPub")?;
    let group = json!({"id":gid,"type":"group","name":invite["name"].as_str().unwrap_or(&gid),"descriptor":descriptor,"descriptorHash":invitation["descriptorHash"],"relaySet":invitation["relaySet"],"controlSeq":0,"mlsEpoch":expected,"stateHeadHash":"0".repeat(64),"ready":true,"joinedControlSeq":control["controlSeq"],"devices":{},"roles":{owner:"owner"},"afterSeq":response["control"]["groupSeq"]});
    save_group(store, &group)?;
    operation(
        store,
        api,
        target,
        "POST",
        &format!("{gid}/welcomes/{keyref}/ack"),
        Some(&json!({"version":1,"welcomeHash":welcome["welcomeHash"]})),
        Some(&device),
    )
    .await?;
    invite["state"] = json!("ready");
    store
        .lock()
        .unwrap()
        .put("group_invitation", &rid, &invite)?;
    Ok(())
}
async fn sync_group(store: &SharedStore, api: &Api, group: Value) -> Result<()> {
    let mut group = sync_state(store, api, group).await?;
    if group["ready"] != true {
        return Ok(());
    }
    let gid = string(&group, "id")?.to_owned();
    let (mut mls, device) = open(&store.lock().unwrap())?;
    let base = messaging::bases(store)?
        .into_iter()
        .next()
        .context("NO_RELAY")?;
    let pin = store
        .lock()
        .unwrap()
        .get("pin", &base)?
        .context("Missing relay identity")?;
    let target = json!({"nodeId":pin["nodeId"],"serverPub":pin["serverPub"],"baseUrl":base});
    publish_device(store, api, &target, &device).await?;
    let response = operation(
        store,
        api,
        &target,
        "GET",
        &format!("{gid}/sync?afterSeq={}&limit=50", group["afterSeq"]),
        None,
        Some(&device),
    )
    .await?;
    ensure!(
        response["groupId"] == gid && response["deviceId"] == device["id"],
        "群同步目标不匹配"
    );
    let mut deliveries = Vec::<Value>::new();
    for field in ["pendingBeforeCursor", "items"] {
        deliveries.extend(response[field].as_array().into_iter().flatten().cloned());
    }
    deliveries.sort_by_key(|v| v["groupSeq"].as_u64().unwrap_or(0));
    deliveries.dedup_by(|a, b| a["deliveryId"] == b["deliveryId"]);
    for delivery in deliveries {
        let env = &delivery["envelope"];
        let mid = string(env, "messageId")?;
        let key = format!("group:{gid}:{}:{mid}", string(env, "senderDeviceId")?);
        let seen = store.lock().unwrap().get("group_received", &key)?.is_some();
        if !seen {
            ensure!(
                env["version"] == 2
                    && env["groupId"] == gid
                    && env["contentType"] == "application/mls",
                "群密文绑定错误"
            );
            let ciphertext = crypto::unb64(string(env, "ciphertext")?)?;
            ensure!(
                crypto::hash(&ciphertext) == env["ciphertextHash"],
                "群密文摘要错误"
            );
            let sender = string(env, "senderPub")?;
            let own = sender == api.identity.public && env["senderDeviceId"] == device["id"];
            if !own {
                let processed = mls.process_application(&group_bytes(&gid)?, &ciphertext)?;
                let ProcessedMessage::Application {
                    plaintext, epoch, ..
                } = processed
                else {
                    anyhow::bail!("无效 MLS application")
                };
                ensure!(Some(epoch) == env["mlsEpoch"].as_u64(), "MLS epoch 不匹配");
                let payload = crypto::json_strict(std::str::from_utf8(&plaintext)?)?;
                ensure!(
                    payload["version"] == 1
                        && payload["type"] == "mox.group-v2.message"
                        && payload["groupId"] == gid
                        && payload["messageId"] == mid
                        && payload["senderPub"] == sender,
                    "群应用消息身份不匹配"
                );
                let s = store.lock().unwrap();
                let text = string(&payload, "text")?;
                if let Some(message) = messaging::record(
                    &s,
                    &gid,
                    sender,
                    mid,
                    text,
                    payload["createdAt"].as_u64().context("无效消息时间")?,
                    "received",
                )? {
                    s.put(
                        "ai_queue",
                        &format!("{:016}:{mid}", crypto::now()),
                        &message,
                    )?;
                }
            }
            store.lock().unwrap().put(
                "group_received",
                &key,
                &json!({"ciphertextHash":env["ciphertextHash"]}),
            )?;
        }
        operation(store,api,&target,"POST",&format!("{gid}/acks"),Some(&json!({"version":1,"items":[{"deliveryId":delivery["deliveryId"],"groupSeq":delivery["groupSeq"],"messageId":mid,"ciphertextHash":env["ciphertextHash"]}]})),Some(&device)).await?;
    }
    // Gaps are repaired by the ingress/Home backfill mechanism; never skip a reported gap.
    if response["gaps"].as_array().is_some_and(Vec::is_empty) {
        group["afterSeq"] = response["nextAfterSeq"].clone();
        save_group(store, &group)?;
    }
    Ok(())
}
