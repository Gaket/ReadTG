mod http;
mod reader;
mod state;
mod telegram;

use std::{env, io::IsTerminal};

use anyhow::{Result, bail};
use reader::{ChatRequest, Scope};
use telegram::Telegram;

const USAGE: &str = "Usage: telegram-reader-mcp [serve | login | chats [NAME] | --help]";

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
            let telegram = Telegram::connect(false).await?;
            let page = telegram.list_chats(&Scope::All, params).await?;
            println!("{}", serde_json::to_string(&page)?);
            Ok(())
        }
        [cmd] if cmd == "--help" || cmd == "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}
