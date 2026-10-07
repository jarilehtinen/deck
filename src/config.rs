//! Settings (`~/.config/deck/config.toml`) and directory paths.

use std::{
    fs::DirBuilder,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
};

use anyhow::{Context, Result, anyhow};
use librespot_oauth::{OAuthClient, OAuthClientBuilder};
use serde::Deserialize;

/// OAuth redirect address. Exactly this Redirect URI must be added to your own app's
/// settings on developer.spotify.com.
pub const OAUTH_REDIRECT_URI: &str = "http://127.0.0.1:8898/login";
pub const OAUTH_RESPONSE: &str = "<!doctype html><html><body>\
    <h1>Deck is signed in</h1><p>You can close this tab.</p>\
    </body></html>";

/// OAuth client that opens the sign-in in the browser and shows Deck's own thank-you
/// page. Used both for the librespot sign-in and for your own app's Web API token.
pub fn oauth_client(client_id: &str, scopes: &[&str]) -> Result<OAuthClient> {
    Ok(
        OAuthClientBuilder::new(client_id, OAUTH_REDIRECT_URI, scopes.to_vec())
            .open_in_browser()
            .with_custom_message(OAUTH_RESPONSE)
            .build()?,
    )
}

/// An unknown key is an error, so a typo (`clientid`) does not go unnoticed.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Client ID of your own Spotify app for Web API calls. Without it the
    /// librespot session token is used, which Spotify rate-limits (429).
    pub client_id: Option<String>,
    /// Last.fm API key for curation (`deck taste`, `deck curate`).
    pub lastfm_api_key: Option<String>,
    /// Last.fm user whose listening history is the taste data for curation. Required,
    /// like `lastfm_api_key`, whenever Last.fm is needed.
    pub lastfm_user: Option<String>,
    /// Audio quality of playback.
    #[serde(default)]
    pub bitrate: Bitrate,
}

/// Audio quality of playback, Ogg Vorbis at 96, 160 or 320 kbps. Written in the config
/// file as a number (`bitrate = 320`); any other number is an error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "i64")]
pub enum Bitrate {
    Kbps96,
    #[default]
    Kbps160,
    Kbps320,
}

impl TryFrom<i64> for Bitrate {
    type Error = String;

    fn try_from(kbps: i64) -> Result<Self, Self::Error> {
        match kbps {
            96 => Ok(Self::Kbps96),
            160 => Ok(Self::Kbps160),
            320 => Ok(Self::Kbps320),
            _ => Err(format!("bitrate must be 96, 160 or 320, not {kbps}")),
        }
    }
}

impl Config {
    /// Reads the settings. A missing file means the defaults.
    pub fn load() -> Result<Self> {
        let path = config_dir()?.join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("config file {} is invalid", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("cannot open {}", path.display())),
        }
    }
}

pub fn config_dir() -> Result<PathBuf> {
    Ok(home()?.join(".config/deck"))
}

pub fn cache_dir() -> Result<PathBuf> {
    Ok(home()?.join(".cache/deck"))
}

/// Creates the cache directory if needed and restricts it to the owner (0700): it
/// holds the sign-in credentials (`credentials.json`, `webapi.json`) and the log.
pub fn private_cache_dir() -> Result<PathBuf> {
    let dir = cache_dir()?;
    make_private_dir(&dir)?;
    Ok(dir)
}

/// Creates the directory with mode 0700, or fixes an existing one's mode to 0700.
fn make_private_dir(dir: &std::path::Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .and_then(|()| std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)))
        .with_context(|| format!("cannot create {}", dir.display()))
}

/// State that persists across restarts but is neither a setting nor a cache
/// (XDG's `XDG_STATE_HOME`): the playback state.
pub fn state_dir() -> Result<PathBuf> {
    Ok(home()?.join(".local/state/deck"))
}

fn home() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().ok_or_else(|| anyhow!("home directory not found"))?;
    Ok(base.home_dir().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_keys_are_read() {
        let config: Config =
            toml::from_str("client_id = \"abc\"\nlastfm_api_key = \"key\"\nlastfm_user = \"me\"\n")
                .unwrap();
        assert_eq!(config.client_id.as_deref(), Some("abc"));
        assert_eq!(config.lastfm_api_key.as_deref(), Some("key"));
        assert_eq!(config.lastfm_user.as_deref(), Some("me"));
    }

    #[test]
    fn bitrate_defaults_to_160() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.bitrate, Bitrate::Kbps160);
    }

    #[test]
    fn bitrate_is_read() {
        let config: Config = toml::from_str("bitrate = 320\n").unwrap();
        assert_eq!(config.bitrate, Bitrate::Kbps320);
        let config: Config = toml::from_str("bitrate = 96\n").unwrap();
        assert_eq!(config.bitrate, Bitrate::Kbps96);
    }

    #[test]
    fn wrong_bitrate_is_an_error_that_lists_the_allowed_values() {
        let error = toml::from_str::<Config>("bitrate = 256\n").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("bitrate must be 96, 160 or 320, not 256"),
            "{error}"
        );
        assert!(toml::from_str::<Config>("bitrate = \"320\"\n").is_err());
    }

    #[test]
    fn unknown_config_key_is_an_error() {
        let error = toml::from_str::<Config>("clientid = \"abc\"\n").unwrap_err();
        assert!(
            error.to_string().contains("unknown field `clientid`"),
            "{error}"
        );
    }

    #[test]
    fn private_dir_is_created_and_fixed_to_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let mode =
            |dir: &std::path::Path| std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        let new = tmp.path().join("a/deck");
        make_private_dir(&new).unwrap();
        assert_eq!(mode(&new), 0o700);

        let old = tmp.path().join("old");
        std::fs::create_dir(&old).unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
        make_private_dir(&old).unwrap();
        assert_eq!(mode(&old), 0o700);
    }
}
