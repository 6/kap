// Public API: only `config` and `container` are stable.
// All other modules are #[doc(hidden)] — visible to the binary crate
// but not part of the public API for downstream consumers.

#[doc(hidden)]
pub mod check;
#[doc(hidden)]
pub mod cli;
pub mod config;
#[doc(hidden)]
pub mod connect_proxy;
pub mod container;
#[doc(hidden)]
pub mod dev;
#[doc(hidden)]
pub mod init;
#[doc(hidden)]
pub mod init_env;
#[doc(hidden)]
pub mod mcp;
#[doc(hidden)]
pub mod mcp_cmd;
#[doc(hidden)]
pub mod proxy;
#[doc(hidden)]
pub mod reload;
#[doc(hidden)]
pub mod remote;
#[doc(hidden)]
pub mod status;
#[doc(hidden)]
pub mod upgrade;
