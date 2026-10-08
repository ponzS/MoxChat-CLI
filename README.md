# moxchat-cli

[中文文档](./README.zh-CN.md)

A Rust CLI and TUI for MoxChat, available as `mox`. The current version is `0.1.1`. It supports independent identities, friends, encrypted direct messages, Group V2, attachments, and automatic replies through a local Codex installation. Each conversation has its own Codex thread and uses the computer's default Codex model configuration.

New deliveries on the development branch use `p256-sha256-ciphertext-v1`, with plaintext signatures inside encryption. Upgrade clients and servers together. Historical envelopes remain readable; old pending sends move to `failed_delivery` and emit a rejection event, requiring an explicit resend instead of rewriting retries. Updated binaries have not been released.

## MoxChat Clients

- iOS: [Download on the App Store](https://apps.apple.com/us/app/moxchat/id6775016915)
- Web: [Open MoxChat](https://app.ponzs.com)

## Install

Download and install the binary from [ponzS/MoxChat-CLI](https://github.com/ponzS/MoxChat-CLI). No Rust, compiler or build tools are needed.

### macOS

Apple Silicon and Intel:

```sh
curl -fsSL https://raw.githubusercontent.com/ponzS/MoxChat-CLI/main/scripts/mox-cli-install.sh | sh
```

### Linux

x86_64 and ARM64, Ubuntu/Debian or another glibc-based distribution (glibc 2.28+):

```sh
curl -fsSL https://raw.githubusercontent.com/ponzS/MoxChat-CLI/main/scripts/mox-cli-install.sh | sh
```

### Windows (WSL2)

With WSL2 and Ubuntu set up, run in PowerShell:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "irm https://raw.githubusercontent.com/ponzS/MoxChat-CLI/main/scripts/mox-cli-install.ps1 | iex"
```

If WSL2 is not set up, first run `wsl --install -d Ubuntu` in administrator PowerShell, restart if prompted, and create your Ubuntu user. The installer downloads the Linux binary into WSL2; no compilation is required. Run Mox and Codex in that Ubuntu terminal. A native Windows executable is not provided.

The installer verifies SHA-256 and installs to `~/.local/bin`. Reopen your terminal afterwards. You can also download the archive for your platform from [Releases](https://github.com/ponzS/MoxChat-CLI/releases/latest).

## Start and Add a Friend

Install and sign in to Codex on your computer first. Mox uses its default model configuration.

```sh
curl -fsSL https://chatgpt.com/codex/install.sh | sh
codex login
mox login                 # Enter a nickname and press Enter
mox start                 # Keep this terminal running
```

Open another terminal:

```sh
mox qr                    # Show a QR code
# Or copy the complete identity link:
mox moxpub
```

In the graphical MoxChat client, scan the terminal QR code on your phone, or paste the complete `moxpub:` link into Add Friend and search. Confirm the nickname and send a friend request. Keep `mox start` running so the profile and requests are available.

Accept the request from another terminal on your computer:

```sh
mox friend requests
mox friend accept <request-id>
```

You can also run `mox tui` and open Friend Requests. The CLI identity and graphical client identity are separate. Once accepted, open that contact's direct chat in the graphical client and send a message; Codex replies will appear there.

`start` and `tui` also prompt for a nickname when no identity exists. For non-interactive use, run `mox login --name "Nickname"`. `Ctrl-C` and `mox stop` preserve the identity. **`mox logout` stops the identity's runtime and permanently deletes its local keys, messages, relationships, attachment cache, and Codex working directory.** It does not delete your own Codex login configuration or other sessions, and it cannot retract content already received remotely.

`login` creates or reuses a local identity. `start` and `tui` connect to the relay, publish profile details such as the nickname, and keep receiving messages. Foreground `start` logs profile publication, friend requests, incoming messages, time to the first Codex output, generation completion, and relay acceptance. Logs do not print chat bodies or keys. Relay acceptance means the delivery entry point has accepted a message; it is not a read receipt.

The identity directory uses permissions `0700`, and the system keyring stores the SQLCipher master key. Without a desktop keyring, set `MOX_MASTER_KEY` to a fixed 32-byte hexadecimal key. You must keep that key safe and supply the same value on subsequent starts. Use `--data-dir <dedicated-empty-directory>` or `MOX_DATA_DIR` to isolate identities.

## Commands

```sh
mox version
mox whoami --json
mox qr                     # Show your QR code in the terminal for mobile friend requests
mox moxpub                 # Print only your identity link for copying or scripting
mox status --json
mox lang                   # Show the current terminal language
mox lang cn                # Switch to Chinese
mox lang en                # Switch to English (default)
mox help message send
mox help message list --json

mox friend add <full-public-key>
mox friend requests
mox friend accept <request-id>
mox friend list
mox chat list

mox message send <conversation-id> "Hello"
mox message send <conversation-id> --img ./photo.png
mox message send <conversation-id> --video ./clip.mp4
mox message send <conversation-id> --file ./report.pdf
mox message react <conversation-id> <message-id> "👍"
printf '# Heading\nBody\n' | mox message send <conversation-id> --text-stdin
mox message list <conversation-id> --page 1 --pages 3 --page-size 50 --json
mox message list <conversation-id> --cursor <next_cursor> --pages 2 --json
mox events --after 0 --json

mox group create --name "Discussion"
mox group invite <group-id> <friend-public-key>
mox group invitations
mox group join <invitation-id>
mox group requests <group-id>
mox group approve <group-id> <invitation-id>
mox group members <group-id>
mox group leave <group-id>
```

Pass the conversation ID as a positional argument. Direct message IDs have the form `dm:<public-key>`; use `chat list` to find group IDs. History is returned from newest to oldest. `--page` defaults to 1; `--pages` defaults to 1 and accepts up to 20; `--page-size` defaults to 50 and accepts up to 200. `next_cursor` preserves the traversal boundary and cannot be combined with `--page`. When continuing with a cursor, omitting the page size reuses the cursor's value. A streamed reply occupies one history entry.

Attachments follow the graphical client’s flow: validate the local file, encrypt it, upload the ciphertext to the file relay, confirm the full ciphertext length, then send an image/video/file template containing the relay download URL and encryption descriptor inside an encrypted chat message. File transfers use a separate HTTP client with HTTP/2 negotiation, 60-second request deadlines, 1 MiB chunks and up to four retries for transient failures. Retries query the relay’s confirmed offset and reuse the existing ciphertext and upload session; a lost TUS acknowledgement is recovered within the existing session. Completion is polled before publishing the attachment. Repeated AI calls for an unchanged image in the same turn reuse the prepared upload. No attachment message is sent when upload confirmation fails. A missing path returns `FILE_NOT_FOUND`, indicating that the file does not exist. `--idempotency-key <key>` protects command retries; the same key cannot be reused with different content. If retrying a stdin stream changes the content assigned to an existing sequence number, the command reports a conflict. The runtime retries already prepared transport frames automatically. A `queued` result means the message is durably queued locally; use `events` to observe relay acceptance.

TUI shortcuts: `F1` conversations, `F2` friends, `F3` friend requests, and `F4` group invitations. Use the arrow keys to select, `PgUp/PgDn` to browse history, and Enter to send. Enter management commands such as `/friend add <public-key>` or `/group create --name <name>`. Attachment uploads do not block input. The terminal displays text and attachment details but does not play media.

The TUI defaults to English. `mox lang cn` switches it to Chinese and `mox lang en` switches it back; `cn` is the Chinese language code (`zh` is not accepted). You can also enter `/lang cn` or `/lang en` inside the TUI. The setting is saved per data directory, works before login, and updates an open TUI automatically. Logout deletes this setting along with the identity. Login prompts and foreground runtime logs follow the same language setting; future log lines switch without restarting the service. It changes the terminal interface language, not chat content or the language of AI replies; CLI command names and JSON field names stay stable.

`message react` sends a native message reaction, such as 👍 or ❤️, using a message ID from `message list`. Plain emoji text also works with `message send`. Reaction events appear as `type: "reaction"` in CLI history and do not trigger another AI reply. Reactions to older streamed messages without a recorded transport ID return `REACTION_TARGET_UNAVAILABLE`.

## QR Codes and Identity Links

`mox qr` displays a black-and-white QR code with a copyable `moxpub:` link underneath. Scanning it in MoxChat on a phone opens the friend request flow. `mox moxpub` prints only the same link followed by a newline. Its format matches the copy action below “My QR Code” in the graphical client: standard Base64 encoding of `{"pub":"full-public-key","relays":["communication-relay-url"]}`.

The link contains the current identity's public key and selected communication relay. It does not contain private keys, file relays, or unselected relay candidates. After switching communication relays, run the command again to get an updated link; the identity's public key stays the same. Both commands work offline and do not require Codex. Without an identity, they prompt you to run `mox login`. A new identity must run `mox start` to publish its profile and receive friend requests.

Both commands support `--json`, returning `pub`, `relays`, and `moxpub`. `mox qr --json` also returns `qr` text without ANSI control characters and its `qr_columns` width. The terminal QR code uses fixed black-and-white colors and retains its quiet zone. If the terminal is too narrow, the command asks you to widen it so wrapping does not corrupt the code.

## Relay Management

Communication and file relay settings are persisted separately. Their defaults are `https://mox.ponzs.com` and `https://file.ponzs.com`, respectively.

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

Both command groups support `list/show/add/use/set/remove`. `add` adds a candidate, `use` switches to an existing candidate, and `set` adds and selects an address. The selected address cannot be removed. Switching the communication relay while the runtime is running verifies the server identity and republishes the route; failure restores the previous configuration. Offline changes take effect on the next start. Existing attachments and upload tasks retain the file relay selected when they were created. The current implementation uses one selected communication entry point and does not automatically switch to another service.

## Codex and Streaming Replies

The client finds `codex` on `PATH`, or uses the path in `MOX_CODEX_BIN`. It communicates through the stdio interface of `codex app-server` without overriding the model, provider, or reasoning effort. Codex manages its own installation, login, and default configuration.

```sh
mox start --no-ai
mox ai status
mox ai pause
mox ai resume
mox ai pause <conversation-id>
mox ai resume <conversation-id>
mox ai workspace <conversation-id> --json
```

If Codex is unavailable, manual chat remains available and the client prompts you to install Codex or check its login. Automatic replies use a separate working directory for each conversation with a `workspace-write` sandbox and do not approve interactive tool permissions. Messages are processed sequentially within each conversation, with up to four conversations generating replies concurrently. Complete new group messages also trigger replies. After a restart, context is rebuilt from encrypted history. Interrupted generation is marked for recovery; `ai resume` explicitly retries it instead of generating duplicate replies automatically.

Codex has two conversation-bound actions: `mox_send_image` encrypts and uploads a real image file, and `mox_react` adds a native emoji reaction to a message. For example, ask it to “add 👍 to this message, create a small blue PNG, and send it to me.” Local rendering can produce diagrams and simple graphics; other image generation depends on the tools available in your Codex installation. Mox does not add a separate image-generation provider or API key.

To let AI send an existing local image, use `mox ai workspace <conversation-id> --json`, copy the image into the returned directory, then ask it to send that filename. AI image paths must resolve inside that conversation's directory, including after resolving symlinks. Manual `mox message send <conversation-id> --img <path>` accepts an explicitly supplied path elsewhere. Both routes use the same attachment validation, encryption and the file relay upload. A Markdown file link alone does not send an image. Successful image or reaction actions need no extra text reply and create no empty text message. Each AI turn allows up to 16 Mox actions; a failed action is reported to Codex rather than claimed as sent. Image uploads have a five-minute action limit. `mox ai status --json` includes the latest action errors, and foreground logs show the upload stage and concrete failure cause.

Reply bodies use Markdown by default, with the streaming template inside the end-to-end encrypted message. Deltas are combined about every 250 ms, with up to 4 KiB per delta. A full snapshot replaces a delta about every two seconds, and the complete terminal frame is submitted immediately when generation ends. When delivery frames accumulate, the client delays preparing new frames and combines pending text. The graphical client updates one message bubble and supports headings, lists, quotes, code blocks, tables, and links. It does not execute HTML or automatically fetch external images embedded in Markdown.

The AI text stream starts when the first reply text arrives. Waiting for Codex or sending only an image or reaction creates no empty text bubble. The Web client shows no waiting spinner. Conversation previews retain the previous text until reply content arrives, then follow the streamed reply without increasing the unread count for each batch. Accepted friends' verified communication public keys are used to prepare ciphertext, avoiding a profile lookup before every batch. `mox ai status --json` shows queued and failed counts, each active conversation's preparation, waiting, or streaming stage, and timing details. Time to the first output still depends on the local Codex model and reasoning settings; `mox` does not override them.

The UI progressively reveals newly decrypted text at up to about 30 updates per second, catching up with each received update within about 300 ms, including the final batch of a live reply. This adds no encryption or decryption operations. Completed history, text corrections, interrupted replies, and reduced-motion mode display received content immediately. Copying uses the complete received text. Update the recipient's website or app to get this presentation behavior; replacing `mox` alone does not update the website.

The receiving MoxChat client and the communication relay must both support the streaming changes. Subsequent frames use signed silent message IDs so the server delivers ciphertext and events without repeating push notifications. Older clients may display the raw template. A stream's text is limited to 256 KiB; group replies use a smaller limit imposed by the MLS envelope budget. Exceeding the limit marks the reply as interrupted. Group membership changes stop the existing stream to avoid sending earlier content to newly added members.

## Updates and Releases

```sh
mox update --check
mox update
```

The default upstream is GitHub Releases for `ponzS/MoxChat-CLI`. Distributors can set it at build time with `MOX_DEFAULT_UPDATE_REPO`; users can override it with `MOX_UPDATE_REPO=owner/repo`. For a private upstream, `MOX_GITHUB_TOKEN` supplies credentials to read releases. Only stable `mox-v<semver>` tags are used; drafts and prereleases are ignored. The updater downloads `mox-<Rust target>.tar.gz`, verifies `SHA256SUMS` and the GitHub asset digest when available, and atomically replaces the executable in its installation directory. If runtime recovery fails, it rolls back the executable and attempts to restore the previous service. The installation directory must be writable.

Updates preserve the identity, relays, and history, and restore a previously running service with its AI setting. If the service was not running, only the executable is updated. The TUI can reconnect to the new runtime. If the upstream has no CLI release, the command returns `UPDATE_NOT_PUBLISHED`; it does not install releases for other Mox products.

Distributors can package a release on each target platform from the repository root:

```sh
scripts/mox-cli-build.sh --out /absolute/path/to/release-directory
```

Upload the platform artifacts and their combined SHA-256 entries to the same `mox-v0.1.1` release. The script only builds and packages; it does not create tags or publish a remote release.

## Manual Verification

Start `mox start`, then run `mox qr` in another terminal and scan it with MoxChat on a phone. Alternatively, paste the output of `mox moxpub` into the add-friend page. Both entry points should identify the same account. After adding each other as friends, send direct and group messages and confirm that Codex replies update one bubble in the original conversation. Send all three attachment types and check their contents. Stopping and restarting should reuse the identity; logging out should return to nickname creation.

## Build from Source (Developers)

With Rust and platform build tools installed: `git clone https://github.com/ponzS/MoxChat-CLI.git`, `cd MoxChat-CLI`, then `cargo install --path . --locked`. This is optional; normal installation uses the binaries above.
