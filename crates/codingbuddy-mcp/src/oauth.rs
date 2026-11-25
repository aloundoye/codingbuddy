//! OAuth for HTTP MCP servers

use super::*;

/// OAuth token for MCP server authentication.
#[derive(Clone, Serialize, Deserialize)]
pub struct McpOAuthToken {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub expires_at: Option<String>,
    pub server_id: String,
}

impl std::fmt::Debug for McpOAuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpOAuthToken")
            .field("server_id", &self.server_id)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}
/// Bind authentication to the exact endpoint, source and canonical project scope.
pub(crate) fn mcp_token_binding(server: &McpServer) -> Result<String> {
    let url = reqwest::Url::parse(server.url.as_deref().context("MCP endpoint unavailable")?)?;
    anyhow::ensure!(
        server.metadata.get("_codingbuddy_source").is_some(),
        "MCP source identity unavailable"
    );
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(
            url.as_str(),
            &server.id,
            &server.metadata["_codingbuddy_source"],
            &server.metadata["_codingbuddy_workspace"],
            &server.metadata["oauth"]
        ))?)
    ))
}

/// Store an OAuth token for an MCP server.
/// Tries system keychain first, falls back to file-based storage.
pub fn store_mcp_token(server_id: &str, token: &McpOAuthToken) -> Result<()> {
    let json = serde_json::to_string(token)?;

    if let Ok(entry) = keyring::Entry::new("codingbuddy-mcp", server_id)
        && entry.set_password(&json).is_ok()
    {
        return Ok(());
    }

    store_token_file(server_id, &json)
}

/// Load a stored OAuth token for an MCP server.
/// Tries system keychain first, falls back to file-based storage.
pub fn load_mcp_token(server_id: &str) -> Result<Option<McpOAuthToken>> {
    if let Ok(entry) = keyring::Entry::new("codingbuddy-mcp", server_id)
        && let Ok(json) = entry.get_password()
        && let Ok(token) = serde_json::from_str::<McpOAuthToken>(&json)
    {
        return Ok(Some(token));
    }

    load_token_file(server_id)
}

/// Delete a stored OAuth token from both keychain and file.
pub fn delete_mcp_token(server_id: &str) -> Result<()> {
    if let Ok(entry) = keyring::Entry::new("codingbuddy-mcp", server_id) {
        let _ = entry.delete_credential();
    }
    delete_token_file(server_id)
}

pub(crate) fn token_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| anyhow::anyhow!("cannot determine home directory"))?;
    Ok(PathBuf::from(home).join(".codingbuddy/mcp-tokens"))
}

pub(crate) fn store_token_file(server_id: &str, json: &str) -> Result<()> {
    let dir = token_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{:x}.json", Sha256::digest(server_id.as_bytes())));
    fs::write(&path, json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub(crate) fn load_token_file(server_id: &str) -> Result<Option<McpOAuthToken>> {
    let path = token_dir()?.join(format!("{:x}.json", Sha256::digest(server_id.as_bytes())));
    match fs::read_to_string(path) {
        Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn delete_token_file(server_id: &str) -> Result<()> {
    let path = token_dir()?.join(format!("{:x}.json", Sha256::digest(server_id.as_bytes())));
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ── OAuth 2.0 for MCP ────────────────────────────────────────────────────────

/// OAuth 2.0 configuration for an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpOAuthConfig {
    pub authorization_url: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    /// Local port for the OAuth redirect callback (default 8912).
    #[serde(default = "default_redirect_port")]
    pub redirect_port: u16,
}

pub(crate) fn default_redirect_port() -> u16 {
    8912
}

/// Check whether an OAuth token has expired.
pub fn is_token_expired(token: &McpOAuthToken) -> bool {
    token.expires_at.as_deref().is_some_and(|exp| {
        chrono::DateTime::parse_from_rfc3339(exp)
            .map(|dt| dt < Utc::now())
            .unwrap_or(false)
    })
}

/// Generate a PKCE code verifier (random URL-safe base64 string).
pub(crate) fn generate_pkce_verifier() -> String {
    let buf = rand_bytes();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Generate a PKCE code challenge (S256) from a verifier.
pub(crate) fn generate_pkce_challenge(verifier: &str) -> String {
    let hash = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash)
}

pub(crate) fn rand_bytes() -> [u8; 32] {
    let mut buf = [0u8; 32];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    } else {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        buf[..16].copy_from_slice(&seed.to_le_bytes());
        buf[16..20].copy_from_slice(&std::process::id().to_le_bytes());
    }
    buf
}

/// Start the OAuth 2.0 authorization code flow for an MCP server.
pub fn start_oauth_flow(server: &McpServer) -> Result<McpOAuthToken> {
    let oauth = server
        .metadata
        .get("oauth")
        .map(|v| serde_json::from_value::<McpOAuthConfig>(v.clone()))
        .transpose()?
        .ok_or_else(|| anyhow!("server has no OAuth config in metadata"))?;

    // 1. Start local listener
    let listener = std::net::TcpListener::bind(format!("127.0.0.1:{}", oauth.redirect_port))?;

    // 2. Build authorization URL with PKCE
    let state = format!("{:x}", Sha256::digest(rand_bytes()));
    let code_verifier = generate_pkce_verifier();
    let code_challenge = generate_pkce_challenge(&code_verifier);
    let redirect_uri = format!("http://127.0.0.1:{}/callback", oauth.redirect_port);
    let auth_url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&state={}&scope={}&code_challenge={}&code_challenge_method=S256",
        oauth.authorization_url,
        oauth.client_id,
        urlencoding(&redirect_uri),
        state,
        oauth.scopes.join("+"),
        code_challenge,
    );

    // 3. Open browser
    eprintln!("Opening browser for OAuth authorization...");
    let _ = open_browser(&auth_url);
    eprintln!("If browser didn't open, visit: {auth_url}");

    // 4. Wait for callback
    let code = wait_for_oauth_callback(&listener, &state, Duration::from_secs(120))?;

    // 5. Exchange code for token
    let mut token = exchange_oauth_code(&oauth, &code, &code_verifier, &server.id)?;
    token.binding = Some(mcp_token_binding(server)?);

    // 6. Store token
    store_mcp_token(&server.id, &token)?;
    Ok(token)
}

pub(crate) fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

pub(crate) fn exchange_oauth_code(
    oauth: &McpOAuthConfig,
    code: &str,
    code_verifier: &str,
    server_id: &str,
) -> Result<McpOAuthToken> {
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?;
    let mut body = json!({
        "grant_type": "authorization_code",
        "code": code,
        "client_id": oauth.client_id,
        "code_verifier": code_verifier,
    });
    if let Some(ref secret) = oauth.client_secret {
        body["client_secret"] = json!(secret);
    }
    let resp: serde_json::Value = client
        .post(&oauth.token_url)
        .json(&body)
        .send()?
        .error_for_status()?
        .json()?;

    Ok(McpOAuthToken {
        binding: None,
        access_token: resp["access_token"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        refresh_token: resp
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(String::from),
        token_type: resp["token_type"].as_str().unwrap_or("Bearer").to_string(),
        expires_at: resp
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .map(|secs| (Utc::now() + chrono::Duration::seconds(secs as i64)).to_rfc3339()),
        server_id: server_id.to_string(),
    })
}

/// Refresh an expired OAuth token.
pub fn refresh_oauth_token(server: &McpServer, token: &McpOAuthToken) -> Result<McpOAuthToken> {
    anyhow::ensure!(
        token.binding.as_ref() == Some(&mcp_token_binding(server)?),
        "MCP authentication binding changed; authenticate explicitly"
    );
    let oauth = server
        .metadata
        .get("oauth")
        .map(|v| serde_json::from_value::<McpOAuthConfig>(v.clone()))
        .transpose()?
        .ok_or_else(|| anyhow!("server has no OAuth config"))?;
    let refresh = token
        .refresh_token
        .as_deref()
        .ok_or_else(|| anyhow!("no refresh token available"))?;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp: serde_json::Value = client
        .post(&oauth.token_url)
        .json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh,
            "client_id": oauth.client_id,
        }))
        .send()?
        .error_for_status()?
        .json()?;

    let new_token = McpOAuthToken {
        binding: token.binding.clone(),
        access_token: resp["access_token"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        refresh_token: resp
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| token.refresh_token.clone()),
        token_type: resp["token_type"].as_str().unwrap_or("Bearer").to_string(),
        expires_at: resp
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .map(|secs| (Utc::now() + chrono::Duration::seconds(secs as i64)).to_rfc3339()),
        server_id: server.id.clone(),
    };
    store_mcp_token(&server.id, &new_token)?;
    Ok(new_token)
}

pub(crate) fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = Command::new("cmd").args(["/c", "start", url]).spawn();
    }
    Ok(())
}

pub(crate) fn wait_for_oauth_callback(
    listener: &std::net::TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    listener.set_nonblocking(true)?;
    let started = std::time::Instant::now();
    loop {
        if started.elapsed() > timeout {
            return Err(anyhow!("OAuth callback timed out"));
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                use std::io::Read;
                let mut buf = [0u8; 4096];
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(
                    Duration::from_secs(5).min(timeout.saturating_sub(started.elapsed())),
                ))?;
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                if let Some(code) = extract_query_param(&request, "code") {
                    let state = extract_query_param(&request, "state").unwrap_or_default();
                    if state != expected_state {
                        return Err(anyhow!("OAuth state mismatch"));
                    }
                    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html><body>Authorization successful. You can close this tab.</body></html>";
                    use std::io::Write;
                    let _ = stream.write_all(response.as_bytes());
                    return Ok(code);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

pub(crate) fn extract_query_param(request: &str, param: &str) -> Option<String> {
    let query = request.split_whitespace().nth(1)?.split('?').nth(1)?;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=')
            && key == param
        {
            return Some(value.to_string());
        }
    }
    None
}
