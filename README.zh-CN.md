# moxchat-cli

[English](./README.md)

Rust 编写的 MoxChat CLI / TUI，程序名 `mox`，当前版本 `0.1.1`。支持独立身份、好友、加密私聊、Group V2、附件和本机 Codex 自动回复。每个聊天拥有独立 Codex thread，采用电脑上 Codex 的默认模型配置。

开发分支的新投递使用 `p256-sha256-ciphertext-v1` 密文签名，明文签名留在加密内容中。客户端和服务端需配套升级；历史对象保持原样读取，旧格式待发送任务转入 `failed_delivery` 并发出拒绝事件，需要用户重新发送，不在重试时改写。此变更尚未发布二进制。

## MoxChat 客户端

- iOS：[在 App Store 下载](https://apps.apple.com/us/app/moxchat/id6775016915)
- Web：[打开 MoxChat 网页版](https://app.ponzs.com)

## 安装

从 [ponzS/MoxChat-CLI](https://github.com/ponzS/MoxChat-CLI) 下载并安装二进制，**不需要安装 Rust、编译器或构建环境**。

### macOS

支持 Apple Silicon 和 Intel：

```sh
curl -fsSL https://raw.githubusercontent.com/ponzS/MoxChat-CLI/main/scripts/mox-cli-install.sh | sh
```

### Linux

支持 x86_64 和 ARM64，适用于 Ubuntu / Debian 等 glibc 2.28 及以上的发行版：

```sh
curl -fsSL https://raw.githubusercontent.com/ponzS/MoxChat-CLI/main/scripts/mox-cli-install.sh | sh
```

### Windows（WSL2）

已启用 WSL2 并设置好 Ubuntu 后，在 PowerShell 中执行：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "irm https://raw.githubusercontent.com/ponzS/MoxChat-CLI/main/scripts/mox-cli-install.ps1 | iex"
```

尚未启用 WSL2 时，先在管理员 PowerShell 执行 `wsl --install -d Ubuntu`，按提示重启并创建 Ubuntu 用户。安装器会下载 Linux 二进制到 WSL2，无需编译；随后在 Ubuntu 终端运行 Mox 和 Codex。目前不提供原生 Windows 可执行程序。

安装器校验 SHA-256 后安装到 `~/.local/bin`，完成后重新打开终端。也可以从 [Releases](https://github.com/ponzS/MoxChat-CLI/releases/latest) 下载对应平台安装包。

## 启动并添加好友

先在电脑上安装并登录 Codex，Mox 会使用其默认模型配置。

```sh
curl -fsSL https://chatgpt.com/codex/install.sh | sh
codex login
mox login                 # 输入昵称，回车创建身份
mox start                 # 保持这个终端运行
```

另开一个终端：

```sh
mox qr                    # 显示二维码
# 或复制完整的身份链接：
mox moxpub
```

在手机 MoxChat 中打开扫一扫，扫描终端二维码；也可以在当前图形客户端的添加好友页面粘贴完整 `moxpub:` 链接并搜索。确认昵称后发送好友申请。电脑的 `mox start` 需要保持运行，才能发布昵称和接收申请。

在电脑另一个终端接受申请：

```sh
mox friend requests
mox friend accept <request-id>
```

把 `<request-id>` 替换成申请列表中的 ID。也可以运行 `mox tui`，在好友申请页面接受。CLI 与图形客户端使用相互独立的身份。接受后，在图形客户端打开与该联系人的私聊并发消息，即可收到 Codex 的回复。

`start` / `tui` 在没有身份时也会询问昵称。无交互环境用 `mox login --name "昵称"`。`Ctrl-C` 或 `mox stop` 保留身份；**`mox logout` 停止本身份运行时并永久删除其本地密钥、消息、关系、附件缓存和 Codex 工作目录**。不会删除用户自己的 Codex 登录配置或其他会话，也不能撤回远端已收到的内容。

`login` 创建或复用本地身份；`start` / `tui` 才连接中继、发布昵称等资料并持续收信。前台 `start` 会打印资料同步、好友申请、收到消息、Codex 首批正文耗时、生成完成和中继接收结果；日志不打印聊天正文或密钥。中继接收代表投递入口已接收，不能当成对方已读。

身份目录权限为 `0700`，系统密钥环保存 SQLCipher 主密钥。无桌面密钥环时可通过 `MOX_MASTER_KEY` 注入固定的 32 字节十六进制密钥，由调用方保管，后续启动必须提供相同值。`--data-dir <专用空目录>` 或 `MOX_DATA_DIR` 用于隔离身份。

## 命令

```sh
mox version
mox whoami --json
mox qr                     # 终端显示自己的二维码，供手机扫码加好友
mox moxpub                 # 只输出身份链接，便于复制或传给脚本
mox status --json
mox lang                   # 查看当前终端语言
mox lang cn                # 切换中文
mox lang en                # 切换英文（默认）
mox help message send
mox help message list --json

mox friend add <完整公钥>
mox friend requests
mox friend accept <申请ID>
mox friend list
mox chat list

mox message send <会话ID> "你好"
mox message send <会话ID> --img ./photo.png
mox message send <会话ID> --video ./clip.mp4
mox message send <会话ID> --file ./report.pdf
mox message react <会话ID> <消息ID> "👍"
printf '# 标题\n正文\n' | mox message send <会话ID> --text-stdin
mox message list <会话ID> --page 1 --pages 3 --page-size 50 --json
mox message list <会话ID> --cursor <next_cursor> --pages 2 --json
mox events --after 0 --json

mox group create --name "讨论组"
mox group invite <群ID> <好友公钥>
mox group invitations
mox group join <邀请ID>
mox group requests <群ID>
mox group approve <群ID> <邀请ID>
mox group members <群ID>
mox group leave <群ID>
```

会话 ID 直接作为位置参数：私聊形如 `dm:<公钥>`，群 ID 从 `chat list` 获取。历史按新到旧返回；`--page` 默认 1，`--pages` 默认 1、最多 20，`--page-size` 默认 50、最多 200。`next_cursor` 固定遍历边界，不能同时传 `--page`；续读省略每页数量时沿用游标。流式回复只占一条历史。

附件遵循图形客户端相同流程：校验本地文件、加密、上传密文到 文件中继、确认完整密文长度，再通过加密聊天消息发送包含中继下载地址与加密描述的图片/视频/文件模板。文件传输使用独立 HTTP 客户端，支持协商 HTTP/2，单请求超时 60 秒、每块 1 MiB，临时错误最多重试 4 次。重试查询中继确认的偏移，复用已有密文和上传会话；TUS 响应丢失时在已有会话内恢复。确认完成状态后才发布附件；同一轮 AI 重试未改变的图片会复用准备结果，上传未确认时不会发出附件消息。不存在的路径返回 `FILE_NOT_FOUND: 文件不存在`。`--idempotency-key <键>` 可保护命令重试，同一键不能改换内容；流式 stdin 重试若重新分批产生不同的同序号内容，会明确返回冲突，已有传输帧由运行时自动重试。返回 `queued` 表示本地持久排队，中继接收结果通过 `events` 查看。

TUI：`F1` 会话，`F2` 好友，`F3` 好友申请，`F4` 群邀请；上下键选择，`PgUp/PgDn` 翻历史，回车发送。输入 `/friend add <公钥>`、`/group create --name <名称>` 等管理命令。附件上传不会阻塞输入；终端显示文字与附件信息，不播放媒体。

TUI 默认英文。执行 `mox lang cn` 切换中文，`mox lang en` 切回英文；中文代码使用 `cn`，不接受 `zh`。TUI 内也可输入 `/lang cn` 或 `/lang en`。设置按数据目录保存，登录前即可使用，已打开的 TUI 会自动更新；退出登录会随身份一起删除语言设置。登录提示与前台运行日志也遵循该设置，运行中的服务无需重启即可切换后续日志语言。它只控制终端界面，不改变聊天内容或 AI 回复语言；CLI 命令名与 JSON 字段名保持稳定。

`message react` 使用 `message list` 返回的消息 ID，发送 👍、❤️ 等原生消息表情反应。普通 emoji 文字仍可通过 `message send` 发送。反应在 CLI 历史中显示为 `type: "reaction"`，收到反应不会再次触发 AI 回复。旧版流式历史若没有保存传输消息 ID，添加反应会返回 `REACTION_TARGET_UNAVAILABLE`。

## 二维码与身份链接

`mox qr` 显示黑白二维码，下方附上可复制的 `moxpub:` 链接；在手机 MoxChat 中扫码即可进入添加好友流程。`mox moxpub` 只输出同一链接和换行，格式与图形客户端“我的二维码”下的复制结果一致：标准 Base64 编码的 `{"pub":"完整公钥","relays":["通信中继地址"]}`。

链接包含当前身份的公钥和当前选中的通信中继，不包含私钥、文件中继或未选中的候选地址。切换通信中继后重新运行命令即可得到新链接，身份公钥保持不变。两个命令都支持离线读取，不依赖 Codex；没有身份时提示先执行 `mox login`。新建身份需运行 `mox start` 发布资料并接收好友申请。

两者均支持 `--json`，返回 `pub`、`relays` 和 `moxpub`；`mox qr --json` 额外返回无 ANSI 控制字符的 `qr` 文本与 `qr_columns` 宽度。终端二维码保留空白边框并固定黑白颜色；终端过窄会提示加宽，避免换行破坏二维码。

## 中继管理

通信与文件配置独立持久化，默认分别为 `https://mox.ponzs.com` 与 `https://file.ponzs.com`。

```sh
mox relay show
mox relay list
mox relay add https://relay.example.com
mox relay use https://relay.example.com
mox relay set https://mox.ponzs.com
mox relay remove https://relay.example.com
mox file-relay show
mox file-relay list
mox file-relay set https://files.example.com
mox file-relay use https://file.ponzs.com
```

两组均支持 `list/show/add/use/set/remove`：`add` 只添加，`use` 切换到已有地址，`set` 添加并切换。当前选中地址不能移除。运行时切换通信入口会验证服务身份、重新发布路由，失败时恢复原配置；离线设置在下次启动连接。已有附件与上传任务继续使用创建时的文件中继。当前实现使用一个选中的通信入口，不自动切换其他服务。

## Codex 与流式回复

从 `PATH` 查找 `codex`，或通过 `MOX_CODEX_BIN` 指定路径。使用 `codex app-server` 的 stdio 接口，不传模型、provider 或 reasoning effort 覆盖项。Codex 安装、登录和默认配置由 Codex 自己管理。

```sh
mox start --no-ai
mox ai status
mox ai pause
mox ai resume
mox ai pause <会话ID>
mox ai resume <会话ID>
mox ai workspace <会话ID> --json
```

Codex 不可用时保留手动聊天并提示安装或检查登录。自动回复为每个会话分配独立工作目录，采用 `workspace-write` 沙箱，不批准交互式工具权限。每个聊天顺序处理，最多 4 个聊天同时生成；群内完整新消息也触发回复。重启后从加密历史重建上下文；意外终止的生成标记为待恢复，`ai resume` 明确重试，不盲目重复生成。

Codex 可调用两项绑定当前会话的操作：`mox_send_image` 加密上传真实图片文件，`mox_react` 给指定消息添加原生表情反应。例如可发消息：“给本条消息加一个 👍，创建一张蓝色小 PNG 并发给我。”本地绘制可生成图表和简单图形；其他图片生成能力取决于本机 Codex 已有工具，Mox 不额外接入图片生成服务或要求 API Key。

让 AI 发送已有本地图片时，先执行 `mox ai workspace <会话ID> --json`，把图片复制到返回的目录，再让 AI 发送该文件。AI 图片路径解析后必须位于当前会话工作目录内，符号链接也不能越界。手动 `mox message send <会话ID> --img <路径>` 可使用明确指定的其他路径；两者共用附件校验、加密和 文件中继 上传。仅回复 Markdown 文件链接不会发送图片。成功发送图片或反应后可以不再附加文字，不会生成空气泡。每轮 AI 最多执行 16 次 Mox 操作；图片上传操作总时限为 5 分钟。失败会反馈给 Codex，不会伪装成发送成功；`mox ai status --json` 可查看最近的操作错误，前台日志显示上传阶段与具体失败原因。

正文默认 Markdown，流式模板位于端到端加密消息内部。增量约 250 ms 合并一次、每次最多 4 KiB，每约 2 秒补完整快照，结束立即提交完整终态。待投递帧积压时暂缓封装新帧并合并文本。图形客户端更新同一气泡，支持标题、列表、引用、代码块、表格和链接，不执行 HTML 或自动请求 Markdown 外链图片。

AI 文字流在首批正文到达后才开始。等待 Codex 或仅发送图片、反应时不创建空文字气泡；Web 不显示等待转圈。会话列表在正文到达前保留原预览，之后随回复内容更新，未读数不按批次重复增加。已接受好友使用已验证的通信公钥准备密文，避免每个批次先查询一次资料。`mox ai status --json` 显示排队数、失败数与当前会话的准备、等待正文、流式发送阶段及耗时。首批正文仍取决于本机 Codex 默认模型与推理设置，这些设置不会被 `mox` 覆盖。

界面以最多约 30 次/秒展开已解密的新增文字，每次更新最多约 300 ms 追上收到的正文，包括实时回复的最后一批；这不增加加解密次数。已完成历史、正文修正、中断和减少动态效果模式直接展示收到的内容；复制使用完整正文。需更新接收方网页/应用才能看到新的展示效果，只替换 `mox` 不会更新网页。

需要同时升级接收方 MoxChat 和 通信中继：后续帧使用已签名的静默消息 ID，服务端投递密文和事件，但不重复推送。旧客户端可能显示模板原文。单流正文上限 256 KiB；群回复还受 MLS 信封限制，采用更小上限，超限时标记中断。群成员变化时停止原流，避免向新成员补发此前正文。

## 版本更新与发布

```sh
mox update --check
mox update
```

默认上游 `ponzS/MoxChat-CLI` GitHub Releases；发行方可用构建变量 `MOX_DEFAULT_UPDATE_REPO` 设置，用户可用 `MOX_UPDATE_REPO=owner/repo` 覆盖。私有上游可使用 `MOX_GITHUB_TOKEN` 提供 Release 读取凭据。只采用 `mox-v<semver>` 稳定标签，忽略草稿和预发布。下载 `mox-<Rust target>.tar.gz`，核对 `SHA256SUMS` 和可用的 GitHub asset digest，在原安装目录原子替换；恢复失败回滚程序并尝试恢复旧服务。安装目录须可写。

更新保留身份、中继与历史，恢复原先运行服务及其 AI 开关；原本未运行时只更新程序。TUI 可重新连接新运行时。上游尚无 CLI Release 时返回 `UPDATE_NOT_PUBLISHED`，不会安装其他 Mox 产品的版本。

发行者在对应平台，从仓库根目录打包：

```sh
scripts/mox-cli-build.sh --out /绝对路径/发布目录
```

多平台产物和各自 SHA-256 行汇总后上传到同一 `mox-v0.1.1` Release。脚本只构建打包，不创建标签或发布远端 Release。

## 手工验证

启动 `mox start`，在另一终端执行 `mox qr`，用手机 MoxChat 扫码；也可把 `mox moxpub` 输出粘贴到添加好友页，两个入口应识别同一身份。互加好友后，分别发送私聊与群消息，确认 Codex 回复在原会话同一气泡持续更新；发送三类附件并核对内容；停止重启应复用身份，退出登录后应进入新昵称流程。

## 从源码构建（开发者）

已安装 Rust 和系统构建工具时，执行 `git clone https://github.com/ponzS/MoxChat-CLI.git`、`cd MoxChat-CLI`，再运行 `cargo install --path . --locked`。这仅供开发使用，普通安装直接使用上面的二进制命令。
