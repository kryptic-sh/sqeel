use std::sync::Arc;

use clap::Parser;
use sqeel_core::{
    AppState, UiProvider,
    config::{load_connections, load_session_data, save_session},
    db::DbConnection,
    persistence::{load_schema_cache, save_schema_cache},
};
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

    /// Show debug panel at the bottom
    #[arg(long)]
    debug: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let state = AppState::new();
    state.lock().unwrap().debug_mode = args.debug;

    let conns = load_connections().unwrap_or_default();
    state
        .lock()
        .unwrap()
        .set_available_connections(conns.clone());

    let session = load_session_data();
    let url = if let Some(url) = args.url {
        Some(url)
    } else {
        let name = args.connection.or(session.connection);
        name.and_then(|n| conns.iter().find(|c| c.name == n).map(|c| c.url.clone()))
    };
    let session_schema_cursor = session.schema_cursor;

    // Runtime for async setup (initial connect + reconnection watcher).
    // TuiProvider::run creates its own runtime; must not be called from inside one.
    let rt = tokio::runtime::Runtime::new()?;

    if let Some(url) = url {
        rt.block_on(connect_and_spawn(&state, &url, session_schema_cursor));
    }

    let watcher_state = state.clone();
    rt.spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let pending = watcher_state.lock().unwrap().pending_reconnect.take();
            if let Some(url) = pending {
                connect_and_spawn(&watcher_state, &url, 0).await;
            }
        }
    });

    TuiProvider::run(state)?;
    Ok(())
}

async fn connect_and_spawn(
    state: &Arc<std::sync::Mutex<AppState>>,
    url: &str,
    session_schema_cursor: usize,
) {
    match DbConnection::connect(url).await {
        Ok(conn) => {
            {
                let mut s = state.lock().unwrap();
                let conn_name = s
                    .available_connections
                    .iter()
                    .find(|c| c.url == url)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| conn.url.clone());
                s.active_connection = Some(conn_name.clone());
                s.set_status(format!("Connected: {conn_name}"));
                if let Some(name) = s
                    .available_connections
                    .iter()
                    .find(|c| c.url == url)
                    .map(|c| c.name.clone())
                {
                    let _ = save_session(&name, s.schema_cursor);
                }
            }
            spawn_executor(state.clone(), conn, session_schema_cursor);
        }
        Err(e) => {
            state
                .lock()
                .unwrap()
                .set_error(format!("Connection failed: {e}"));
        }
    }
}

fn spawn_executor(
    state: Arc<std::sync::Mutex<AppState>>,
    conn: DbConnection,
    session_schema_cursor: usize,
) {
    let conn = Arc::new(conn);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(8);
    state.lock().unwrap().query_tx = Some(tx);

    // Show cached schema immediately with restored cursor, then refresh in background
    if let Some(cached) = load_schema_cache(&conn.url) {
        let mut s = state.lock().unwrap();
        s.set_schema_nodes(cached);
        let max = s.visible_schema_items().len().saturating_sub(1);
        s.schema_cursor = session_schema_cursor.min(max);
    }

    let schema_conn = conn.clone();
    let schema_state = state.clone();
    let schema_url = conn.url.clone();
    tokio::spawn(async move {
        match schema_conn.load_schema().await {
            Ok(nodes) => {
                let _ = save_schema_cache(&schema_url, &nodes);
                let mut s = schema_state.lock().unwrap();
                // Preserve expansion state + cursor across the refresh
                let cursor = s.schema_cursor;
                s.refresh_schema_nodes(nodes);
                // Save cursor to session so it persists across restarts
                if let Some(ref name) = s.active_connection.clone() {
                    let _ = save_session(name, cursor);
                }
            }
            Err(e) => schema_state
                .lock()
                .unwrap()
                .set_status(format!("Schema load failed: {e}")),
        }
    });

    tokio::spawn(async move {
        while let Some(query) = rx.recv().await {
            let result = conn.execute(&query).await;
            let mut s = state.lock().unwrap();
            match result {
                Ok(r) => {
                    s.persist_result(&query, &r);
                    s.push_history(&query);
                    s.set_results(r);
                }
                Err(e) => s.set_error(e.to_string()),
            }
        }
    });
}
