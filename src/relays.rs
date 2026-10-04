use crate::{api, args::RelayCommand, store::Store};
use anyhow::{ensure, Result};
use serde_json::{json, Value};

pub const OFFICIAL_COMMUNICATION: &str = "https://mox.ponzs.com";
pub const OFFICIAL_FILE: &str = "https://file.ponzs.com";
pub fn default_url(file: bool) -> &'static str {
    if file {
        OFFICIAL_FILE
    } else {
        OFFICIAL_COMMUNICATION
    }
}
fn kind(file: bool) -> &'static str {
    if file {
        "file_relay"
    } else {
        "relay"
    }
}
fn setting(file: bool) -> &'static str {
    if file {
        "selected_file_relay"
    } else {
        "selected_relay"
    }
}
pub fn selected(s: &Store, file: bool) -> Result<String> {
    Ok(s.get("settings", setting(file))?
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| default_url(file).into()))
}
pub fn command(store: Option<&Store>, file: bool, command: RelayCommand) -> Result<Value> {
    let current = store
        .map(|s| selected(s, file))
        .transpose()?
        .unwrap_or_else(|| default_url(file).into());
    let kind = kind(file);
    match command {
        RelayCommand::Show => Ok(json!({"current":current,"official":default_url(file)})),
        RelayCommand::List => {
            let mut items = store.map(|s| s.list(kind)).transpose()?.unwrap_or_default();
            if items.is_empty() {
                items.push(json!({"url":default_url(file)}));
            }
            for item in &mut items {
                item["selected"] = json!(item["url"] == current);
                item["official"] = json!(item["url"] == default_url(file));
            }
            Ok(json!({"current":current,"official":default_url(file),"items":items}))
        }
        mutation => {
            let s = store.ok_or_else(|| anyhow::anyhow!("NOT_LOGGED_IN: 请先运行 mox login"))?;
            if s.list(kind)?.is_empty() {
                s.put(kind, default_url(file), &json!({"url":default_url(file)}))?;
            }
            match mutation {
                RelayCommand::Add { url } => {
                    let url = api::origin(&url)?;
                    s.put(kind, &url, &json!({"url":url}))?;
                    Ok(json!({"added":url,"current":current}))
                }
                RelayCommand::Use { url } => {
                    let url = api::origin(&url)?;
                    ensure!(
                        s.get(kind, &url)?.is_some(),
                        "RELAY_NOT_FOUND: 中继未配置，请先 add 或使用 set"
                    );
                    s.put("settings", setting(file), &json!(url))?;
                    Ok(json!({"current":url}))
                }
                RelayCommand::Set { url } => {
                    let url = api::origin(&url)?;
                    s.put(kind, &url, &json!({"url":url}))?;
                    s.put("settings", setting(file), &json!(url))?;
                    Ok(json!({"current":url}))
                }
                RelayCommand::Remove { url } => {
                    let url = api::origin(&url)?;
                    ensure!(url != current, "RELAY_IN_USE: 请先切换中继再移除当前入口");
                    ensure!(s.get(kind, &url)?.is_some(), "RELAY_NOT_FOUND: 中继未配置");
                    s.delete(kind, &url)?;
                    Ok(json!({"removed":url,"current":current}))
                }
                _ => unreachable!(),
            }
        }
    }
}
