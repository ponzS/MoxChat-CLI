use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "mox",
    version,
    about = "Encrypted MoxChat terminal client",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        help = "Identity directory (defaults to the system app data directory)"
    )]
    pub data_dir: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        help = "Print machine-readable JSON; events uses JSONL"
    )]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum Command {
    /// Show or change the terminal language (en/cn; default en)
    Lang {
        #[arg(value_enum)]
        language: Option<crate::language::Language>,
    },
    /// Show the version without logging in or connecting
    Version,
    /// Update from GitHub and restore a previously running service
    Update {
        #[arg(long, help = "Check for updates only")]
        check: bool,
    },
    /// Show command help, e.g. mox help message send
    Help { command: Vec<String> },
    /// Create or reuse an identity without starting the network service
    Login {
        #[arg(long)]
        name: Option<String>,
    },
    /// Run networking and AI in the foreground; Ctrl-C preserves the identity
    Start {
        #[arg(long)]
        no_ai: bool,
    },
    /// Stop the runtime and preserve the identity
    Stop,
    /// Open the terminal interface for chats and friends
    Tui {
        #[arg(long)]
        no_ai: bool,
    },
    /// Show the current public identity
    Whoami,
    /// Show your QR code for adding friends from mobile MoxChat
    Qr,
    /// Print your moxpub link with the public key and selected relay
    Moxpub,
    /// Show runtime status
    Status,
    /// Destroy the current identity and all its local data
    Logout,
    /// Edit your public profile
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Friends and friend requests
    Friend {
        #[command(subcommand)]
        command: FriendCommand,
    },
    /// List conversations
    Chat {
        #[command(subcommand)]
        command: ChatCommand,
    },
    /// Send messages and read history
    Message {
        #[command(subcommand)]
        command: MessageCommand,
    },
    /// Manage groups
    Group {
        #[command(subcommand)]
        command: GroupCommand,
    },
    /// Manage communication relays
    Relay {
        #[command(subcommand)]
        command: RelayCommand,
    },
    /// Manage file relays (defaults to the official relay)
    FileRelay {
        #[command(subcommand)]
        command: RelayCommand,
    },
    /// AI reply status and controls
    Ai {
        #[command(subcommand)]
        command: AiCommand,
    },
    /// Read events continuously as JSONL; resume with --after
    Events {
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
}
#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum ProfileCommand {
    Set {
        #[arg(long)]
        name: String,
    },
}
#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum FriendCommand {
    Add {
        public_key: String,
        #[arg(long, default_value = "")]
        message: String,
    },
    List,
    Requests,
    Accept {
        request_id: String,
    },
    Reject {
        request_id: String,
    },
    Cancel {
        request_id: String,
    },
    Remove {
        public_key: String,
    },
    Block {
        public_key: String,
    },
    Unblock {
        public_key: String,
    },
}
#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum ChatCommand {
    List,
}
#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum MessageCommand {
    /// Add an emoji reaction to a message in this conversation
    React {
        id: String,
        message_id: String,
        emoji: String,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Send text, streamed stdin or a local attachment
    Send(SendArgs),
    /// Read paginated conversation history
    List(HistoryArgs),
}
#[derive(Args, Debug, Serialize, Deserialize)]
#[command(group(clap::ArgGroup::new("content").required(true).args(["text","text_stdin","img","video","file"])))]
pub struct SendArgs {
    /// Conversation ID from mox chat list
    pub id: String,
    #[arg(long)]
    pub reply_to: Option<String>,
    #[arg(long)]
    pub idempotency_key: Option<String>,
    /// Text to send; quote text containing spaces
    pub text: Option<String>,
    /// Stream text from stdin; EOF completes the message
    #[arg(long)]
    pub text_stdin: bool,
    /// Image file path
    #[arg(long, value_name = "PATH")]
    pub img: Option<PathBuf>,
    /// Video file path
    #[arg(long, value_name = "PATH")]
    pub video: Option<PathBuf>,
    /// File path
    #[arg(long, value_name = "PATH")]
    pub file: Option<PathBuf>,
}
#[derive(Args, Debug, Serialize, Deserialize)]
pub struct HistoryArgs {
    pub id: String,
    /// Starting page (1-based); cannot be combined with --cursor
    #[arg(long, conflicts_with="cursor", value_parser=clap::value_parser!(u32).range(1..))]
    pub page: Option<u32>,
    /// Number of consecutive pages to read
    #[arg(long, default_value_t=1, value_parser=clap::value_parser!(u32).range(1..=20))]
    pub pages: u32,
    /// Messages per page
    #[arg(long, value_parser=clap::value_parser!(u32).range(1..=200), help="Page size; defaults to 50 or the cursor value when continuing")]
    pub page_size: Option<u32>,
    /// Continue the same history snapshot from an earlier response
    #[arg(long)]
    pub cursor: Option<String>,
}
#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum GroupCommand {
    Create {
        #[arg(long)]
        name: String,
    },
    Join {
        invitation: String,
    },
    Invitations,
    Invite {
        id: String,
        public_key: String,
    },
    Members {
        id: String,
    },
    Requests {
        id: String,
    },
    Approve {
        id: String,
        request_id: String,
    },
    Reject {
        id: String,
        request_id: String,
    },
    Remove {
        id: String,
        public_key: String,
    },
    Leave {
        id: String,
    },
}
#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum RelayCommand {
    List,
    Show,
    Add { url: String },
    Use { url: String },
    Set { url: String },
    Remove { url: String },
}
#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum AiCommand {
    /// Show the isolated image/output workspace for a conversation
    Workspace {
        id: String,
    },
    Status,
    Pause {
        id: Option<String>,
    },
    Resume {
        id: Option<String>,
    },
}
