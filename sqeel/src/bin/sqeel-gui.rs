use std::sync::Arc;

use clap::Parser;
use sqeel_core::{AppState, UiProvider, config::load_connections, db::DbConnection};
use sqeel_gui::GuiProvider;

#[derive(Parser)]
#[command(name = "sqeel-gui", about = "Fast vim-native SQL client (GUI)")]
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

    let conns = load_connections().unwrap_or_default();
    state
        .lock()
        .unwrap()
        .set_available_connections(conns.clone());

    let url = if let Some(url) = args.url {
        Some(url)
    } else if let Some(name) = args.connection {
        conns.into_iter().find(|c| c.name == name).map(|c| c.url)
    } else {
        None
    };

    if let Some(url) = url {
        connect_and_spawn(&state, &url).await;
    }

    // Reconnection watcher for the GUI
    let watcher_state = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let pending = watcher_state.lock().unwrap().pending_reconnect.take();
            if let Some(url) = pending {
                connect_and_spawn(&watcher_state, &url).await;
            }
        }
    });

    GuiProvider::run(state)?;
    Ok(())
}

async fn connect_and_spawn(state: &Arc<std::sync::Mutex<AppState>>, url: &str) {
    match DbConnection::connect(url).await {
        Ok(conn) => {
            {
                let mut s = state.lock().unwrap();
                s.active_connection = Some(conn.url.clone());
                s.set_status(format!("Connected: {}", conn.url));
            }
            spawn_executor(state.clone(), conn);
        }
        Err(e) => {
            state
                .lock()
                .unwrap()
                .set_error(format!("Connection failed: {e}"));
        }
    }
}

fn spawn_executor(state: Arc<std::sync::Mutex<AppState>>, conn: DbConnection) {
    let conn = Arc::new(conn);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(8);
    state.lock().unwrap().query_tx = Some(tx);

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
