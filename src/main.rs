mod http;
mod reader;
mod state;
mod telegram;

use std::{env, io::IsTerminal};

use anyhow::{Result, bail};
use reader::{ChatRequest, Scope};
use telegram::Telegram;

const USAGE: &str = "Usage: telegram-reader-mcp [serve | login | chats [NAME] | --help]";
const SCOPE_HELP: &str = "With TELEGRAM_ALLOWED_CHATS unset, serve opens all cloud chats after verifying the saved Telegram sign-in.
Before setup, it serves with chat access closed.
Set TELEGRAM_ALLOWED_CHATS to closed (or empty), all, or a comma-separated list of chat IDs before startup.";

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("telegram-reader-mcp: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [] => http::serve().await,
        [cmd] if cmd == "serve" => http::serve().await,
        [cmd] if cmd == "login" => {
            if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
                bail!("Run login directly in an interactive terminal.");
            }
            Telegram::connect(true).await?.login().await
        }
        [cmd, ..] if cmd == "chats" && args.len() <= 2 => {
            let params = ChatRequest {
                name: args.get(1).cloned(),
                limit: Some(40),
            }
            .validate()?;
            // Listing dialogs checks the session on the first read.
            let telegram = Telegram::connect(false).await?;
            let page = telegram.list_chats(&Scope::All, params).await?;
            println!("{}", serde_json::to_string(&page)?);
            Ok(())
        }
        [cmd] if cmd == "--help" || cmd == "-h" => {
            println!("{USAGE}\n\n{SCOPE_HELP}");
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}
