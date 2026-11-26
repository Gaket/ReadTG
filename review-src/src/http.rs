use std::{
    env, fs,
    io::{ErrorKind, Read, Write},
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    sync::Arc,
};

use anyhow::{Result, anyhow, bail};
use axum::{
    Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use subtle::ConstantTimeEq;
use tokio::{net::TcpListener, sync::Semaphore};
use zeroize::Zeroizing;

use crate::{
    reader::{Reader, Scope},
    state,
    telegram::Telegram,
};

const TOKEN_FILE: &str = "mcp-token";
const TOKEN_LENGTH: usize = 64;

struct Access {
    token: Zeroizing<String>,
    in_flight: Semaphore,
}

pub async fn serve() -> Result<()> {
    let scope = Scope::from_env()?;
    let port: u16 = match env::var("TELEGRAM_MCP_PORT") {
        Ok(value) => value
            .parse()
            .map_err(|_| anyhow!("TELEGRAM_MCP_PORT must be a port number."))?,
        Err(env::VarError::NotPresent) => 8173,
        Err(_) => bail!("TELEGRAM_MCP_PORT must be a port number."),
    };
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let address = listener.local_addr()?;
    let token = token(&state::directory()?)?;
    let telegram = if scope.is_closed() {
        None
    } else {
        Some(Telegram::connect(false).await?)
    };
    let router = app(address, token, Reader::new(scope, telegram));
    eprintln!("Telegram MCP listening at http://{address}/mcp");
    axum::serve(listener, router).await?;
    Ok(())
}

fn app(address: SocketAddr, token: Zeroizing<String>, reader: Reader) -> Router {
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts([address.to_string()])
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(8 * 1024);
    let service: StreamableHttpService<_, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(reader.clone()), Default::default(), config);
    let access = Access {
        token,
        in_flight: Semaphore::new(8),
    };
    Router::new()
        .route_service("/mcp", service)
        .route_layer(middleware::from_fn_with_state(Arc::new(access), authorize))
}

async fn authorize(State(access): State<Arc<Access>>, request: Request, next: Next) -> Response {
    // This native-client endpoint permits requests without a browser Origin.
    if request.headers().contains_key(header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !has_token(request.headers(), access.token.as_bytes()) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
        )
            .into_response();
    }
    let Ok(_permit) = access.in_flight.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    next.run(request).await
}

fn has_token(headers: &HeaderMap, token: &[u8]) -> bool {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }

    let bytes = value.as_bytes();
    bytes.len() == token.len() + 7
        && bytes[..7].eq_ignore_ascii_case(b"Bearer ")
        && bool::from(bytes[7..].ct_eq(token))
}

fn token(directory: &Path) -> Result<Zeroizing<String>> {
    let path = directory.join(TOKEN_FILE);
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let mut random = Zeroizing::new([0; TOKEN_LENGTH / 2]);
            getrandom::fill(random.as_mut())
                .map_err(|_| anyhow!("Could not generate a local MCP token."))?;
            let mut file = tempfile::NamedTempFile::new_in(directory)?;
            for &byte in random.iter() {
                write!(file, "{byte:02x}")?;
            }
            match file.persist_noclobber(&path) {
                Ok(_) => {}
                Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.error.into()),
            }
        }
        Err(error) => return Err(error.into()),
    }
    let mut token = Zeroizing::new(String::new());
    state::private_file(&path, false)?
        .take((TOKEN_LENGTH + 1) as u64)
        .read_to_string(&mut token)?;
    if token.len() != TOKEN_LENGTH || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("The local MCP token file is invalid.");
    }
    Ok(token)
}
