use std::{net::SocketAddr, path::{Path, PathBuf}};
use anyhow::{Context, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

#[cfg(target_os = "windows")]
const DEFAULT_LIBRARY: &str = "xray.dll";
#[cfg(target_os = "linux")]
const DEFAULT_LIBRARY: &str = "libxray.so";
#[cfg(target_os = "macos")]
const DEFAULT_LIBRARY: &str = "libxray.dylib";
#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
compile_error!("xray-manager supports Windows, Linux and macOS");

fn default_library() -> PathBuf { PathBuf::from(DEFAULT_LIBRARY) }
fn default_api() -> SocketAddr { "127.0.0.1:10085".parse().unwrap() }
fn default_fingerprint() -> String { "chrome".into() }

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
	pub listen: SocketAddr,
	pub database_url: String,
	pub token_ttl_seconds: i64,
	pub session_ttl_seconds: i64,
	pub heartbeat_timeout_seconds: i64,
	pub connect_grace_seconds: i64,
	pub offline_grace_seconds: i64,
	pub cleanup_interval_seconds: u64,
	#[serde(default)]
	pub admin: Option<AdminConfig>,
	#[serde(default)]
	pub smtp: Option<SmtpConfig>,
	pub xray: XrayConfig,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
	pub username: String,
	pub password: String,
	#[serde(default)]
	pub email: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmtpConfig {
	pub host: String,
	#[serde(default = "smtp_port")]
	pub port: u16,
	pub username: String,
	pub password: String,
	pub from: String,
	#[serde(default)]
	pub title: Option<String>,
	#[serde(default)]
	pub body: Option<String>,
}
fn smtp_port() -> u16 { 465 }

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XrayConfig {
	#[serde(default = "default_library")]
	pub library: PathBuf,
	pub server: Value,
	#[serde(skip)]
	pub inbound_tag: String,
	#[serde(skip, default = "default_api")]
	pub api_listen: SocketAddr,
	#[serde(default)]
	pub public_address: Option<String>,
	#[serde(default)]
	pub public_port: Option<u16>,
	#[serde(skip)]
	pub protocol: String,
	#[serde(default)]
	pub flow: String,
	#[serde(default = "default_fingerprint")]
	pub fingerprint: String,
	#[serde(default)]
	pub tls_server_name: Option<String>,
	// Derived using a whitelist: server private settings never reach clients.
	#[serde(skip)]
	pub client_stream_settings: Value,
}

impl XrayConfig {
	fn derive_settings(&mut self) -> anyhow::Result<()> {
		self.public_address = self.public_address.take().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
		let inbounds = self.server["inbounds"].as_array().context("xray.server.inbounds must be an array")?;
		let managed: Vec<_> = inbounds.iter().filter(|i| matches!(i["protocol"].as_str(), Some("vless" | "trojan"))).collect();
		ensure!(managed.len() == 1, "configure exactly one VLESS/Trojan inbound in xray.server");
		let inbound = managed[0];
		self.inbound_tag = inbound["tag"].as_str().context("managed inbound requires tag")?.into();
		self.protocol = inbound["protocol"].as_str().unwrap().into();
		let inbound_port = u16::try_from(inbound["port"].as_u64().context("inbound port must be an integer")?)?;
		ensure!(inbound_port > 0, "invalid inbound port");
		self.public_port = Some(self.public_port.unwrap_or(inbound_port));
		self.api_listen = self.server["api"]["listen"].as_str().context("xray.server.api.listen is required")?.parse()?;
		ensure!(inbound["settings"]["clients"].as_array().is_some_and(|v| v.is_empty()), "managed clients must be empty");
		let stream = &inbound["streamSettings"];
		let network = stream["network"].as_str().unwrap_or("raw");
		ensure!(matches!(network, "raw" | "tcp"), "automatic client configuration currently supports raw/tcp only");
		// A custom TCP header needs its own client mapping, not an unsafe whole-object copy.
		for field in ["tcpSettings", "rawSettings"] {
			if let Some(settings) = stream.get(field) {
				ensure!(settings.as_object().is_some_and(|s| s.is_empty()), "custom TCP/raw settings are not supported");
			}
		}
		let security = stream["security"].as_str().unwrap_or("none");
		let mut client = json!({"network":network, "security":security});
		match security {
			"reality" => {
				let reality = &stream["realitySettings"];
				let key = reality["privateKey"].as_str().context("REALITY privateKey is required")?;
				let key: [u8; 32] = URL_SAFE_NO_PAD.decode(key).context("invalid REALITY private key encoding")?
					.try_into().map_err(|_| anyhow::anyhow!("REALITY private key must be 32 bytes"))?;
				let public = x25519_dalek::x25519(key, x25519_dalek::X25519_BASEPOINT_BYTES);
				let sni = reality["serverNames"][0].as_str().filter(|s| !s.is_empty()).context("REALITY requires a serverNames entry")?;
				let short_id = reality["shortIds"][0].as_str().context("REALITY requires a shortIds entry")?;
				ensure!(short_id.len() <= 16 && short_id.len() % 2 == 0 && short_id.bytes().all(|v| v.is_ascii_hexdigit()), "invalid REALITY shortId");
				ensure!(!self.fingerprint.is_empty(), "REALITY fingerprint is required");
				ensure!(reality["mldsa65Seed"].as_str().is_none_or(|s| s.is_empty()), "ML-DSA REALITY client derivation is not supported");
				client["realitySettings"] = json!({"serverName":sni,"fingerprint":self.fingerprint,
					"password":URL_SAFE_NO_PAD.encode(public),"shortId":short_id,"spiderX":"/"});
			}
			"tls" => {
				client["tlsSettings"] = json!({"allowInsecure":false});
				if let Some(name) = self.tls_server_name.as_deref().filter(|s| !s.trim().is_empty()).or(self.public_address.as_deref()) {
					client["tlsSettings"]["serverName"] = json!(name);
				}
				if let Some(alpn) = stream["tlsSettings"].get("alpn") { client["tlsSettings"]["alpn"] = alpn.clone(); }
			}
			"none" => {},
			_ => anyhow::bail!("unsupported stream security"),
		}
		ensure!(self.flow.is_empty() || (self.protocol == "vless" && self.flow == "xtls-rprx-vision" && matches!(security, "tls" | "reality")), "invalid flow for protocol/security");
		self.client_stream_settings = client;
		Ok(())
	}
}

impl Config {
	pub fn load(path: &Path) -> anyhow::Result<Self> {
		let mut config: Self = toml::from_str(&std::fs::read_to_string(path)?)?;
		if let Some(admin) = &mut config.admin {
			admin.username = crate::auth::normalize_username(&admin.username).map_err(|e| anyhow::anyhow!(e.1))?;
			crate::auth::validate_password(&admin.password).map_err(|e| anyhow::anyhow!(e.1))?;
		}
		let base = path.parent().unwrap_or(Path::new("."));
		for item in [&mut config.xray.library] {
			if item.is_relative() { *item = base.join(&*item); }
		}
		let expected = Path::new(DEFAULT_LIBRARY).extension().unwrap();
		ensure!(config.xray.library.extension().is_some_and(|extension| extension.eq_ignore_ascii_case(expected)),
			"library extension must match this platform ({DEFAULT_LIBRARY})");
		if let Ok(url) = std::env::var("DATABASE_URL") { config.database_url = url; }
		config.xray.derive_settings()?;
		ensure!(config.xray.api_listen.ip().is_loopback(), "Xray API must listen on loopback");
		ensure!(matches!(config.xray.protocol.as_str(), "vless" | "trojan"), "protocol must be vless or trojan");
		ensure!(!config.xray.inbound_tag.is_empty(), "inbound_tag cannot be empty");
		ensure!(config.xray.public_port.is_some_and(|port| port > 0), "invalid public port");
		ensure!(config.xray.client_stream_settings.is_object(), "client_stream_settings must be an object");
		ensure!([config.token_ttl_seconds, config.session_ttl_seconds, config.heartbeat_timeout_seconds,
			config.connect_grace_seconds, config.offline_grace_seconds].iter().all(|v| (10..=31_536_000).contains(v)), "timeouts must be 10..31536000 seconds");
		ensure!((1..=60).contains(&config.cleanup_interval_seconds), "cleanup interval must be 1..60 seconds");
		Ok(config)
	}

	pub fn xray_json(&self) -> anyhow::Result<String> {
		let mut value = self.xray.server.clone();
		ensure!(value.is_object(), "Xray config must be an object");
		let inbound = value.get_mut("inbounds").and_then(Value::as_array_mut)
			.context("Xray config requires inbounds")?.iter_mut()
			.find(|v| v["tag"].as_str() == Some(&self.xray.inbound_tag))
			.context("configured inbound_tag does not exist")?;
		ensure!(inbound["protocol"].as_str() == Some(&self.xray.protocol), "inbound protocol mismatch");
		ensure!(inbound["settings"]["clients"].as_array().is_some_and(|a| a.is_empty()),
			"managed inbound must start with settings.clients = []");
		value["api"] = json!({"tag":"manager-api", "listen":self.xray.api_listen.to_string(),
			"services":["HandlerService", "StatsService"]});
		value["stats"] = json!({});
		if value["policy"].is_null() { value["policy"] = json!({}); }
		ensure!(value["policy"].is_object(), "invalid Xray policy");
		if value["policy"]["levels"].is_null() { value["policy"]["levels"] = json!({}); }
		ensure!(value["policy"]["levels"].is_object(), "invalid Xray policy levels");
		if value["policy"]["levels"]["0"].is_null() { value["policy"]["levels"]["0"] = json!({}); }
		ensure!(value["policy"]["levels"]["0"].is_object(), "invalid level 0 policy");
		value["policy"]["levels"]["0"]["statsUserOnline"] = json!(true);
		Ok(serde_json::to_string(&value)?)
	}

	pub fn client_config(&self, credential: &str) -> Value {
		let x = &self.xray;
		let mut settings = if x.protocol == "vless" {
			json!({"vnext":[{"port":x.public_port,
				"users":[{"id":credential,"encryption":"none","flow":x.flow}]}]})
		} else {
			json!({"servers":[{"port":x.public_port,"password":credential}]})
		};
		// When omitted, the frontend must insert the endpoint before starting Xray.
		if let Some(address) = &x.public_address {
			let list = if x.protocol == "vless" { "vnext" } else { "servers" };
			settings[list][0]["address"] = json!(address);
		}
		json!({"inbounds":[{"tag":"socks","listen":"127.0.0.1","port":10808,"protocol":"socks",
			"settings":{"auth":"noauth","udp":true}}],
			"outbounds":[{"tag":"proxy","protocol":x.protocol,"settings":settings,"streamSettings":x.client_stream_settings}]})
	}
}
