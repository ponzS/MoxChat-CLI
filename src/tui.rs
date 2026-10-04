use crate::language::Language;
use crate::{
    args::*,
    runtime::{self, Request},
};
use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};
use serde_json::Value;
use std::{path::Path, process::Stdio, time::Duration};

struct Screen;
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}
pub async fn run(dir: &Path, no_ai: bool) -> Result<()> {
    let mut child = None;
    if runtime::call(dir, &Request::Command(Command::Status))
        .await
        .is_err()
    {
        let mut command = tokio::process::Command::new(std::env::current_exe()?);
        command
            .arg("--data-dir")
            .arg(dir)
            .arg("start")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if no_ai {
            command.arg("--no-ai");
        }
        child = Some(command.spawn()?);
        for _ in 0..100 {
            if runtime::call(dir, &Request::Command(Command::Status))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    let result = screen(dir).await;
    if let Some(mut child) = child {
        let _ = runtime::call(dir, &Request::Command(Command::Stop)).await;
        if tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
    result
}
fn safe(s: &str) -> String {
    s.chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect()
}
async fn screen(dir: &Path) -> Result<()> {
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    let _guard = Screen;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut selected = 0;
    let mut list = ListState::default();
    let mut input = String::new();
    let mut lang = Language::load(dir);
    let mut status = match runtime::call(dir, &Request::Command(Command::Status)).await {
        Ok(v) => lang.error(v["aiError"].as_str().unwrap_or("")),
        Err(e) => lang.error(&e.to_string()),
    };
    let (completed_tx, mut completed_rx) = tokio::sync::mpsc::channel::<Result<Value, String>>(16);
    let mut pending = tokio::task::JoinSet::new();
    let mut mode = 0;
    let mut rows: Vec<Value> = vec![];
    let mut messages = String::new();
    let mut page = 1;
    let mut refresh = tokio::time::Instant::now() - Duration::from_secs(2);
    loop {
        let selected_language = Language::load(dir);
        if selected_language != lang {
            lang = selected_language;
            status = lang.text("Language: English", "语言：中文").into();
            refresh -= Duration::from_secs(1);
        }
        while let Ok(message) = completed_rx.try_recv() {
            status = match message {
                Ok(v) => v.to_string(),
                Err(e) => lang.error(&e),
            };
            refresh -= Duration::from_secs(1);
        }
        while pending.try_join_next().is_some() {}
        if refresh.elapsed() > Duration::from_millis(250) {
            let command = match mode {
                1 => Command::Friend {
                    command: FriendCommand::List,
                },
                3 => Command::Group {
                    command: GroupCommand::Invitations,
                },
                2 => Command::Friend {
                    command: FriendCommand::Requests,
                },
                _ => Command::Chat {
                    command: ChatCommand::List,
                },
            };
            match runtime::call(dir, &Request::Command(command)).await {
                Ok(v) => rows = v.as_array().cloned().unwrap_or_default(),
                Err(e) => status = lang.error(&e.to_string()),
            }
            selected = selected.min(rows.len().saturating_sub(1));
            list.select((!rows.is_empty()).then_some(selected));
            if mode == 0 {
                if let Some(id) = rows.get(selected).and_then(|v| v["id"].as_str()) {
                    match runtime::call(
                        dir,
                        &Request::Command(Command::Message {
                            command: MessageCommand::List(HistoryArgs {
                                id: id.into(),
                                page: Some(page),
                                pages: 1,
                                page_size: Some(50),
                                cursor: None,
                            }),
                        }),
                    )
                    .await
                    {
                        Ok(v) => {
                            messages = v["messages"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .rev()
                                .map(|m| {
                                    format!(
                                        "{}\n{}\n",
                                        m["sender"]
                                            .as_str()
                                            .unwrap_or("")
                                            .chars()
                                            .take(14)
                                            .collect::<String>(),
                                        safe(&if m["type"] == "reaction" {
                                            format!(
                                                "{} {} → {}",
                                                lang.text("Reaction", "表情反应"),
                                                m["reaction"]["emoji"].as_str().unwrap_or(""),
                                                m["reaction"]["targetId"].as_str().unwrap_or("")
                                            )
                                        } else {
                                            m["text"].as_str().map(str::to_owned).unwrap_or_else(
                                                || {
                                                    format!(
                                                        "[{}] {}",
                                                        m["type"].as_str().unwrap_or("file"),
                                                        m["attachment"]["fileName"]
                                                            .as_str()
                                                            .unwrap_or(
                                                                lang.text("Attachment", "附件")
                                                            )
                                                    )
                                                },
                                            )
                                        })
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        }
                        Err(e) => status = lang.error(&e.to_string()),
                    }
                } else {
                    messages.clear();
                }
            } else {
                messages = rows
                    .get(selected)
                    .map(|v| safe(&serde_json::to_string_pretty(v).unwrap_or_default()))
                    .unwrap_or_default();
            }
            refresh = tokio::time::Instant::now();
        }
        terminal.draw(|f| {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(4),
                    Constraint::Length(3),
                    Constraint::Length(2),
                ])
                .split(f.area());
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(28), Constraint::Percentage(72)])
                .split(layout[0]);
            let items = rows
                .iter()
                .map(|r| {
                    ListItem::new(safe(
                        r["name"]
                            .as_str()
                            .or(r["peer"].as_str())
                            .or(r["pub"].as_str())
                            .or(r["id"].as_str())
                            .unwrap_or(""),
                    ))
                })
                .collect::<Vec<_>>();
            f.render_stateful_widget(
                List::new(items)
                    .block(Block::default().borders(Borders::ALL).title(match mode {
                        1 => lang.text("Friends", "好友"),
                        2 => lang.text("Friend requests", "好友申请"),
                        3 => lang.text("Group invitations", "群邀请"),
                        _ => lang.text("Conversations", "会话"),
                    }))
                    .highlight_style(Style::default().fg(Color::Cyan)),
                panes[0],
                &mut list,
            );
            let mut paragraph = Paragraph::new(messages.as_str())
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(if page == 1 {
                    lang.text("Messages", "消息")
                } else {
                    lang.text("History", "历史消息")
                }));
            if mode == 0 {
                let width = panes[1].width.saturating_sub(2).max(1) as usize;
                let lines: usize = messages
                    .lines()
                    .map(|l| l.chars().count().div_ceil(width).max(1))
                    .sum();
                paragraph = paragraph.scroll((
                    lines
                        .saturating_sub(panes[1].height.saturating_sub(2) as usize)
                        .min(u16::MAX as usize) as u16,
                    0,
                ));
            }
            f.render_widget(paragraph, panes[1]);
            f.render_widget(
                Paragraph::new(input.as_str()).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(lang.text("Enter Send · /friend add <key> · /help", "Enter 发送 · /friend add <公钥> · /help")),
                ),
                layout[1],
            );
            f.render_widget(
                Paragraph::new(format!(
                    "{}\n{}",
                    lang.text("F1 Chats  F2 Friends  F3 Requests  F4 Invites  ↑↓ Select  PgUp/PgDn History  Ctrl-C Quit", "F1 会话  F2 好友  F3 申请  F4 群邀请  ↑↓ 选择  PgUp/PgDn 历史  Ctrl-C 退出"),
                    safe(&status)
                )),
                layout[2],
            );
        })?;
        if !event::poll(Duration::from_millis(33))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            KeyCode::F(n @ 1..=4) => {
                mode = n - 1;
                page = 1;
                refresh -= Duration::from_secs(1)
            }
            KeyCode::Up => {
                selected = selected.saturating_sub(1);
                page = 1;
                refresh -= Duration::from_secs(1)
            }
            KeyCode::Down => {
                selected = (selected + 1).min(rows.len().saturating_sub(1));
                page = 1;
                refresh -= Duration::from_secs(1)
            }
            KeyCode::PageUp => {
                page += 1;
                refresh -= Duration::from_secs(1)
            }
            KeyCode::PageDown => {
                page = page.saturating_sub(1).max(1);
                refresh -= Duration::from_secs(1)
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) => input.push(c),
            KeyCode::Enter => {
                let text = std::mem::take(&mut input);
                let command = if let Some(command) = text.strip_prefix('/') {
                    match shell_words::split(command)
                        .map_err(|e| e.to_string())
                        .and_then(|words| {
                            Cli::try_parse_from(std::iter::once("mox".to_owned()).chain(words))
                                .map_err(|e| e.to_string())
                        }) {
                        Ok(cli) => Some(cli.command),
                        Err(e) => {
                            status = lang.error(&e);
                            None
                        }
                    }
                } else if mode == 0 && !text.is_empty() {
                    rows.get(selected)
                        .and_then(|r| r["id"].as_str())
                        .map(|id| Command::Message {
                            command: MessageCommand::Send(SendArgs {
                                id: id.into(),
                                text: Some(text),
                                text_stdin: false,
                                img: None,
                                video: None,
                                file: None,
                                reply_to: None,
                                idempotency_key: None,
                            }),
                        })
                } else {
                    None
                };
                if let Some(command) = command {
                    if matches!(command, Command::Help { .. }) {
                        status=lang.text("/lang en|cn; /friend add <key>; /friend accept <request>; /message send <chat> --img <path>; /message react <chat> <message> 👍", "/lang en|cn；/friend add <公钥>；/friend accept <申请ID>；/message send <会话ID> --img <路径>；/message react <会话ID> <消息ID> 👍").into()
                    } else if let Command::Lang { language } = command {
                        if let Some(value) = language {
                            value.save(dir)?;
                        }
                        lang = Language::load(dir);
                        status = lang.text("Language: English", "语言：中文").into();
                    } else {
                        let dir = dir.to_owned();
                        let tx = completed_tx.clone();
                        pending.spawn(async move {
                            let result = async {
                                let mut command = command;
                                if let Command::Message {
                                    command: MessageCommand::Send(ref mut args),
                                } = command
                                {
                                    for path in [&mut args.img, &mut args.video, &mut args.file]
                                        .into_iter()
                                        .flatten()
                                    {
                                        *path = crate::files::check_path(path)?;
                                    }
                                }
                                runtime::call(&dir, &Request::Command(command)).await
                            }
                            .await;
                            let _ = tx.send(result.map_err(|e| e.to_string())).await;
                        });
                        status = lang.text("Working…", "正在处理…").into();
                    }
                }
                refresh -= Duration::from_secs(1);
            }
            _ => {}
        }
    }
    pending.abort_all();
    while pending.join_next().await.is_some() {}
    Ok(())
}
