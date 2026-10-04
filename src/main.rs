mod ai;
mod api;
mod args;
mod codex;
mod crypto;
mod files;
mod group;
mod language;
mod messaging;
mod mls;
mod outgoing;
mod relays;
mod runtime;
mod share;
mod store;
mod stream;
mod tui;
mod update;
use anyhow::{ensure, Result};
use args::*;
use clap::{CommandFactory, Parser};
use runtime::Request;
use serde_json::{json, Value};
use std::{
    io::{IsTerminal, Write},
    path::Path,
    time::Duration,
};

#[tokio::main]
async fn main() {
    if std::env::args_os().len() == 1 {
        let _ = help(&[], false);
        return;
    }
    let mut cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            if e.use_stderr() && std::env::args_os().any(|a| a == "--json") {
                println!(
                    "{}",
                    json!({"error":{"code":"INVALID_ARGUMENT","message":e.to_string(),"retryable":false}})
                );
                std::process::exit(2);
            }
            e.exit();
        }
    };
    let selected_dir = cli.data_dir.clone();
    let result = execute(&mut cli).await;
    match result {
        Ok(Some(v)) => print_value(&v, cli.json),
        Ok(None) => (),
        Err(e) => {
            if cli.json {
                println!("{}", runtime::failure(&e))
            } else {
                let lang = store::data_dir(selected_dir)
                    .map(|dir| language::Language::load(&dir))
                    .unwrap_or_default();
                eprintln!("{}", lang.error(&format!("{e:#}")))
            }
            std::process::exit(1)
        }
    }
}
fn print_value(v: &Value, json: bool) {
    if json {
        println!("{v}")
    } else {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default())
    }
}
fn prompt_name(dir: &Path) -> Result<String> {
    let lang = language::Language::load(dir);
    ensure!(
        std::io::stdin().is_terminal(),
        "NICKNAME_REQUIRED: {}",
        lang.text(
            "Run mox login --name <nickname> first",
            "首次登录请运行 mox login --name <昵称>"
        )
    );
    eprint!(
        "{}",
        lang.text(
            "Enter a nickname for your new account: ",
            "请输入新账号昵称："
        )
    );
    std::io::stderr().flush()?;
    let mut name = String::new();
    std::io::stdin().read_line(&mut name)?;
    Ok(name.trim().into())
}
fn open_identity(dir: &Path, name: Option<&str>) -> Result<store::Store> {
    let s = store::Store::open(dir, true)?;
    if s.get("identity", "current")?.is_none() {
        let name = match name {
            Some(v) => v.to_owned(),
            None => prompt_name(dir)?,
        };
        s.login(&name)?;
    }
    Ok(s)
}
fn help(path: &[String], machine: bool) -> Result<()> {
    let mut command = Cli::command();
    command.build();
    for name in path {
        command = command
            .find_subcommand(name)
            .ok_or_else(|| anyhow::anyhow!("UNKNOWN_COMMAND: Unknown command: {name}"))?
            .clone();
    }
    if machine {
        println!("{}", help_json(&mut command))
    } else {
        command.print_long_help()?;
        println!();
    }
    Ok(())
}
fn help_json(command: &mut clap::Command) -> Value {
    let help = command.render_long_help().to_string();
    let commands = command
        .get_subcommands_mut()
        .map(help_json)
        .collect::<Vec<_>>();
    json!({"name":command.get_name(),"about":command.get_about().map(ToString::to_string),"help":help,"commands":commands})
}
async fn execute(cli: &mut Cli) -> Result<Option<Value>> {
    if matches!(cli.command, Command::Version) {
        if cli.json {
            return Ok(Some(json!({"version":env!("CARGO_PKG_VERSION")})));
        }
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(None);
    }
    if let Command::Help { command } = &cli.command {
        help(command, cli.json)?;
        return Ok(None);
    }
    let dir = store::data_dir(cli.data_dir.take())?;
    if let Command::Lang { language } = cli.command {
        if let Some(value) = language {
            value.save(&dir)?;
        }
        return Ok(Some(json!({"language":language::Language::load(&dir)})));
    }
    if let Command::Message {
        command: MessageCommand::Send(args),
    } = &mut cli.command
    {
        for path in [&mut args.img, &mut args.video, &mut args.file]
            .into_iter()
            .flatten()
        {
            *path = files::check_path(path)?;
        }
    }
    let command = std::mem::replace(&mut cli.command, Command::Status);
    match command {
        Command::Qr => show_identity_code(&dir, true, cli.json).await,
        Command::Moxpub => show_identity_code(&dir, false, cli.json).await,
        Command::Update { check } => update::run(&dir, check).await.map(Some),
        Command::Start { no_ai } => {
            let s = open_identity(&dir, None)?;
            runtime::run(s, no_ai).await?;
            Ok(None)
        }
        Command::Tui { no_ai } => {
            let result = async {
                if !store::socket_path(&dir).exists() {
                    drop(open_identity(&dir, None)?);
                }
                tui::run(&dir, no_ai).await
            }
            .await;
            result.map_err(|e| {
                anyhow::anyhow!(language::Language::load(&dir).error(&format!("{e:#}")))
            })?;
            Ok(None)
        }
        Command::Logout => {
            if store::socket_path(&dir).exists() {
                let _ = runtime::call(&dir, &Request::Command(Command::Stop)).await;
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            let s = loop {
                match store::Store::open(&dir, false) {
                    Ok(s) => break s,
                    Err(e) => {
                        if !e.to_string().starts_with("RUNTIME_RUNNING")
                            || tokio::time::Instant::now() >= deadline
                        {
                            return Err(e);
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            };
            s.destroy()?;
            Ok(Some(json!({"logged_out":true,"identity_destroyed":true})))
        }
        Command::Events { mut after } => {
            loop {
                let rows =
                    runtime::call(&dir, &Request::Command(Command::Events { after })).await?;
                for row in rows.as_array().into_iter().flatten() {
                    println!("{row}");
                    after = row["id"].as_u64().unwrap_or(after);
                }
                tokio::select! {_=tokio::signal::ctrl_c()=>break,_=tokio::time::sleep(Duration::from_millis(100))=>{}}
            }
            Ok(None)
        }
        Command::Message {
            command: MessageCommand::Send(args),
        } if args.text_stdin => send_stdin(&dir, &args).await.map(Some),
        command => {
            let running = runtime::call(&dir, &Request::Command(Command::Status))
                .await
                .is_ok();
            if running {
                return runtime::call(&dir, &Request::Command(command))
                    .await
                    .map(Some);
            }
            let result = match command {
                Command::Status => {
                    json!({"running":false,"logged_in":dir.join("identity.db").exists()})
                }
                Command::Login { name } => {
                    let identity = open_identity(&dir, name.as_deref())?.identity()?.view();
                    eprintln!("{}", language::Language::load(&dir).text(
                        "Identity ready. Run mox start or mox tui to publish your profile and receive messages.",
                        "身份已就绪；运行 mox start 或 mox tui 后同步资料并接收消息。"
                    ));
                    identity
                }
                Command::Whoami => store::Store::open(&dir, false)?.identity()?.view(),
                Command::Chat {
                    command: ChatCommand::List,
                } => json!(store::Store::open(&dir, false)?.list("chat")?),
                Command::Message {
                    command: MessageCommand::List(args),
                } => store::Store::open(&dir, false)?.history(&args)?,
                Command::Friend { command } => {
                    runtime::friend_local(&store::Store::open(&dir, false)?, command)?
                }
                Command::Relay { command } => {
                    let store = if dir.join("identity.db").exists() {
                        Some(store::Store::open(&dir, false)?)
                    } else {
                        None
                    };
                    crate::relays::command(store.as_ref(), false, command)?
                }
                Command::FileRelay { command } => {
                    let store = if dir.join("identity.db").exists() {
                        Some(store::Store::open(&dir, false)?)
                    } else {
                        None
                    };
                    crate::relays::command(store.as_ref(), true, command)?
                }
                Command::Stop => json!({"running":false}),
                _ => anyhow::bail!("RUNTIME_NOT_RUNNING: 请先运行 mox start"),
            };
            Ok(Some(result))
        }
    }
}
async fn show_identity_code(dir: &Path, qr: bool, machine: bool) -> Result<Option<Value>> {
    let running = runtime::call(dir, &Request::Command(Command::Status))
        .await
        .is_ok();
    let mut data = if running {
        runtime::call(dir, &Request::Command(Command::Moxpub)).await?
    } else {
        ensure!(
            dir.join("identity.db").exists(),
            "NOT_LOGGED_IN: 请运行 mox login"
        );
        share::identity(&store::Store::open(dir, false)?)?
    };
    let link = data["moxpub"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("INVALID_RESPONSE: 缺少 moxpub 身份链接"))?
        .to_owned();
    if qr {
        let rendered = share::qr(&link)?;
        if machine {
            data["qr"] = json!(rendered.text);
            data["qr_columns"] = json!(rendered.columns);
        } else {
            let terminal = std::io::stdout().is_terminal();
            if terminal {
                if let Ok((width, _)) = crossterm::terminal::size() {
                    ensure!(
                        usize::from(width) >= rendered.columns,
                        "TERMINAL_TOO_NARROW: 二维码需要至少 {} 列，请加宽终端或使用 mox moxpub",
                        rendered.columns
                    );
                }
            }
            let mut out = std::io::stdout().lock();
            for row in rendered.text.lines() {
                if terminal {
                    // Explicit black on white, independent of the terminal theme.
                    writeln!(out, "\x1b[0;38;2;0;0;0;48;2;255;255;255m{row}\x1b[0m")?;
                } else {
                    writeln!(out, "{row}")?;
                }
            }
            writeln!(out, "\n{link}")?;
            return Ok(None);
        }
    }
    if machine {
        Ok(Some(data))
    } else {
        writeln!(std::io::stdout().lock(), "{link}")?;
        Ok(None)
    }
}
async fn send_stdin(dir: &Path, args: &SendArgs) -> Result<Value> {
    use tokio::io::AsyncReadExt;
    runtime::call(dir, &Request::Command(Command::Status)).await?;
    let mut reader = tokio::io::stdin();
    let mut buffer = [0; 4096];
    let mut bytes = Vec::new();
    let mut frame = stream::Frame::final_text(String::new(), args.reply_to.clone());
    if let Some(key) = &args.idempotency_key {
        frame.stream_id = crypto::hash(format!("stdin:{}:{key}", args.id));
    }
    frame.seq = 0;
    let mut changed = false;
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            read=reader.read(&mut buffer)=>{
                let n=match read{Ok(n)=>n,Err(e)=>{frame.kind="interrupted".into();frame.reason=Some("stdin_error".into());frame.seq+=1;let _=runtime::call(dir,&Request::Frame{id:args.id.clone(),frame}).await;return Err(e.into())}};
                if n==0{break}
                bytes.extend_from_slice(&buffer[..n]);ensure!(bytes.len()<=stream::MAX_TEXT,"MESSAGE_TOO_LARGE: 消息超过 256 KiB");changed=true;
            }
            _=interval.tick()=>{
                if !changed{continue}
                let valid=match std::str::from_utf8(&bytes){Ok(s)=>s,Err(e) if e.error_len().is_none()=>std::str::from_utf8(&bytes[..e.valid_up_to()])?,Err(e)=>return Err(e.into())};
                if valid.is_empty(){continue}
                frame.text=valid.into();frame.seq+=1;frame.kind="snapshot".into();
                runtime::call(dir,&Request::Frame{id:args.id.clone(),frame:frame.clone()}).await?;changed=false;
            }
            _=tokio::signal::ctrl_c()=>{frame.text=String::from_utf8_lossy(&bytes).into();frame.kind="interrupted".into();frame.reason=Some("cancelled".into());frame.seq+=1;return runtime::call(dir,&Request::Frame{id:args.id.clone(),frame}).await;}
        }
    }
    frame.text = String::from_utf8(bytes)?;
    ensure!(!frame.text.is_empty(), "EMPTY_MESSAGE: 消息不能为空");
    frame.seq += 1;
    frame.kind = "final".into();
    runtime::call(
        dir,
        &Request::Frame {
            id: args.id.clone(),
            frame,
        },
    )
    .await
}
