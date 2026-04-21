use clap::Parser;
use sqeel_core::{AppState, UiProvider, config::load_connections, db::DbConnection};
use sqeel_tui::TuiProvider;

#[derive(Parser)]
#[command(name = "sqeel", about = "Fast vim-native SQL client")]
struct Args {
    /// Connection URL (e.g. mysql://user:pass@host/db)
    #[arg(short = 'u', long)]
    url: Option<String>,

    /// Named connection from config (e.g. local)
    #[arg(short = 'c', long)]
    connection: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let state = AppState::new();

    // Resolve connection URL
    let url = if let Some(url) = args.url {
        Some(url)
    } else if let Some(name) = args.connection {
        let conns = load_connections().unwrap_or_default();
        conns.into_iter().find(|c| c.name == name).map(|c| c.url)
    } else {
        None
    };

    if let Some(url) = url {
        match DbConnection::connect(&url).await {
            Ok(conn) => {
                {
                    let mut s = state.lock().unwrap();
                    s.active_connection = Some(conn.url.clone());
                    s.set_status(format!("Connected: {}", conn.url));
                }
                run_with_connection(state, conn).await?;
            }
            Err(e) => {
                {
                    let mut s = state.lock().unwrap();
                    s.set_error(format!("Connection failed: {e}"));
                }
                TuiProvider::run(state)?;
            }
        }
    } else {
        TuiProvider::run(state)?;
    }

    Ok(())
}

async fn run_with_connection(
    state: std::sync::Arc<std::sync::Mutex<AppState>>,
    conn: DbConnection,
) -> anyhow::Result<()> {
    use std::sync::Arc;
    let conn = Arc::new(conn);
    let conn_clone = conn.clone();
    let state_clone = state.clone();

    // Spawn query executor task
    let (query_tx, mut query_rx) = tokio::sync::mpsc::channel::<String>(8);

    tokio::spawn(async move {
        while let Some(query) = query_rx.recv().await {
            let result = conn_clone.execute(&query).await;
            let mut s = state_clone.lock().unwrap();
            match result {
                Ok(r) => s.set_results(r),
                Err(e) => s.set_error(e.to_string()),
            }
        }
    });

    // Store query sender in state via a global (simple approach for now)
    QUERY_TX.set(query_tx).ok();

    TuiProvider::run(state)?;
    Ok(())
}

static QUERY_TX: std::sync::OnceLock<tokio::sync::mpsc::Sender<String>> =
    std::sync::OnceLock::new();

pub fn send_query(query: String) {
    if let Some(tx) = QUERY_TX.get() {
        let _ = tx.try_send(query);
    }
}
