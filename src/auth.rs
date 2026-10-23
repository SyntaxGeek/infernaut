use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use url::Url;

use crate::config::{ProviderConfig, ProviderKind};

const OPENAI_CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_ISSUER: &str = "https://auth.openai.com";
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const XAI_ISSUER: &str = "https://auth.x.ai";
const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const XAI_REDIRECT_URI: &str = "http://127.0.0.1:56121/callback";

#[derive(Clone, Debug)]
pub struct AuthManager {
    path: PathBuf,
    refresh_lock: Arc<Mutex<()>>,
}

#[derive(Clone, Debug)]
pub struct RuntimeCredential {
    pub access_token: String,
    pub account_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AuthStore {
    #[serde(default = "store_version")]
    version: u32,
    #[serde(default)]
    providers: BTreeMap<String, StoredCredential>,
}

impl Default for AuthStore {
    fn default() -> Self {
        Self {
            version: store_version(),
            providers: BTreeMap::new(),
        }
    }
}

fn store_version() -> u32 {
    1
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredCredential {
    kind: String,
    access_token: String,
    refresh_token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    id_token: String,
    expires_at: u64,
    token_endpoint: String,
    client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default = "default_token_lifetime")]
    expires_in: u64,
}

fn default_token_lifetime() -> u64 {
    3600
}

#[derive(Debug, Deserialize)]
struct OidcDiscovery {
    authorization_endpoint: String,
    token_endpoint: String,
}

impl AuthManager {
    pub fn new() -> Result<Self> {
        Ok(Self {
            path: auth_store_path()?,
            refresh_lock: Arc::new(Mutex::new(())),
        })
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self {
            path,
            refresh_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn store_path(&self) -> &Path {
        &self.path
    }

    pub async fn credential(
        &self,
        provider: &ProviderConfig,
        client: &Client,
    ) -> Result<RuntimeCredential> {
        if !provider.kind.uses_oauth() {
            bail!("provider '{}' is not configured for OAuth", provider.id);
        }
        let _guard = self.refresh_lock.lock().await;
        let _store_lock = self.lock_store()?;
        let mut store = self.load()?;
        let credential = store.providers.get_mut(&provider.id).ok_or_else(|| {
            anyhow!(
                "not authenticated; run `infernaut auth login {}`",
                provider.id
            )
        })?;

        let skew = if provider.kind == ProviderKind::XaiOauth {
            3600
        } else {
            120
        };
        let refreshed = credential.expires_at <= unix_time().saturating_add(skew);
        if refreshed {
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                refresh(client, credential),
            )
            .await
            .context("OAuth refresh timed out after 30 seconds")??;
        }
        let runtime = RuntimeCredential {
            access_token: credential.access_token.clone(),
            account_id: credential.account_id.clone(),
        };
        if refreshed {
            self.save(&store)?;
        }
        Ok(runtime)
    }

    pub fn status(&self, provider: &ProviderConfig) -> Result<Value> {
        let store = self.load()?;
        let credential = store.providers.get(&provider.id);
        Ok(serde_json::json!({
            "provider": provider.id,
            "kind": provider.kind,
            "authenticated": credential.is_some(),
            "expires_at": credential.map(|value| value.expires_at),
            "expired": credential.map(|value| value.expires_at <= unix_time()),
            "auth_store": self.path,
        }))
    }

    pub fn logout(&self, provider: &ProviderConfig) -> Result<bool> {
        let _store_lock = self.lock_store()?;
        let mut store = self.load()?;
        let removed = store.providers.remove(&provider.id).is_some();
        if removed {
            self.save(&store)?;
        }
        Ok(removed)
    }

    pub async fn login(&self, provider: &ProviderConfig, client: &Client) -> Result<()> {
        let credential = match provider.kind {
            ProviderKind::OpenaiCodex => login_openai_codex(client).await?,
            ProviderKind::XaiOauth => login_xai(provider, client).await?,
            _ => bail!(
                "provider '{}' does not use an interactive OAuth flow",
                provider.id
            ),
        };
        let _store_lock = self.lock_store()?;
        let mut store = self.load()?;
        store.providers.insert(provider.id.clone(), credential);
        self.save(&store)
    }

    fn lock_store(&self) -> Result<fs::File> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("auth store has no parent"))?;
        fs::create_dir_all(parent)?;
        crate::storage::lock(&self.path.with_extension("json.lock"))
    }

    fn load(&self) -> Result<AuthStore> {
        match fs::read(&self.path) {
            Ok(raw) => serde_json::from_slice(&raw)
                .with_context(|| format!("failed to parse {}", self.path.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(AuthStore {
                version: store_version(),
                providers: BTreeMap::new(),
            }),
            Err(error) => {
                Err(error).with_context(|| format!("failed to read {}", self.path.display()))
            }
        }
    }

    fn save(&self, store: &AuthStore) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("auth store has no parent directory"))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let temp = self
            .path
            .with_extension(format!("json.tmp-{:032x}", rand::random::<u128>()));
        let payload = serde_json::to_vec_pretty(store)?;
        write_private(&temp, &payload)?;
        fs::rename(&temp, &self.path)
            .with_context(|| format!("failed to replace {}", self.path.display()))?;
        Ok(())
    }
}

impl ProviderKind {
    pub fn uses_oauth(self) -> bool {
        matches!(self, Self::OpenaiCodex | Self::XaiOauth)
    }
}

async fn login_openai_codex(client: &Client) -> Result<StoredCredential> {
    let device: Value = client
        .post(format!("{OPENAI_ISSUER}/api/accounts/deviceauth/usercode"))
        .json(&serde_json::json!({ "client_id": OPENAI_CODEX_CLIENT_ID }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let user_code = required_string(&device, "user_code")?;
    let device_auth_id = required_string(&device, "device_auth_id")?;
    let interval = device
        .get("interval")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .max(3);

    println!("Open this URL and approve Infernaut's Codex access:");
    println!("{OPENAI_ISSUER}/codex/device");
    println!("Enter code: {user_code}");
    println!("Waiting for authorization (Ctrl+C to cancel)...");

    let deadline = unix_time() + 15 * 60;
    let authorization = loop {
        if unix_time() >= deadline {
            bail!("OpenAI device authorization timed out after 15 minutes");
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        let response = client
            .post(format!("{OPENAI_ISSUER}/api/accounts/deviceauth/token"))
            .json(&serde_json::json!({
                "device_auth_id": device_auth_id,
                "user_code": user_code,
            }))
            .send()
            .await?;
        if response.status().is_success() {
            break response.json::<Value>().await?;
        }
        if !matches!(response.status().as_u16(), 403 | 404) {
            bail!(
                "OpenAI device authorization failed with HTTP {}",
                response.status()
            );
        }
    };

    let code = required_string(&authorization, "authorization_code")?;
    let verifier = required_string(&authorization, "code_verifier")?;
    let token: TokenResponse = client
        .post(OPENAI_TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            (
                "redirect_uri",
                "https://auth.openai.com/deviceauth/callback",
            ),
            ("client_id", OPENAI_CODEX_CLIENT_ID),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    stored_token(
        "openai_codex",
        OPENAI_TOKEN_URL,
        OPENAI_CODEX_CLIENT_ID,
        token,
    )
}

async fn login_xai(provider: &ProviderConfig, client: &Client) -> Result<StoredCredential> {
    let client_id = provider
        .oauth_client_id
        .as_deref()
        .ok_or_else(|| anyhow!("xAI OAuth requires oauth_client_id"))?;
    let discovery: OidcDiscovery = client
        .get(format!("{XAI_ISSUER}/.well-known/openid-configuration"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    validate_xai_url(&discovery.authorization_endpoint)?;
    validate_xai_url(&discovery.token_endpoint)?;

    let verifier = random_urlsafe(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe(24);
    let nonce = random_urlsafe(24);
    let mut authorize = Url::parse(&discovery.authorization_endpoint)?;
    authorize
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", XAI_REDIRECT_URI)
        .append_pair("scope", XAI_SCOPE)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("nonce", &nonce)
        .append_pair("plan", "generic")
        .append_pair("referrer", "infernaut");

    println!("Open this URL and approve Infernaut's xAI access:");
    println!("{authorize}");
    println!();
    println!("Paste the code or the complete callback URL here:");
    print!("> ");
    io::stdout().flush()?;
    let mut pasted = String::new();
    io::stdin().read_line(&mut pasted)?;
    let (code, callback_state) = parse_callback(pasted.trim())?;
    if let Some(callback_state) = callback_state
        && callback_state != state
    {
        bail!("xAI authorization state mismatch");
    }

    let token: TokenResponse = client
        .post(&discovery.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", XAI_REDIRECT_URI),
            ("client_id", client_id),
            ("code_verifier", verifier.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    stored_token("xai_oauth", &discovery.token_endpoint, client_id, token)
}

async fn refresh(client: &Client, credential: &mut StoredCredential) -> Result<()> {
    if credential.refresh_token.is_empty() {
        bail!("OAuth credential has no refresh token; authenticate again");
    }
    let response = client
        .post(&credential.token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", credential.refresh_token.as_str()),
            ("client_id", credential.client_id.as_str()),
        ])
        .send()
        .await?
        .error_for_status()
        .context("OAuth refresh failed; authenticate again")?;
    let token: TokenResponse = response.json().await?;
    credential.access_token = token.access_token;
    if !token.refresh_token.is_empty() {
        credential.refresh_token = token.refresh_token;
    }
    if !token.id_token.is_empty() {
        credential.id_token = token.id_token;
    }
    credential.expires_at = unix_time().saturating_add(token.expires_in);
    credential.account_id = account_id(&credential.access_token)
        .or_else(|| account_id(&credential.id_token))
        .or_else(|| credential.account_id.clone());
    Ok(())
}

fn stored_token(
    kind: &str,
    token_endpoint: &str,
    client_id: &str,
    token: TokenResponse,
) -> Result<StoredCredential> {
    if token.access_token.is_empty() || token.refresh_token.is_empty() {
        bail!("OAuth token response did not include access and refresh tokens");
    }
    let account_id = account_id(&token.access_token).or_else(|| account_id(&token.id_token));
    Ok(StoredCredential {
        kind: kind.to_owned(),
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        id_token: token.id_token,
        expires_at: unix_time().saturating_add(token.expires_in),
        token_endpoint: token_endpoint.to_owned(),
        client_id: client_id.to_owned(),
        account_id,
    })
}

fn account_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(|value| value.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn parse_callback(value: &str) -> Result<(String, Option<String>)> {
    if value.starts_with("http://") || value.starts_with("https://") {
        let url = Url::parse(value)?;
        let code = url
            .query_pairs()
            .find(|(key, _)| key == "code")
            .map(|(_, value)| value.into_owned())
            .ok_or_else(|| anyhow!("callback URL does not contain a code"))?;
        let state = url
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned());
        Ok((code, state))
    } else if value.is_empty() {
        bail!("authorization code is empty")
    } else {
        Ok((value.to_owned(), None))
    }
}

fn validate_xai_url(value: &str) -> Result<()> {
    let url = Url::parse(value)?;
    let host = url.host_str().unwrap_or_default();
    if url.scheme() != "https" || (host != "x.ai" && !host.ends_with(".x.ai")) {
        bail!("xAI discovery returned an endpoint outside the HTTPS x.ai origin");
    }
    Ok(())
}

fn required_string(value: &Value, key: &str) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("OAuth response is missing '{key}'"))
}

fn random_urlsafe(bytes: usize) -> String {
    let mut value = vec![0_u8; bytes];
    rand::rng().fill_bytes(&mut value);
    URL_SAFE_NO_PAD.encode(value)
}

fn auth_store_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("INFERNAUT_AUTH_FILE") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow!("HOME is not set; set INFERNAUT_AUTH_FILE to an explicit path"))?;
    Ok(PathBuf::from(home).join(".infernaut/auth.json"))
}

fn write_private(path: &Path, payload: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(payload)?;
        file.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)?;
        file.write_all(payload)?;
        file.sync_all()?;
    }
    Ok(())
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_accepts_url_or_bare_code() {
        assert_eq!(
            parse_callback("http://127.0.0.1/callback?code=abc&state=xyz").unwrap(),
            ("abc".into(), Some("xyz".into()))
        );
        assert_eq!(parse_callback("abc").unwrap(), ("abc".into(), None));
    }

    #[test]
    fn auth_store_is_private() {
        let root = std::env::temp_dir().join(format!("infernaut-auth-{}", random_urlsafe(8)));
        let manager = AuthManager::at(root.join("auth.json"));
        manager.save(&AuthStore::default()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(manager.store_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let _ = fs::remove_dir_all(root);
    }
}
