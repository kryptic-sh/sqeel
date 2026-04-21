use crate::state::KeybindingMode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct MainConfig {
    #[serde(default)]
    pub editor: EditorConfig,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EditorConfig {
    pub keybindings: KeybindingMode,
    pub lsp_binary: String,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            keybindings: KeybindingMode::Vim,
            lsp_binary: "sqls".into(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ConnectionConfig {
    pub url: String,
    pub name: String,
}

impl serde::Serialize for KeybindingMode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            KeybindingMode::Vim => s.serialize_str("vim"),
            KeybindingMode::Emacs => s.serialize_str("emacs"),
        }
    }
}

impl<'de> serde::Deserialize<'de> for KeybindingMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.to_lowercase().as_str() {
            "emacs" => Ok(KeybindingMode::Emacs),
            _ => Ok(KeybindingMode::Vim),
        }
    }
}

pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("sqeel"))
}

pub fn load_main_config() -> anyhow::Result<MainConfig> {
    let path = config_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine config dir"))?
        .join("config.toml");

    if !path.exists() {
        return Ok(MainConfig::default());
    }

    let content = std::fs::read_to_string(&path)?;
    Ok(toml::from_str(&content)?)
}

pub fn load_connections() -> anyhow::Result<Vec<ConnectionConfig>> {
    let conns_dir = config_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine config dir"))?
        .join("conns");

    if !conns_dir.exists() {
        return Ok(vec![]);
    }

    let mut conns = Vec::new();
    for entry in std::fs::read_dir(&conns_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();
            let content = std::fs::read_to_string(&path)?;
            let mut conn: ConnectionConfig = toml::from_str(&content)?;
            conn.name = name;
            conns.push(conn);
        }
    }
    Ok(conns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_vim_bindings() {
        let config = MainConfig::default();
        assert_eq!(config.editor.keybindings, KeybindingMode::Vim);
    }

    #[test]
    fn default_config_has_sqls_lsp() {
        let config = MainConfig::default();
        assert_eq!(config.editor.lsp_binary, "sqls");
    }

    #[test]
    fn keybinding_mode_deserialize_vim() {
        let config: MainConfig = toml::from_str(
            r#"
[editor]
keybindings = "vim"
lsp_binary = "sqls"
"#,
        )
        .unwrap();
        assert_eq!(config.editor.keybindings, KeybindingMode::Vim);
    }

    #[test]
    fn keybinding_mode_deserialize_emacs() {
        let config: MainConfig = toml::from_str(
            r#"
[editor]
keybindings = "emacs"
lsp_binary = "sqls"
"#,
        )
        .unwrap();
        assert_eq!(config.editor.keybindings, KeybindingMode::Emacs);
    }

    #[test]
    fn connection_config_parse() {
        let conn: ConnectionConfig = toml::from_str(
            r#"
url = "mysql://user:pass@localhost/mydb"
name = "local"
"#,
        )
        .unwrap();
        assert_eq!(conn.url, "mysql://user:pass@localhost/mydb");
    }
}
