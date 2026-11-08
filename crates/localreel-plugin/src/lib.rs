//! Sidecar plugins for LocalReel.
//!
//! A plugin package is a zip with a `manifest.json` at its root. The manifest is
//! validated before extraction or execution; the host then starts the entry
//! program as a child process and talks to it over loopback HTTP. Storage
//! plugins implement the [`localreel_kernel::storage::Storage`] contract through
//! [`PluginStorage`].
//!
//! [`PluginHost`] owns the configured set and supervises it: failures become
//! reported states, crashes are retried with backoff, stops drain in-flight
//! requests before the process is killed, and the child's process tree dies with
//! the host.

pub mod audit;
pub mod broker;
pub mod broker_client;
pub mod capability;
mod gate;
pub mod grants;
pub mod host;
pub mod http;
pub mod manifest;
pub mod package;
mod platform;
pub mod runtime;
pub mod server;
pub mod storage;
mod supervisor;

pub use audit::{AuditEntry, AuditEvent, AuditLog};
pub use broker::{BrokerError, BrokerRequest, BrokerResponse};
pub use broker_client::{BrokerClient, BrokerReply};
pub use capability::{
    scope_digest, CapabilityDescriptor, CapabilityRequest, NetFetchScope, RequestedCapability,
    CAPABILITIES,
};
pub use gate::Gate;
pub use grants::{CapabilityGrant, GrantStore};
pub use host::{
    CapabilityReport, PluginDescriptor, PluginFailure, PluginHost, PluginSpec, PluginState,
    ProxyResponse,
};
pub use manifest::{
    Manifest, PluginError, PluginKind, API_VERSION, PERMISSION_NETWORK, PERMISSION_STORAGE_READ,
};
pub use package::{extract, inspect};
pub use runtime::{PluginRuntime, PluginSandbox};
pub use storage::PluginStorage;
pub use supervisor::ProviderSink;
