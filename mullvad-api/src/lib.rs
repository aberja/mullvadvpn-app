#![allow(rustdoc::private_intra_doc_links)]
use async_trait::async_trait;
#[cfg(target_os = "android")]
use futures::channel::mpsc;
use hyper::body::Incoming;
use mullvad_types::account::{AccountData, AccountNumber, VoucherSubmission};
#[cfg(target_os = "android")]
use mullvad_types::account::{PlayPurchase, PlayPurchasePaymentToken};
use proxy::{ApiConnectionMode, ConnectionModeProvider};
use std::{collections::BTreeMap, future::Future, io, net::SocketAddr, path::Path, sync::Arc};
use talpid_types::ErrorExt;

pub mod availability;
use availability::ApiAvailability;
pub mod rest;
#[cfg(not(target_os = "ios"))]
pub mod version;

mod abortable_stream;
pub mod access_mode;
mod https_client_with_sni;
pub mod proxy;
mod tls_stream;
#[cfg(target_os = "android")]
pub use crate::https_client_with_sni::SocketBypassRequest;

mod access;
mod address_cache;
pub mod device;
mod relay_list;

pub mod ffi;

pub use address_cache::AddressCache;
pub use device::DevicesProxy;
pub use hyper::StatusCode;
pub use relay_list::RelayListProxy;

/// Error code returned by the Mullvad API if the voucher has alreaby been used.
pub const VOUCHER_USED: &str = "VOUCHER_USED";

/// Error code returned by the Mullvad API if the voucher code is invalid.
pub const INVALID_VOUCHER: &str = "INVALID_VOUCHER";

/// Error code returned by the Mullvad API if the account number is invalid.
pub const INVALID_ACCOUNT: &str = "INVALID_ACCOUNT";

/// Error code returned by the Mullvad API if the device does not exist.
pub const DEVICE_NOT_FOUND: &str = "DEVICE_NOT_FOUND";

/// Error code returned by the Mullvad API if the access token is invalid.
pub const INVALID_ACCESS_TOKEN: &str = "INVALID_ACCESS_TOKEN";

pub const MAX_DEVICES_REACHED: &str = "MAX_DEVICES_REACHED";
pub const PUBKEY_IN_USE: &str = "PUBKEY_IN_USE";

pub const API_IP_CACHE_FILENAME: &str = "api-ip-address.txt";

const ACCOUNTS_URL_PREFIX: &str = "accounts/v1";
const APP_URL_PREFIX: &str = "app/v1";

#[cfg(target_os = "ios")]
const APPLE_PAYMENT_URL_PREFIX: &str = "payments/apple/v2";

#[cfg(target_os = "android")]
const GOOGLE_PAYMENTS_URL_PREFIX: &str = "payments/google-play/v1";

use mullvad_api_constants::*;

/// A hostname and socketaddr to reach the Mullvad REST API over.
#[derive(Debug, Clone)]
pub struct ApiEndpoint {
    /// An overriden API hostname. Initialized with the value of the environment
    /// variable `MULLVAD_API_HOST` if it has been set.
    ///
    /// Use the associated function [`Self::host`] to read this value with a
    /// default fallback if `MULLVAD_API_HOST` was not set.
    pub host: Option<String>,
    /// An overriden API address. Initialized with the value of the environment
    /// variable `MULLVAD_API_ADDR` if it has been set.
    ///
    /// Use the associated function [`Self::address()`] to read this value with
    /// a default fallback if `MULLVAD_API_ADDR` was not set.
    ///
    /// # Note
    ///
    /// If [`Self::address`] is populated with [`Some(SocketAddr)`], it should
    /// always be respected when establishing API connections.
    pub address: Option<SocketAddr>,
    #[cfg(any(feature = "api-override", test))]
    pub disable_tls: bool,
    #[cfg(feature = "api-override")]
    /// Whether bridges/proxies can be used to access the API or not. This is
    /// useful primarily for testing purposes.
    ///
    /// * If `force_direct` is `true`, bridges and proxies will not be used to reach the API.
    /// * If `force_direct` is `false`, bridges and proxies can be used to reach the API.
    ///
    /// # Note
    ///
    /// By default, `force_direct` will be `true` if the `api-override` feature
    /// is enabled and overrides are in use. This is supposedly less error prone, as
    /// common targets such as Devmole might be unreachable from behind a bridge server.
    ///
    /// To disable `force_direct`, set the environment variable
    /// `MULLVAD_API_FORCE_DIRECT=0` before starting the daemon.
    pub force_direct: bool,
}

impl ApiEndpoint {
    /// Returns the endpoint to connect to the API over.
    ///
    /// # Panics
    ///
    /// Panics if `MULLVAD_API_ADDR`, `MULLVAD_API_HOST` or
    /// `MULLVAD_API_DISABLE_TLS` has invalid contents.
    #[cfg(feature = "api-override")]
    pub fn from_env_vars() -> ApiEndpoint {
        let host_var = Self::read_var(env::API_HOST_VAR);
        let address_var = Self::read_var(env::API_ADDR_VAR);
        let disable_tls_var = Self::read_var(env::DISABLE_TLS_VAR);
        let force_direct = Self::read_var(env::API_FORCE_DIRECT_VAR);

        let mut api = ApiEndpoint {
            host: None,
            address: None,
            disable_tls: false,
            force_direct: force_direct
                .map(|force_direct| force_direct != "0")
                .unwrap_or_else(|| host_var.is_some() || address_var.is_some()),
        };

        match (host_var, address_var) {
            (None, None) => {}
            (Some(host), None) => {
                use std::net::ToSocketAddrs;
                log::debug!(
                    "{api_addr} not found. Resolving API IP address from {api_host}={host}",
                    api_addr = env::API_ADDR_VAR,
                    api_host = env::API_HOST_VAR
                );
                api.address = format!("{}:{}", host, API_PORT_DEFAULT)
                    .to_socket_addrs()
                    .unwrap_or_else(|_| {
                        panic!(
                            "Unable to resolve API IP address from host {host}:{port}",
                            port = API_PORT_DEFAULT,
                        )
                    })
                    .next();
                api.host = Some(host);
            }
            (host, Some(address)) => {
                let addr = address.parse().unwrap_or_else(|_| {
                    panic!(
                        "{api_addr}={address} is not a valid socketaddr",
                        api_addr = env::API_ADDR_VAR,
                    )
                });
                api.address = Some(addr);
                api.host = host;
            }
        }

        if api.host.is_none() && api.address.is_none() {
            if disable_tls_var.is_some() {
                log::warn!(
                    "{disable_tls} is ignored since {api_host} and {api_addr} are not set",
                    disable_tls = env::DISABLE_TLS_VAR,
                    api_host = env::API_HOST_VAR,
                    api_addr = env::API_ADDR_VAR,
                );
            }
        } else {
            api.disable_tls = disable_tls_var
                .as_ref()
                .map(|disable_tls| disable_tls != "0")
                .unwrap_or(api.disable_tls);

            log::debug!(
                "Overriding API. Using {host} at {scheme}{addr} (force direct={direct})",
                host = api.host(),
                addr = api.address(),
                scheme = if api.disable_tls {
                    "http://"
                } else {
                    "https://"
                },
                direct = api.force_direct,
            );
        }
        api
    }

    #[cfg(feature = "api-override")]
    pub fn should_disable_address_cache(&self) -> bool {
        self.host.is_some() || self.address.is_some()
    }

    /// Returns the endpoint to connect to the API over.
    ///
    /// # Panics
    ///
    /// Panics if `MULLVAD_API_ADDR`, `MULLVAD_API_HOST` or
    /// `MULLVAD_API_DISABLE_TLS` has invalid contents.
    #[cfg(not(feature = "api-override"))]
    pub fn from_env_vars() -> ApiEndpoint {
        let env_vars = [
            env::API_HOST_VAR,
            env::API_ADDR_VAR,
            env::DISABLE_TLS_VAR,
            env::API_FORCE_DIRECT_VAR,
        ];

        if env_vars.map(Self::read_var).iter().any(Option::is_some) {
            log::warn!(
                "These variables are ignored in production builds: {env_vars_pretty}",
                env_vars_pretty = env_vars.join(", ")
            );
        }

        ApiEndpoint {
            host: None,
            address: None,
            #[cfg(test)]
            disable_tls: false,
        }
    }

    /// Returns a new API endpoint with the given host and socket address.
    pub fn new(
        host: String,
        address: SocketAddr,
        #[cfg(any(feature = "api-override", test))] disable_tls: bool,
    ) -> Self {
        Self {
            host: Some(host),
            address: Some(address),
            #[cfg(any(feature = "api-override", test))]
            disable_tls,
            #[cfg(feature = "api-override")]
            force_direct: false,
        }
    }

    pub fn set_addr(&mut self, address: SocketAddr) {
        self.address = Some(address);
    }

    /// Read the [`Self::host`] value, falling back to
    /// [`API_HOST_DEFAULT`] as default value if it does not exist.
    pub fn host(&self) -> &str {
        self.host.as_deref().unwrap_or(API_HOST_DEFAULT)
    }

    /// Read the [`Self::address`] value, falling back to
    /// [`Self::API_IP_DEFAULT`] as default value if it does not exist.
    pub fn address(&self) -> SocketAddr {
        self.address
            .unwrap_or(SocketAddr::new(API_IP_DEFAULT, API_PORT_DEFAULT))
    }

    /// Try to read the value of an environment variable. Returns `None` if the
    /// environment variable has not been set.
    ///
    /// # Panics
    ///
    /// Panics if the environment variable was found, but it did not contain
    /// valid unicode data.
    fn read_var(key: &'static str) -> Option<String> {
        use std::env;
        match env::var(key) {
            Ok(v) => Some(v),
            Err(env::VarError::NotPresent) => None,
            Err(env::VarError::NotUnicode(_)) => panic!("{key} does not contain valid UTF-8"),
        }
    }
}

#[async_trait]
pub trait DnsResolver: 'static + Send + Sync {
    async fn resolve(&self, host: String) -> io::Result<Vec<SocketAddr>>;
}

/// DNS resolver that relies on `ToSocketAddrs` (`getaddrinfo`).
pub struct DefaultDnsResolver;

#[async_trait]
impl DnsResolver for DefaultDnsResolver {
    async fn resolve(&self, host: String) -> io::Result<Vec<SocketAddr>> {
        use std::net::ToSocketAddrs;
        // Spawn a blocking thread, since `to_socket_addrs` relies on `libc::getaddrinfo`, which
        // blocks and either has no timeout or a very long one.
        let addrs = tokio::task::spawn_blocking(move || (host, 0).to_socket_addrs())
            .await
            .expect("DNS task panicked")?;
        Ok(addrs.collect())
    }
}

/// DNS resolver that always returns no results
pub struct NullDnsResolver;

#[async_trait]
impl DnsResolver for NullDnsResolver {
    async fn resolve(&self, _host: String) -> io::Result<Vec<SocketAddr>> {
        Ok(vec![])
    }
}

/// A type that helps with the creation of API connections.
pub struct Runtime {
    handle: tokio::runtime::Handle,
    address_cache: AddressCache,
    api_availability: availability::ApiAvailability,
    endpoint: ApiEndpoint,
    #[cfg(target_os = "android")]
    socket_bypass_tx: Option<mpsc::Sender<SocketBypassRequest>>,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Failed to construct a rest client")]
    RestError(#[from] rest::Error),

    #[error("Failed to load address cache")]
    AddressCacheError(#[from] address_cache::Error),

    #[error("API availability check failed")]
    ApiCheckError(#[from] availability::Error),

    #[error("DNS resolution error")]
    ResolutionFailed(#[from] std::io::Error),
}

impl Runtime {
    /// Will create a new Runtime without a cache with the provided API endpoint.
    pub fn new(
        handle: tokio::runtime::Handle,
        endpoint: &ApiEndpoint,
        #[cfg(target_os = "android")] socket_bypass_tx: Option<mpsc::Sender<SocketBypassRequest>>,
    ) -> Self {
        Runtime {
            handle,
            address_cache: AddressCache::new(endpoint, None),
            api_availability: ApiAvailability::default(),
            endpoint: endpoint.clone(),
            #[cfg(target_os = "android")]
            socket_bypass_tx,
        }
    }

    /// Create a new `Runtime` using the specified directories.
    /// Try to use the cache directory first, and fall back on the bundled address otherwise.
    /// Will try to construct an API endpoint from the environment.
    pub async fn with_cache(
        endpoint: &ApiEndpoint,
        cache_dir: &Path,
        write_changes: bool,
        #[cfg(target_os = "android")] socket_bypass_tx: Option<mpsc::Sender<SocketBypassRequest>>,
    ) -> Result<Self, Error> {
        let handle = tokio::runtime::Handle::current();

        #[cfg(feature = "api-override")]
        if endpoint.should_disable_address_cache() {
            return Ok(Self::new(
                handle,
                endpoint,
                #[cfg(target_os = "android")]
                socket_bypass_tx,
            ));
        }

        let cache_file = cache_dir.join(API_IP_CACHE_FILENAME);
        let write_file = if write_changes {
            Some(cache_file.clone().into_boxed_path())
        } else {
            None
        };

        let address_cache = match AddressCache::from_file(
            &cache_file,
            write_file.clone(),
            endpoint.host().to_owned(),
        )
        .await
        {
            Ok(cache) => cache,
            Err(error) => {
                if cache_file.exists() {
                    log::error!(
                        "{}",
                        error.display_chain_with_msg(
                            "Failed to load cached API addresses. Falling back on bundled address"
                        )
                    );
                }
                AddressCache::new(endpoint, write_file)
            }
        };

        let api_availability = ApiAvailability::default();

        Ok(Runtime {
            handle,
            address_cache,
            api_availability,
            endpoint: endpoint.clone(),
            #[cfg(target_os = "android")]
            socket_bypass_tx,
        })
    }

    /// Returns a request factory initialized to create requests for the master API Assumes an API
    /// endpoint that is constructed from env vars, or uses default values.
    pub fn mullvad_rest_handle<T: ConnectionModeProvider + 'static>(
        &self,
        connection_mode_provider: T,
    ) -> rest::MullvadRestHandle {
        let service = self.new_request_service(
            connection_mode_provider,
            Arc::new(self.address_cache.clone()),
            #[cfg(target_os = "android")]
            self.socket_bypass_tx.clone(),
            #[cfg(any(feature = "api-override", test))]
            self.endpoint.disable_tls,
        );
        let hostname = self.endpoint.host().to_owned();
        let token_store = access::AccessTokenStore::new(service.clone(), hostname.clone());
        let factory = rest::RequestFactory::new(hostname, Some(token_store));

        rest::MullvadRestHandle::new(service, factory, self.availability_handle())
    }

    /// Returns a new request service handle
    pub fn rest_handle(&self, dns_resolver: impl DnsResolver) -> rest::RequestServiceHandle {
        self.new_request_service(
            ApiConnectionMode::Direct.into_provider(),
            Arc::new(dns_resolver),
            #[cfg(target_os = "android")]
            None,
            #[cfg(any(feature = "api-override", test))]
            false,
        )
    }

    /// Creates a new request service and returns a handle to it.
    fn new_request_service<T: ConnectionModeProvider + 'static>(
        &self,
        connection_mode_provider: T,
        dns_resolver: Arc<dyn DnsResolver>,
        #[cfg(target_os = "android")] socket_bypass_tx: Option<mpsc::Sender<SocketBypassRequest>>,
        #[cfg(any(feature = "api-override", test))] disable_tls: bool,
    ) -> rest::RequestServiceHandle {
        rest::RequestService::spawn(
            self.api_availability.clone(),
            connection_mode_provider,
            dns_resolver,
            #[cfg(target_os = "android")]
            socket_bypass_tx,
            #[cfg(any(feature = "api-override", test))]
            disable_tls,
        )
    }

    pub fn handle(&self) -> &tokio::runtime::Handle {
        &self.handle
    }

    pub fn availability_handle(&self) -> ApiAvailability {
        self.api_availability.clone()
    }

    pub fn address_cache(&self) -> &AddressCache {
        &self.address_cache
    }
}

#[derive(Clone)]
pub struct AccountsProxy {
    handle: rest::MullvadRestHandle,
}

impl AccountsProxy {
    pub fn new(handle: rest::MullvadRestHandle) -> Self {
        Self { handle }
    }

    pub fn get_data(
        &self,
        account: AccountNumber,
    ) -> impl Future<Output = Result<AccountData, rest::Error>> + use<> {
        let request = self.get_data_response(account);

        async move { request.await?.deserialize().await }
    }

    pub fn get_data_response(
        &self,
        account: AccountNumber,
    ) -> impl Future<Output = Result<rest::Response<Incoming>, rest::Error>> + use<> {
        let service = self.handle.service.clone();
        let factory = self.handle.factory.clone();

        async move {
            let request = factory
                .get(&format!("{ACCOUNTS_URL_PREFIX}/accounts/me"))?
                .expected_status(&[StatusCode::OK])
                .account(account)?;
            service.request(request).await
        }
    }

    pub fn create_account(
        &self,
    ) -> impl Future<Output = Result<AccountNumber, rest::Error>> + use<> {
        #[derive(serde::Deserialize)]
        struct AccountCreationResponse {
            number: AccountNumber,
        }

        let request = self.create_account_response();

        async move {
            let account: AccountCreationResponse = request.await?.deserialize().await?;
            Ok(account.number)
        }
    }

    pub fn create_account_response(
        &self,
    ) -> impl Future<Output = Result<rest::Response<Incoming>, rest::Error>> + use<> {
        let service = self.handle.service.clone();
        let factory = self.handle.factory.clone();

        async move {
            let request = factory
                .post(&format!("{ACCOUNTS_URL_PREFIX}/accounts"))?
                .expected_status(&[StatusCode::CREATED]);
            service.request(request).await
        }
    }

    pub fn submit_voucher(
        &self,
        account: AccountNumber,
        voucher_code: String,
    ) -> impl Future<Output = Result<VoucherSubmission, rest::Error>> + use<> {
        #[derive(serde::Serialize)]
        struct VoucherSubmission {
            voucher_code: String,
        }

        let service = self.handle.service.clone();
        let factory = self.handle.factory.clone();
        let submission = VoucherSubmission { voucher_code };

        async move {
            let request = factory
                .post_json(&format!("{APP_URL_PREFIX}/submit-voucher"), &submission)?
                .account(account)?
                .expected_status(&[StatusCode::OK]);
            service.request(request).await?.deserialize().await
        }
    }

    pub fn delete_account(
        &self,
        account: AccountNumber,
    ) -> impl Future<Output = Result<(), rest::Error>> + use<> {
        let service = self.handle.service.clone();
        let factory = self.handle.factory.clone();

        async move {
            let request = factory
                .delete(&format!("{ACCOUNTS_URL_PREFIX}/accounts/me"))?
                .account(account.clone())?
                .header("Mullvad-Account-Number", &account)?
                .expected_status(&[StatusCode::NO_CONTENT]);

            let _ = service.request(request).await?;
            Ok(())
        }
    }

    #[cfg(target_os = "ios")]
    pub async fn legacy_storekit_payment(
        &self,
        account: AccountNumber,
        body: Vec<u8>,
    ) -> Result<rest::Response<Incoming>, rest::Error> {
        let request = self
            .handle
            .factory
            .post_json_bytes(&format!("{APP_URL_PREFIX}/create-apple-payment"), body)?
            .expected_status(&[StatusCode::OK])
            .account(account)?;
        self.handle.service.request(request).await
    }

    #[cfg(target_os = "ios")]
    pub async fn init_storekit_payment(
        &self,
        account: AccountNumber,
    ) -> Result<rest::Response<Incoming>, rest::Error> {
        let request = self
            .handle
            .factory
            .post(&format!("{APPLE_PAYMENT_URL_PREFIX}/init"))?
            .expected_status(&[StatusCode::OK])
            .account(account)?;
        self.handle.service.request(request).await
    }

    #[cfg(target_os = "ios")]
    pub async fn check_storekit_payment(
        &self,
        account: AccountNumber,
        body: Vec<u8>,
    ) -> Result<rest::Response<Incoming>, rest::Error> {
        let request = self
            .handle
            .factory
            .post_json_bytes(&format!("{APPLE_PAYMENT_URL_PREFIX}/check"), body)?
            .expected_status(&[StatusCode::OK])
            .account(account)?;
        self.handle.service.request(request).await
    }

    #[cfg(target_os = "android")]
    pub fn init_play_purchase(
        &mut self,
        account: AccountNumber,
    ) -> impl Future<Output = Result<PlayPurchasePaymentToken, rest::Error>> + use<> {
        #[derive(serde::Deserialize)]
        struct PlayPurchaseInitResponse {
            obfuscated_id: String,
        }

        let service = self.handle.service.clone();
        let factory = self.handle.factory.clone();

        async move {
            let request = factory
                .post_json(&format!("{GOOGLE_PAYMENTS_URL_PREFIX}/init"), &())?
                .account(account)?
                .expected_status(&[StatusCode::OK]);
            let response = service.request(request).await?;

            let PlayPurchaseInitResponse { obfuscated_id } = response.deserialize().await?;

            Ok(obfuscated_id)
        }
    }

    #[cfg(target_os = "android")]
    pub fn verify_play_purchase(
        &mut self,
        account: AccountNumber,
        play_purchase: PlayPurchase,
    ) -> impl Future<Output = Result<(), rest::Error>> + use<> {
        let service = self.handle.service.clone();
        let factory = self.handle.factory.clone();

        async move {
            let request = factory
                .post_json(
                    &format!("{GOOGLE_PAYMENTS_URL_PREFIX}/acknowledge"),
                    &play_purchase,
                )?
                .account(account)?
                .expected_status(&[StatusCode::ACCEPTED]);
            service.request(request).await?;
            Ok(())
        }
    }

    pub fn get_www_auth_token(
        &self,
        account: AccountNumber,
    ) -> impl Future<Output = Result<String, rest::Error>> + use<> {
        #[derive(serde::Deserialize)]
        struct AuthTokenResponse {
            auth_token: String,
        }

        let service = self.handle.service.clone();
        let factory = self.handle.factory.clone();

        async move {
            let request = factory
                .post(&format!("{APP_URL_PREFIX}/www-auth-token"))?
                .account(account)?
                .expected_status(&[StatusCode::OK]);
            let response = service.request(request).await?;
            let response: AuthTokenResponse = response.deserialize().await?;
            Ok(response.auth_token)
        }
    }
}

pub struct ProblemReportProxy {
    handle: rest::MullvadRestHandle,
}

impl ProblemReportProxy {
    pub fn new(handle: rest::MullvadRestHandle) -> Self {
        Self { handle }
    }

    pub fn problem_report(
        &self,
        email: &str,
        message: &str,
        log: &str,
        metadata: &BTreeMap<String, String>,
    ) -> impl Future<Output = Result<(), rest::Error>> {
        #[derive(serde::Serialize)]
        struct ProblemReport {
            address: String,
            message: String,
            log: String,
            metadata: BTreeMap<String, String>,
        }

        let report = ProblemReport {
            address: email.to_owned(),
            message: message.to_owned(),
            log: log.to_owned(),
            metadata: metadata.clone(),
        };

        let service = self.handle.service.clone();
        let factory = self.handle.factory.clone();

        async move {
            let request = factory
                .post_json(&format!("{APP_URL_PREFIX}/problem-report"), &report)?
                .expected_status(&[StatusCode::NO_CONTENT]);
            service.request(request).await?;
            Ok(())
        }
    }
}

#[derive(Clone)]
pub struct ApiProxy {
    handle: rest::MullvadRestHandle,
}

impl ApiProxy {
    pub fn new(handle: rest::MullvadRestHandle) -> Self {
        Self { handle }
    }

    pub async fn get_api_addrs(&self) -> Result<Vec<SocketAddr>, rest::Error> {
        self.get_api_addrs_response().await?.deserialize().await
    }

    pub async fn get_api_addrs_response(&self) -> Result<rest::Response<Incoming>, rest::Error> {
        let request = self
            .handle
            .factory
            .get(&format!("{APP_URL_PREFIX}/api-addrs"))?
            .expected_status(&[StatusCode::OK]);

        self.handle.service.request(request).await
    }

    /// Check the availablility of `{APP_URL_PREFIX}/api-addrs`.
    pub async fn api_addrs_available(&self) -> Result<bool, rest::Error> {
        let request = self
            .handle
            .factory
            .head(&format!("{APP_URL_PREFIX}/api-addrs"))?
            .expected_status(&[StatusCode::OK]);

        let response = self.handle.service.request(request).await?;
        Ok(response.status().is_success())
    }
}
