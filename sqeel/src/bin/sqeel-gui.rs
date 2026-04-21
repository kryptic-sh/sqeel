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
                let mut s = state.lock().unwrap();
                s.active_connection = Some(conn.url.clone());
                drop(s);
            }
            Err(e) => {
                state
                    .lock()
                    .unwrap()
                    .set_error(format!("Connection failed: {e}"));
            }
        }
    }

    GuiProvider::run(state)?;
    Ok(())
}
