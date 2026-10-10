use std::{ffi::{CStr, CString, c_char, c_int}, mem::ManuallyDrop, path::Path, time::Duration};
use anyhow::Context;
use libloading::Library;
use prost::Message;
use tonic::{Code, client::Grpc, codec::ProstCodec, transport::{Channel, Endpoint}};
use crate::config::XrayConfig;

// ABI from the supplied xray-shared-windows-amd64/xray.h.
// Go shared libraries cannot safely be unloaded.
pub struct Runtime {
	_library: ManuallyDrop<Library>,
	stop: unsafe extern "C" fn() -> c_int,
	last_error: unsafe extern "C" fn() -> *mut c_char,
	free: unsafe extern "C" fn(*mut c_char),
	started: bool,
}

impl Runtime {
	pub fn start(path: &Path, config: &str) -> anyhow::Result<Self> {
		let config = CString::new(config)?;
		let absolute = std::path::absolute(path)?;
		let path = absolute.canonicalize().with_context(|| format!("Xray library file does not exist: {}", absolute.display()))?;
		// SAFETY: signatures match the supplied C header. Only this owner starts/stops
		// the library, and reads the shared error immediately after a failed operation.
		unsafe {
			let library = ManuallyDrop::new(Library::new(path).context("cannot load manually supplied Xray library; check OS and CPU architecture")?);
			let start = *library.get::<unsafe extern "C" fn(*const c_char) -> c_int>(b"XrayStart\0").context("missing XrayStart")?;
			let stop = *library.get::<unsafe extern "C" fn() -> c_int>(b"XrayStop\0").context("missing XrayStop")?;
			let last_error = *library.get::<unsafe extern "C" fn() -> *mut c_char>(b"XrayLastError\0").context("missing XrayLastError")?;
			let free = *library.get::<unsafe extern "C" fn(*mut c_char)>(b"XrayFreeString\0").context("missing XrayFreeString")?;
			let mut runtime = Self { _library: library, stop, last_error, free, started: false };
			if start(config.as_ptr()) != 0 {
				anyhow::bail!("Xray start failed: {}", runtime.error_message());
			}
			runtime.started = true;
			Ok(runtime)
		}
	}

	fn error_message(&self) -> String {
		// SAFETY: C owns this NUL-terminated copy; free with the same DLL allocator.
		unsafe {
			let pointer = (self.last_error)();
			if pointer.is_null() { return "unknown Xray error".into(); }
			let error = CStr::from_ptr(pointer).to_string_lossy().into_owned();
			(self.free)(pointer);
			error
		}
	}
}

impl Drop for Runtime {
	fn drop(&mut self) {
		if !self.started { return; }
		// SAFETY: function pointers stay valid because the library remains loaded.
		unsafe {
			if (self.stop)() != 0 {
				tracing::error!("Xray stop failed: {}", self.error_message());
			}
		}
	}
}

// Wire-compatible subset of the Xray .proto files; no system protoc required.
#[derive(Clone, PartialEq, Message)]
struct TypedMessage {
	#[prost(string, tag="1")] kind: String,
	#[prost(bytes="vec", tag="2")] value: Vec<u8>,
}
fn typed(kind: &str, message: impl Message) -> Option<TypedMessage> {
	Some(TypedMessage { kind: kind.into(), value: message.encode_to_vec() })
}
#[derive(Clone, PartialEq, Message)]
struct User {
	#[prost(uint32, tag="1")] level: u32,
	#[prost(string, tag="2")] email: String,
	#[prost(message, optional, tag="3")] account: Option<TypedMessage>,
}
#[derive(Clone, PartialEq, Message)]
struct AddUser { #[prost(message, optional, tag="1")] user: Option<User> }
#[derive(Clone, PartialEq, Message)]
struct RemoveUser { #[prost(string, tag="1")] email: String }
#[derive(Clone, PartialEq, Message)]
struct AlterInbound {
	#[prost(string, tag="1")] tag: String,
	#[prost(message, optional, tag="2")] operation: Option<TypedMessage>,
}
#[derive(Clone, PartialEq, Message)]
struct Account {
	#[prost(string, tag="1")] credential: String,
	#[prost(string, tag="2")] flow: String,
}
#[derive(Clone, PartialEq, Message)]
struct Empty {}
#[derive(Clone, PartialEq, Message)]
struct GetUsers {
	#[prost(string, tag="1")] tag: String,
	#[prost(string, tag="2")] email: String,
}
#[derive(Clone, PartialEq, Message)]
struct Users { #[prost(message, repeated, tag="1")] users: Vec<User> }
#[derive(Clone, PartialEq, Message)]
struct GetStats {
	#[prost(string, tag="1")] name: String,
	#[prost(bool, tag="2")] reset: bool,
}
#[derive(Clone, PartialEq, Message)]
struct Stat {
	#[prost(string, tag="1")] name: String,
	#[prost(int64, tag="2")] value: i64,
}
#[derive(Clone, PartialEq, Message)]
struct Stats { #[prost(message, optional, tag="1")] stat: Option<Stat> }

#[derive(Clone)]
pub struct Xray {
	channel: Channel,
	config: XrayConfig,
}

impl Xray {
	pub async fn connect(config: &XrayConfig) -> anyhow::Result<Self> {
		let channel = Endpoint::from_shared(format!("http://{}", config.api_listen))?
			.connect_timeout(Duration::from_secs(5)).timeout(Duration::from_secs(5)).connect().await?;
		let result = Self { channel, config: config.clone() };
		let _: Users = result.rpc("/xray.app.proxyman.command.HandlerService/GetInboundUsers",
			GetUsers { tag: config.inbound_tag.clone(), email: String::new() }).await?;
		result.online("manager-capability-probe").await.context("Xray must support GetStatsOnline")?;
		Ok(result)
	}

	async fn rpc<Req, Res>(&self, path: &'static str, request: Req) -> Result<Res, tonic::Status>
	where Req: Message + Default + Send + Sync + 'static, Res: Message + Default + Send + Sync + 'static {
		let mut client = Grpc::new(self.channel.clone());
		client.ready().await.map_err(|e| tonic::Status::unavailable(e.to_string()))?;
		let result = client.unary(tonic::Request::new(request),
			tonic::codegen::http::uri::PathAndQuery::from_static(path), ProstCodec::<Req, Res>::default()).await?;
		Ok(result.into_inner())
	}

	pub async fn add(&self, email: &str, credential: &str) -> anyhow::Result<()> {
		let kind = if self.config.protocol == "vless" { "xray.proxy.vless.Account" } else { "xray.proxy.trojan.Account" };
		let _: Empty = self.rpc("/xray.app.proxyman.command.HandlerService/AlterInbound", AlterInbound {
			tag: self.config.inbound_tag.clone(),
			operation: typed("xray.app.proxyman.command.AddUserOperation", AddUser {
				user: Some(User { email: email.into(), level: 0,
					account: typed(kind, Account { credential: credential.into(), flow: self.config.flow.clone() }) }),
			}),
		}).await?;
		Ok(())
	}

	pub async fn remove(&self, email: &str) -> anyhow::Result<()> {
		// Query all users: some Xray versions cannot serialize an absent single user.
		let users: Users = self.rpc("/xray.app.proxyman.command.HandlerService/GetInboundUsers",
			GetUsers { tag: self.config.inbound_tag.clone(), email: String::new() }).await?;
		if !users.users.iter().any(|u| u.email == email) { return Ok(()); }
		let _: Empty = self.rpc("/xray.app.proxyman.command.HandlerService/AlterInbound", AlterInbound {
			tag: self.config.inbound_tag.clone(),
			operation: typed("xray.app.proxyman.command.RemoveUserOperation", RemoveUser { email: email.into() }),
		}).await?;
		Ok(())
	}

	pub async fn online(&self, email: &str) -> anyhow::Result<i64> {
		let result: Result<Stats, _> = self.rpc("/xray.app.stats.command.StatsService/GetStatsOnline",
			GetStats { name: format!("user>>>{email}>>>online"), reset: false }).await;
		match result {
			Ok(stats) => Ok(stats.stat.context("missing online statistic")?.value),
			Err(e) if e.code() == Code::NotFound => Ok(0),
			Err(e) => Err(e.into()),
		}
	}
}
