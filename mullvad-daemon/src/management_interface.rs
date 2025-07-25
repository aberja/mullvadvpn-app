use crate::{account_history, device, DaemonCommand, DaemonCommandSender};
use futures::{
    channel::{mpsc, oneshot},
    StreamExt,
};
use mullvad_api::{rest::Error as RestError, StatusCode};
use mullvad_management_interface::types::FromProtobufTypeError;
use mullvad_management_interface::{
    types::{self, daemon_event, management_service_server::ManagementService},
    Code, Request, Response, ServerJoinHandle, Status,
};
use mullvad_types::relay_constraints::GeographicLocationConstraint;
use mullvad_types::{
    account::AccountNumber,
    relay_constraints::{
        allowed_ip::AllowedIps, BridgeSettings, BridgeState, ObfuscationSettings, RelayOverride,
        RelaySettings,
    },
    relay_list::RelayList,
    settings::{DnsOptions, Settings},
    states::{TargetState, TunnelState},
    version,
    wireguard::{RotationInterval, RotationIntervalError},
};
use std::collections::BTreeSet;
use std::{
    path::Path,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};
use talpid_types::ErrorExt;
use tokio::time::timeout;
use tokio_stream::wrappers::UnboundedReceiverStream;

const RPC_SERVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(thiserror::Error, Debug)]
pub enum Error {
    // Unable to start the management interface server
    #[error("Unable to start management interface server")]
    SetupError(#[source] mullvad_management_interface::Error),
}

pub type AppUpgradeBroadcast = tokio::sync::broadcast::Sender<version::AppUpgradeEvent>;

struct ManagementServiceImpl {
    daemon_tx: DaemonCommandSender,
    subscriptions: Arc<Mutex<Vec<EventsListenerSender>>>,
    pub app_upgrade_broadcast: AppUpgradeBroadcast,
}

pub type ServiceResult<T> = std::result::Result<Response<T>, Status>;
type EventsListenerReceiver = UnboundedReceiverStream<Result<types::DaemonEvent, Status>>;
type EventsListenerSender = tokio::sync::mpsc::UnboundedSender<Result<types::DaemonEvent, Status>>;

type AppUpgradeEventListenerReceiver =
    Box<dyn futures::Stream<Item = Result<types::AppUpgradeEvent, Status>> + Send + Unpin>;

const INVALID_VOUCHER_MESSAGE: &str = "This voucher code is invalid";
const USED_VOUCHER_MESSAGE: &str = "This voucher code has already been used";

#[mullvad_management_interface::async_trait]
impl ManagementService for ManagementServiceImpl {
    type GetSplitTunnelProcessesStream = UnboundedReceiverStream<Result<i32, Status>>;
    type EventsListenStream = EventsListenerReceiver;
    type AppUpgradeEventsListenStream = AppUpgradeEventListenerReceiver;

    // Control and get the tunnel state
    //

    async fn connect_tunnel(&self, _: Request<()>) -> ServiceResult<bool> {
        log::debug!("connect_tunnel");

        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetTargetState(tx, TargetState::Secured))?;
        let connect_issued = self.wait_for_result(rx).await?;
        Ok(Response::new(connect_issued))
    }

    async fn disconnect_tunnel(&self, _: Request<()>) -> ServiceResult<bool> {
        log::debug!("disconnect_tunnel");

        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetTargetState(tx, TargetState::Unsecured))?;
        let disconnect_issued = self.wait_for_result(rx).await?;
        Ok(Response::new(disconnect_issued))
    }

    async fn reconnect_tunnel(&self, _: Request<()>) -> ServiceResult<bool> {
        log::debug!("reconnect_tunnel");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::Reconnect(tx))?;
        let reconnect_issued = self.wait_for_result(rx).await?;
        Ok(Response::new(reconnect_issued))
    }

    async fn get_tunnel_state(&self, _: Request<()>) -> ServiceResult<types::TunnelState> {
        log::debug!("get_tunnel_state");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::GetState(tx))?;
        let state = self.wait_for_result(rx).await?;
        Ok(Response::new(types::TunnelState::from(state)))
    }

    // Control the daemon and receive events
    //

    async fn events_listen(&self, _: Request<()>) -> ServiceResult<Self::EventsListenStream> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        let mut subscriptions = self.subscriptions.lock().unwrap();
        subscriptions.push(tx);

        Ok(Response::new(UnboundedReceiverStream::new(rx)))
    }

    async fn prepare_restart(&self, _: Request<()>) -> ServiceResult<()> {
        log::debug!("prepare_restart");
        // Note: The old `PrepareRestart` behavior never shutdown the daemon.
        let shutdown = false;
        self.send_command_to_daemon(DaemonCommand::PrepareRestart(shutdown))?;
        Ok(Response::new(()))
    }

    async fn prepare_restart_v2(&self, shutdown: Request<bool>) -> ServiceResult<()> {
        log::debug!("prepare_restart_v2");
        self.send_command_to_daemon(DaemonCommand::PrepareRestart(shutdown.into_inner()))?;
        Ok(Response::new(()))
    }

    async fn factory_reset(&self, _: Request<()>) -> ServiceResult<()> {
        #[cfg(not(target_os = "android"))]
        {
            log::debug!("factory_reset");
            let (tx, rx) = oneshot::channel();
            self.send_command_to_daemon(DaemonCommand::FactoryReset(tx))?;
            self.wait_for_result(rx)
                .await?
                .map(Response::new)
                .map_err(map_daemon_error)
        }
        #[cfg(target_os = "android")]
        {
            Ok(Response::new(()))
        }
    }

    async fn get_current_version(&self, _: Request<()>) -> ServiceResult<String> {
        log::debug!("get_current_version");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::GetCurrentVersion(tx))?;
        let version = self.wait_for_result(rx).await?.to_string();
        Ok(Response::new(version))
    }

    async fn get_version_info(&self, _: Request<()>) -> ServiceResult<types::AppVersionInfo> {
        log::debug!("get_version_info");

        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::GetVersionInfo(tx))?;
        self.wait_for_result(rx)
            .await?
            .map(types::AppVersionInfo::from)
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    async fn is_performing_post_upgrade(&self, _: Request<()>) -> ServiceResult<bool> {
        log::debug!("is_performing_post_upgrade");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::IsPerformingPostUpgrade(tx))?;
        Ok(Response::new(self.wait_for_result(rx).await?))
    }

    // Relays and tunnel constraints
    //

    async fn update_relay_locations(&self, _: Request<()>) -> ServiceResult<()> {
        log::debug!("update_relay_locations");
        self.send_command_to_daemon(DaemonCommand::UpdateRelayLocations)?;
        Ok(Response::new(()))
    }

    async fn set_relay_settings(
        &self,
        request: Request<types::RelaySettings>,
    ) -> ServiceResult<()> {
        log::debug!("set_relay_settings");
        let (tx, rx) = oneshot::channel();
        let constraints_update =
            RelaySettings::try_from(request.into_inner()).map_err(map_protobuf_type_err)?;

        let message = DaemonCommand::SetRelaySettings(tx, constraints_update);
        self.send_command_to_daemon(message)?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    async fn get_relay_locations(&self, _: Request<()>) -> ServiceResult<types::RelayList> {
        log::debug!("get_relay_locations");

        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::GetRelayLocations(tx))?;
        self.wait_for_result(rx)
            .await
            .map(|relays| Response::new(types::RelayList::from(relays)))
    }

    async fn set_bridge_settings(
        &self,
        request: Request<types::BridgeSettings>,
    ) -> ServiceResult<()> {
        let settings =
            BridgeSettings::try_from(request.into_inner()).map_err(map_protobuf_type_err)?;

        log::debug!("set_bridge_settings({:?})", settings);

        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetBridgeSettings(tx, settings))?;
        self.wait_for_result(rx).await?.map_err(map_daemon_error)?;
        Ok(Response::new(()))
    }

    async fn set_obfuscation_settings(
        &self,
        request: Request<types::ObfuscationSettings>,
    ) -> ServiceResult<()> {
        let settings =
            ObfuscationSettings::try_from(request.into_inner()).map_err(map_protobuf_type_err)?;
        log::debug!("set_obfuscation_settings({:?})", settings);
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetObfuscationSettings(tx, settings))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    async fn set_bridge_state(&self, request: Request<types::BridgeState>) -> ServiceResult<()> {
        let bridge_state =
            BridgeState::try_from(request.into_inner()).map_err(map_protobuf_type_err)?;

        log::debug!("set_bridge_state({:?})", bridge_state);
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetBridgeState(tx, bridge_state))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    // Settings
    //

    async fn get_settings(&self, _: Request<()>) -> ServiceResult<types::Settings> {
        log::debug!("get_settings");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::GetSettings(tx))?;
        self.wait_for_result(rx)
            .await
            .map(|settings| Response::new(types::Settings::from(&settings)))
    }

    async fn reset_settings(&self, _: Request<()>) -> ServiceResult<()> {
        log::debug!("reset_settings");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::ResetSettings(tx))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    async fn set_allow_lan(&self, request: Request<bool>) -> ServiceResult<()> {
        let allow_lan = request.into_inner();
        log::debug!("set_allow_lan({})", allow_lan);
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetAllowLan(tx, allow_lan))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    async fn set_show_beta_releases(&self, request: Request<bool>) -> ServiceResult<()> {
        let enabled = request.into_inner();
        log::debug!("set_show_beta_releases({})", enabled);
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetShowBetaReleases(tx, enabled))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    #[cfg(not(target_os = "android"))]
    async fn set_block_when_disconnected(&self, request: Request<bool>) -> ServiceResult<()> {
        let block_when_disconnected = request.into_inner();
        log::debug!("set_block_when_disconnected({})", block_when_disconnected);
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetBlockWhenDisconnected(
            tx,
            block_when_disconnected,
        ))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    #[cfg(target_os = "android")]
    async fn set_block_when_disconnected(&self, request: Request<bool>) -> ServiceResult<()> {
        let block_when_disconnected = request.into_inner();
        log::debug!("set_block_when_disconnected({})", block_when_disconnected);
        Err(Status::unimplemented(
            "Setting Lockdown mode on Android is not supported - this is handled by the OS, not the daemon",
        ))
    }

    async fn set_auto_connect(&self, request: Request<bool>) -> ServiceResult<()> {
        let auto_connect = request.into_inner();
        log::debug!("set_auto_connect({})", auto_connect);
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetAutoConnect(tx, auto_connect))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    async fn set_openvpn_mssfix(&self, request: Request<u32>) -> ServiceResult<()> {
        let mssfix = request.into_inner();
        let mssfix = if mssfix != 0 {
            Some(mssfix as u16)
        } else {
            None
        };
        log::debug!("set_openvpn_mssfix({:?})", mssfix);
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetOpenVpnMssfix(tx, mssfix))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    async fn set_wireguard_mtu(&self, request: Request<u32>) -> ServiceResult<()> {
        let mtu = request.into_inner();
        let mtu = if mtu != 0 { Some(mtu as u16) } else { None };
        log::debug!("set_wireguard_mtu({:?})", mtu);
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetWireguardMtu(tx, mtu))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    async fn set_enable_ipv6(&self, request: Request<bool>) -> ServiceResult<()> {
        let enable_ipv6 = request.into_inner();
        log::debug!("set_enable_ipv6({})", enable_ipv6);
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetEnableIpv6(tx, enable_ipv6))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    async fn set_quantum_resistant_tunnel(
        &self,
        request: Request<types::QuantumResistantState>,
    ) -> ServiceResult<()> {
        let state = mullvad_types::wireguard::QuantumResistantState::try_from(request.into_inner())
            .map_err(map_protobuf_type_err)?;

        log::debug!("set_quantum_resistant_tunnel({state:?})");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetQuantumResistantTunnel(tx, state))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    #[cfg(daita)]
    async fn set_enable_daita(&self, request: Request<bool>) -> ServiceResult<()> {
        let daita_enabled = request.into_inner();
        log::debug!("set_enable_daita({daita_enabled})");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetEnableDaita(tx, daita_enabled))?;
        self.wait_for_result(rx).await?.map(Response::new)?;
        Ok(Response::new(()))
    }

    #[cfg(daita)]
    async fn set_daita_direct_only(&self, request: Request<bool>) -> ServiceResult<()> {
        let direct_only_enabled = request.into_inner();
        log::debug!("set_daita_direct_only({direct_only_enabled})");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetDaitaUseMultihopIfNecessary(
            tx,
            !direct_only_enabled,
        ))?;
        self.wait_for_result(rx).await?.map(Response::new)?;
        Ok(Response::new(()))
    }

    #[cfg(daita)]
    async fn set_daita_settings(
        &self,
        request: Request<types::DaitaSettings>,
    ) -> ServiceResult<()> {
        let state = mullvad_types::wireguard::DaitaSettings::from(request.into_inner());

        log::debug!("set_daita_settings({state:?})");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetDaitaSettings(tx, state))?;
        self.wait_for_result(rx).await?.map(Response::new)?;
        Ok(Response::new(()))
    }

    #[cfg(not(daita))]
    async fn set_enable_daita(&self, _: Request<bool>) -> ServiceResult<()> {
        Ok(Response::new(()))
    }

    #[cfg(not(daita))]
    async fn set_daita_direct_only(&self, _: Request<bool>) -> ServiceResult<()> {
        Ok(Response::new(()))
    }

    #[cfg(not(daita))]
    async fn set_daita_settings(&self, _: Request<types::DaitaSettings>) -> ServiceResult<()> {
        Ok(Response::new(()))
    }

    async fn set_dns_options(&self, request: Request<types::DnsOptions>) -> ServiceResult<()> {
        let options = DnsOptions::try_from(request.into_inner()).map_err(map_protobuf_type_err)?;
        log::debug!("set_dns_options({:?})", options);

        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetDnsOptions(tx, options))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    async fn set_relay_override(
        &self,
        request: Request<types::RelayOverride>,
    ) -> ServiceResult<()> {
        let relay_override =
            RelayOverride::try_from(request.into_inner()).map_err(map_protobuf_type_err)?;
        log::debug!("set_relay_override");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetRelayOverride(tx, relay_override))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    async fn clear_all_relay_overrides(&self, _: Request<()>) -> ServiceResult<()> {
        log::debug!("clear_all_relay_overrides");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::ClearAllRelayOverrides(tx))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    // Account management
    //

    async fn create_new_account(&self, _: Request<()>) -> ServiceResult<String> {
        log::debug!("create_new_account");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::CreateNewAccount(tx))?;
        self.wait_for_result(rx)
            .await?
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    async fn login_account(&self, request: Request<AccountNumber>) -> ServiceResult<()> {
        log::debug!("login_account");
        let account_number = request.into_inner();
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::LoginAccount(tx, account_number))?;
        self.wait_for_result(rx)
            .await?
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    async fn logout_account(&self, _: Request<()>) -> ServiceResult<()> {
        log::debug!("logout_account");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::LogoutAccount(tx))?;
        self.wait_for_result(rx)
            .await?
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    async fn get_account_data(
        &self,
        request: Request<AccountNumber>,
    ) -> ServiceResult<types::AccountData> {
        log::debug!("get_account_data");
        let account_number = request.into_inner();
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::GetAccountData(tx, account_number))?;
        let result = self.wait_for_result(rx).await?;
        result
            .map(|account_data| Response::new(types::AccountData::from(account_data)))
            .map_err(|error: RestError| {
                log::error!(
                    "Unable to get account data from API: {}",
                    error.display_chain()
                );
                map_rest_error(&error)
            })
    }

    async fn get_account_history(&self, _: Request<()>) -> ServiceResult<types::AccountHistory> {
        log::debug!("get_account_history");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::GetAccountHistory(tx))?;
        self.wait_for_result(rx)
            .await
            .map(|history| Response::new(types::AccountHistory { number: history }))
    }

    async fn clear_account_history(&self, _: Request<()>) -> ServiceResult<()> {
        log::debug!("clear_account_history");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::ClearAccountHistory(tx))?;
        self.wait_for_result(rx)
            .await?
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    async fn get_www_auth_token(&self, _: Request<()>) -> ServiceResult<String> {
        log::debug!("get_www_auth_token");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::GetWwwAuthToken(tx))?;
        let result = self.wait_for_result(rx).await?;
        result.map(Response::new).map_err(|error| {
            log::error!(
                "Unable to get account data from API: {}",
                error.display_chain()
            );
            map_daemon_error(error)
        })
    }

    async fn submit_voucher(
        &self,
        request: Request<String>,
    ) -> ServiceResult<types::VoucherSubmission> {
        log::debug!("submit_voucher");
        let voucher = request.into_inner();
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SubmitVoucher(tx, voucher))?;
        let result = self.wait_for_result(rx).await?;
        result
            .map(|submission| Response::new(types::VoucherSubmission::from(submission)))
            .map_err(map_daemon_error)
    }

    // Device management
    async fn get_device(&self, _: Request<()>) -> ServiceResult<types::DeviceState> {
        log::debug!("get_device");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::GetDevice(tx))?;
        let device = self.wait_for_result(rx).await?.map_err(map_daemon_error)?;
        Ok(Response::new(types::DeviceState::from(device)))
    }

    async fn update_device(&self, _: Request<()>) -> ServiceResult<()> {
        log::debug!("update_device");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::UpdateDevice(tx))?;
        self.wait_for_result(rx)
            .await?
            .map_err(map_daemon_error)
            .map(Response::new)
    }

    async fn list_devices(
        &self,
        request: Request<AccountNumber>,
    ) -> ServiceResult<types::DeviceList> {
        log::debug!("list_devices");
        let (tx, rx) = oneshot::channel();
        let token = request.into_inner();
        self.send_command_to_daemon(DaemonCommand::ListDevices(tx, token))?;
        let device = self.wait_for_result(rx).await?.map_err(map_daemon_error)?;
        Ok(Response::new(types::DeviceList::from(device)))
    }

    async fn remove_device(&self, request: Request<types::DeviceRemoval>) -> ServiceResult<()> {
        log::debug!("remove_device");
        let (tx, rx) = oneshot::channel();
        let removal = request.into_inner();
        self.send_command_to_daemon(DaemonCommand::RemoveDevice(
            tx,
            removal.account_number,
            removal.device_id,
        ))?;
        self.wait_for_result(rx).await?.map_err(map_daemon_error)?;
        Ok(Response::new(()))
    }

    // WireGuard key management
    //

    async fn set_wireguard_rotation_interval(
        &self,
        request: Request<types::Duration>,
    ) -> ServiceResult<()> {
        let interval: RotationInterval = Duration::try_from(request.into_inner())
            .map_err(|_| Status::invalid_argument("unexpected negative rotation interval"))?
            .try_into()
            .map_err(|error: RotationIntervalError| {
                Status::invalid_argument(error.display_chain())
            })?;

        log::debug!("set_wireguard_rotation_interval({:?})", interval);
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetWireguardRotationInterval(
            tx,
            Some(interval),
        ))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    async fn reset_wireguard_rotation_interval(&self, _: Request<()>) -> ServiceResult<()> {
        log::debug!("reset_wireguard_rotation_interval");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetWireguardRotationInterval(tx, None))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    async fn rotate_wireguard_key(&self, _: Request<()>) -> ServiceResult<()> {
        log::debug!("rotate_wireguard_key");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::RotateWireguardKey(tx))?;
        self.wait_for_result(rx)
            .await?
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    async fn get_wireguard_key(&self, _: Request<()>) -> ServiceResult<types::PublicKey> {
        log::debug!("get_wireguard_key");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::GetWireguardKey(tx))?;
        let key = self.wait_for_result(rx).await?.map_err(map_daemon_error)?;
        match key {
            Some(key) => Ok(Response::new(types::PublicKey::from(key))),
            None => Err(Status::not_found("no WireGuard key was found")),
        }
    }

    async fn set_wireguard_allowed_ips(
        &self,
        request: Request<types::AllowedIpsList>,
    ) -> ServiceResult<()> {
        let allowed_ips_str = request.into_inner().values;
        log::debug!("set_wireguard_allowed_ips({:?})", allowed_ips_str);

        let (tx, rx) = oneshot::channel();
        let allowed_ips = AllowedIps::parse(&allowed_ips_str)
            .map_err(|e| {
                log::error!("{e}");
                Status::invalid_argument(format!("Invalid allowed IPs: {}", e))
            })?
            .to_constraint();

        self.send_command_to_daemon(DaemonCommand::SetWireguardAllowedIps(tx, allowed_ips))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    // Custom lists
    //

    async fn create_custom_list(
        &self,
        request: Request<types::NewCustomList>,
    ) -> ServiceResult<String> {
        log::debug!("create_custom_list");
        let request = request.into_inner();
        let locations = request
            .locations
            .into_iter()
            .map(GeographicLocationConstraint::try_from)
            .collect::<Result<BTreeSet<_>, FromProtobufTypeError>>()?;
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::CreateCustomList(tx, request.name, locations))?;
        self.wait_for_result(rx)
            .await?
            .map(|id| Response::new(id.to_string()))
            .map_err(map_daemon_error)
    }

    async fn delete_custom_list(&self, request: Request<String>) -> ServiceResult<()> {
        log::debug!("delete_custom_list");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::DeleteCustomList(
            tx,
            mullvad_types::custom_list::Id::from_str(&request.into_inner())
                .map_err(|_| Status::invalid_argument("invalid ID"))?,
        ))?;
        self.wait_for_result(rx)
            .await?
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    async fn update_custom_list(&self, request: Request<types::CustomList>) -> ServiceResult<()> {
        log::debug!("update_custom_list");
        let custom_list = mullvad_types::custom_list::CustomList::try_from(request.into_inner())?;
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::UpdateCustomList(tx, custom_list))?;
        self.wait_for_result(rx)
            .await?
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    async fn clear_custom_lists(&self, _: Request<()>) -> ServiceResult<()> {
        log::debug!("clear_custom_lists");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::ClearCustomLists(tx))?;
        self.wait_for_result(rx)
            .await?
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    // Access Methods

    async fn add_api_access_method(
        &self,
        request: Request<types::NewAccessMethodSetting>,
    ) -> ServiceResult<types::Uuid> {
        log::debug!("add_api_access_method");
        let request = request.into_inner();
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::AddApiAccessMethod(
            tx,
            request.name,
            request.enabled,
            request
                .access_method
                .ok_or(Status::invalid_argument("Could not find access method"))
                .map(mullvad_types::access_method::AccessMethod::try_from)??,
        ))?;
        self.wait_for_result(rx)
            .await?
            .map(types::Uuid::from)
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    async fn remove_api_access_method(&self, request: Request<types::Uuid>) -> ServiceResult<()> {
        log::debug!("remove_api_access_method");
        let api_access_method = mullvad_types::access_method::Id::try_from(request.into_inner())?;
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::RemoveApiAccessMethod(tx, api_access_method))?;
        self.wait_for_result(rx)
            .await?
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    async fn set_api_access_method(&self, request: Request<types::Uuid>) -> ServiceResult<()> {
        log::debug!("set_api_access_method");
        let api_access_method = mullvad_types::access_method::Id::try_from(request.into_inner())?;
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetApiAccessMethod(tx, api_access_method))?;
        self.wait_for_result(rx)
            .await?
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    async fn update_api_access_method(
        &self,
        request: Request<types::AccessMethodSetting>,
    ) -> ServiceResult<()> {
        log::debug!("update_api_access_method");
        let access_method_update =
            mullvad_types::access_method::AccessMethodSetting::try_from(request.into_inner())?;
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::UpdateApiAccessMethod(
            tx,
            access_method_update,
        ))?;
        self.wait_for_result(rx)
            .await?
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    async fn clear_custom_api_access_methods(&self, _: Request<()>) -> ServiceResult<()> {
        log::debug!("clear_custom_api_access_methods");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::ClearCustomApiAccessMethods(tx))?;
        self.wait_for_result(rx)
            .await?
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    /// Return the [`types::AccessMethodSetting`] which the daemon is using to
    /// connect to the Mullvad API.
    async fn get_current_api_access_method(
        &self,
        _: Request<()>,
    ) -> ServiceResult<types::AccessMethodSetting> {
        log::debug!("get_current_api_access_method");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::GetCurrentAccessMethod(tx))?;
        self.wait_for_result(rx)
            .await?
            .map(types::AccessMethodSetting::from)
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    async fn test_custom_api_access_method(
        &self,
        config: Request<types::CustomProxy>,
    ) -> ServiceResult<bool> {
        log::debug!("test_custom_api_access_method");
        let (tx, rx) = oneshot::channel();
        let proxy = talpid_types::net::proxy::CustomProxy::try_from(config.into_inner())?;
        self.send_command_to_daemon(DaemonCommand::TestCustomApiAccessMethod(tx, proxy))?;
        self.wait_for_result(rx)
            .await?
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    async fn test_api_access_method_by_id(
        &self,
        request: Request<types::Uuid>,
    ) -> ServiceResult<bool> {
        log::debug!("test_api_access_method_by_id");
        let (tx, rx) = oneshot::channel();
        let api_access_method = mullvad_types::access_method::Id::try_from(request.into_inner())?;
        self.send_command_to_daemon(DaemonCommand::TestApiAccessMethodById(
            tx,
            api_access_method,
        ))?;
        self.wait_for_result(rx)
            .await?
            .map(Response::new)
            .map_err(map_daemon_error)
    }

    // Split tunneling
    //

    async fn split_tunnel_is_enabled(&self, _: Request<()>) -> ServiceResult<bool> {
        #[cfg(target_os = "linux")]
        {
            log::debug!("split_tunnel_is_enabled");
            let (tx, rx) = oneshot::channel();
            self.send_command_to_daemon(DaemonCommand::SplitTunnelIsEnabled(tx))?;
            Ok(self.wait_for_result(rx).await.map(Response::new)?)
        }
        #[cfg(not(target_os = "linux"))]
        {
            log::error!("split_tunnel_is_enabled is only available on Linux");
            Ok(Response::new(false))
        }
    }

    async fn get_split_tunnel_processes(
        &self,
        _: Request<()>,
    ) -> ServiceResult<Self::GetSplitTunnelProcessesStream> {
        #[cfg(target_os = "linux")]
        {
            log::debug!("get_split_tunnel_processes");
            let (tx, rx) = oneshot::channel();
            self.send_command_to_daemon(DaemonCommand::GetSplitTunnelProcesses(tx))?;
            let pids = self
                .wait_for_result(rx)
                .await?
                .map_err(|error| Status::failed_precondition(error.to_string()))?;

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(async move {
                for pid in pids {
                    let _ = tx.send(Ok(pid));
                }
            });

            Ok(Response::new(UnboundedReceiverStream::new(rx)))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let (_, rx) = tokio::sync::mpsc::unbounded_channel();
            Ok(Response::new(UnboundedReceiverStream::new(rx)))
        }
    }

    #[cfg(target_os = "linux")]
    async fn add_split_tunnel_process(&self, request: Request<i32>) -> ServiceResult<()> {
        let pid = request.into_inner();
        log::debug!("add_split_tunnel_process");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::AddSplitTunnelProcess(tx, pid))?;
        self.wait_for_result(rx)
            .await?
            .map_err(|error| Status::failed_precondition(error.to_string()))?;
        Ok(Response::new(()))
    }
    #[cfg(not(target_os = "linux"))]
    async fn add_split_tunnel_process(&self, _: Request<i32>) -> ServiceResult<()> {
        Ok(Response::new(()))
    }

    #[cfg(target_os = "linux")]
    async fn remove_split_tunnel_process(&self, request: Request<i32>) -> ServiceResult<()> {
        let pid = request.into_inner();
        log::debug!("remove_split_tunnel_process");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::RemoveSplitTunnelProcess(tx, pid))?;
        self.wait_for_result(rx)
            .await?
            .map_err(|error| Status::failed_precondition(error.to_string()))?;
        Ok(Response::new(()))
    }
    #[cfg(not(target_os = "linux"))]
    async fn remove_split_tunnel_process(&self, _: Request<i32>) -> ServiceResult<()> {
        Ok(Response::new(()))
    }

    async fn clear_split_tunnel_processes(&self, _: Request<()>) -> ServiceResult<()> {
        #[cfg(target_os = "linux")]
        {
            log::debug!("clear_split_tunnel_processes");
            let (tx, rx) = oneshot::channel();
            self.send_command_to_daemon(DaemonCommand::ClearSplitTunnelProcesses(tx))?;
            self.wait_for_result(rx)
                .await?
                .map_err(|error| Status::failed_precondition(error.to_string()))?;
            Ok(Response::new(()))
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(Response::new(()))
        }
    }

    #[cfg(any(windows, target_os = "android", target_os = "macos"))]
    async fn add_split_tunnel_app(&self, request: Request<String>) -> ServiceResult<()> {
        use mullvad_types::settings::SplitApp;
        log::debug!("add_split_tunnel_app");
        let path = SplitApp::from(request.into_inner());
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::AddSplitTunnelApp(tx, path))?;
        self.wait_for_result(rx)
            .await?
            .map_err(map_daemon_error)
            .map(Response::new)
    }

    #[cfg(target_os = "linux")]
    async fn add_split_tunnel_app(&self, _: Request<String>) -> ServiceResult<()> {
        Ok(Response::new(()))
    }

    #[cfg(any(windows, target_os = "android", target_os = "macos"))]
    async fn remove_split_tunnel_app(&self, request: Request<String>) -> ServiceResult<()> {
        use mullvad_types::settings::SplitApp;
        log::debug!("remove_split_tunnel_app");
        let path = SplitApp::from(request.into_inner());
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::RemoveSplitTunnelApp(tx, path))?;
        self.wait_for_result(rx)
            .await?
            .map_err(map_daemon_error)
            .map(Response::new)
    }
    #[cfg(target_os = "linux")]
    async fn remove_split_tunnel_app(&self, _: Request<String>) -> ServiceResult<()> {
        Ok(Response::new(()))
    }

    #[cfg(any(windows, target_os = "android", target_os = "macos"))]
    async fn clear_split_tunnel_apps(&self, _: Request<()>) -> ServiceResult<()> {
        log::debug!("clear_split_tunnel_apps");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::ClearSplitTunnelApps(tx))?;
        self.wait_for_result(rx)
            .await?
            .map_err(map_daemon_error)
            .map(Response::new)
    }
    #[cfg(target_os = "linux")]
    async fn clear_split_tunnel_apps(&self, _: Request<()>) -> ServiceResult<()> {
        Ok(Response::new(()))
    }

    #[cfg(any(windows, target_os = "android", target_os = "macos"))]
    async fn set_split_tunnel_state(&self, request: Request<bool>) -> ServiceResult<()> {
        log::debug!("set_split_tunnel_state");
        let enabled = request.into_inner();
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::SetSplitTunnelState(tx, enabled))?;
        self.wait_for_result(rx)
            .await?
            .map_err(map_daemon_error)
            .map(Response::new)
    }
    #[cfg(target_os = "linux")]
    async fn set_split_tunnel_state(&self, _: Request<bool>) -> ServiceResult<()> {
        Ok(Response::new(()))
    }

    #[cfg(windows)]
    async fn get_excluded_processes(
        &self,
        _: Request<()>,
    ) -> ServiceResult<types::ExcludedProcessList> {
        log::debug!("get_excluded_processes");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::GetSplitTunnelProcesses(tx))?;
        self.wait_for_result(rx)
            .await?
            .map_err(map_split_tunnel_error)
            .map(|processes| {
                Response::new(types::ExcludedProcessList {
                    processes: processes
                        .into_iter()
                        .map(types::ExcludedProcess::from)
                        .collect(),
                })
            })
    }

    #[cfg(not(windows))]
    async fn get_excluded_processes(
        &self,
        _: Request<()>,
    ) -> ServiceResult<types::ExcludedProcessList> {
        Ok(Response::new(types::ExcludedProcessList {
            processes: vec![],
        }))
    }

    #[cfg(target_os = "macos")]
    async fn need_full_disk_permissions(&self, _: Request<()>) -> ServiceResult<bool> {
        log::debug!("need_full_disk_permissions");
        let has_access = talpid_core::split_tunnel::has_full_disk_access().await;
        Ok(Response::new(!has_access))
    }

    #[cfg(not(target_os = "macos"))]
    async fn need_full_disk_permissions(&self, _: Request<()>) -> ServiceResult<bool> {
        Ok(Response::new(false))
    }

    #[cfg(windows)]
    async fn check_volumes(&self, _: Request<()>) -> ServiceResult<()> {
        log::debug!("check_volumes");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::CheckVolumes(tx))?;
        self.wait_for_result(rx)
            .await?
            .map_err(map_daemon_error)
            .map(Response::new)
    }

    #[cfg(not(windows))]
    async fn check_volumes(&self, _: Request<()>) -> ServiceResult<()> {
        Ok(Response::new(()))
    }

    async fn apply_json_settings(&self, blob: Request<String>) -> ServiceResult<()> {
        log::debug!("apply_json_settings");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::ApplyJsonSettings(tx, blob.into_inner()))?;
        self.wait_for_result(rx).await??;
        Ok(Response::new(()))
    }

    async fn export_json_settings(&self, _: Request<()>) -> ServiceResult<String> {
        log::debug!("export_json_settings");
        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::ExportJsonSettings(tx))?;
        let blob = self.wait_for_result(rx).await??;
        Ok(Response::new(blob))
    }

    #[cfg(target_os = "android")]
    async fn init_play_purchase(
        &self,
        _request: Request<()>,
    ) -> ServiceResult<types::PlayPurchasePaymentToken> {
        log::debug!("init_play_purchase");

        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::InitPlayPurchase(tx))?;

        let payment_token = self
            .wait_for_result(rx)
            .await?
            .map(types::PlayPurchasePaymentToken::from)
            .map_err(map_daemon_error)?;

        Ok(Response::new(payment_token))
    }

    /// On non-Android platforms, the return value will be useless.
    #[cfg(not(target_os = "android"))]
    async fn init_play_purchase(
        &self,
        _: Request<()>,
    ) -> ServiceResult<types::PlayPurchasePaymentToken> {
        log::error!("Called `init_play_purchase` on non-Android platform");
        Ok(Response::new(types::PlayPurchasePaymentToken {
            token: String::default(),
        }))
    }

    #[cfg(target_os = "android")]
    async fn verify_play_purchase(
        &self,
        request: Request<types::PlayPurchase>,
    ) -> ServiceResult<()> {
        log::debug!("verify_play_purchase");

        let (tx, rx) = oneshot::channel();
        let play_purchase = mullvad_types::account::PlayPurchase::try_from(request.into_inner())?;

        self.send_command_to_daemon(DaemonCommand::VerifyPlayPurchase(tx, play_purchase))?;

        self.wait_for_result(rx).await?.map_err(map_daemon_error)?;

        Ok(Response::new(()))
    }

    #[cfg(not(target_os = "android"))]
    async fn verify_play_purchase(&self, _: Request<types::PlayPurchase>) -> ServiceResult<()> {
        log::error!("Called `verify_play_purchase` on non-Android platform");
        Ok(Response::new(()))
    }

    async fn get_feature_indicators(
        &self,
        _: Request<()>,
    ) -> ServiceResult<types::FeatureIndicators> {
        log::debug!("get_feature_indicators");

        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::GetFeatureIndicators(tx))?;

        let feature_indicators = self
            .wait_for_result(rx)
            .await
            .map(types::FeatureIndicators::from)?;

        Ok(Response::new(feature_indicators))
    }

    // Debug features

    async fn disable_relay(&self, relay: Request<String>) -> ServiceResult<()> {
        log::debug!("disable_relay");
        let (tx, rx) = oneshot::channel();
        let relay = relay.into_inner();
        self.send_command_to_daemon(DaemonCommand::DisableRelay { relay, tx })?;
        self.wait_for_result(rx).await?;
        Ok(Response::new(()))
    }

    async fn enable_relay(&self, relay: Request<String>) -> ServiceResult<()> {
        log::debug!("enable_relay");
        let (tx, rx) = oneshot::channel();
        let relay = relay.into_inner();
        self.send_command_to_daemon(DaemonCommand::EnableRelay { relay, tx })?;
        self.wait_for_result(rx).await?;
        Ok(Response::new(()))
    }

    // App upgrade

    async fn app_upgrade(&self, _: Request<()>) -> ServiceResult<()> {
        log::debug!("app_upgrade");

        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::AppUpgrade(tx))?;

        self.wait_for_result(rx)
            .await?
            .map_err(map_version_check_error)?;

        Ok(Response::new(()))
    }

    async fn app_upgrade_abort(&self, _: Request<()>) -> ServiceResult<()> {
        log::debug!("app_upgrade_abort");

        let (tx, rx) = oneshot::channel();
        self.send_command_to_daemon(DaemonCommand::AppUpgradeAbort(tx))?;

        self.wait_for_result(rx)
            .await?
            .map_err(map_version_check_error)?;

        Ok(Response::new(()))
    }

    async fn app_upgrade_events_listen(
        &self,
        _: Request<()>,
    ) -> ServiceResult<Self::AppUpgradeEventsListenStream> {
        log::debug!("app_upgrade_events_listen");
        let rx = self.app_upgrade_broadcast.subscribe();
        let upgrade_event_stream =
            tokio_stream::wrappers::BroadcastStream::new(rx).map(|result| match result {
                Ok(event) => Ok(event.into()),
                Err(error) => Err(Status::internal(format!(
                    "Failed to receive app upgrade event: {error}"
                ))),
            });

        Ok(Response::new(
            Box::new(upgrade_event_stream) as Self::AppUpgradeEventsListenStream
        ))
    }
}

impl ManagementServiceImpl {
    /// Sends a command to the daemon and maps the error to an RPC error.
    fn send_command_to_daemon(&self, command: DaemonCommand) -> Result<(), Status> {
        self.daemon_tx
            .send(command)
            .map_err(|_| Status::internal("the daemon channel receiver has been dropped"))
    }

    async fn wait_for_result<T>(&self, rx: oneshot::Receiver<T>) -> Result<T, Status> {
        rx.await.map_err(|_| Status::internal("sender was dropped"))
    }
}

/// The running management interface serving gRPC requests.
pub struct ManagementInterfaceServer {
    /// The rpc server spawned by [`Self::start`]. When the underlying join handle yields, the rpc
    /// server has shutdown.
    rpc_server_join_handle: ServerJoinHandle,
    /// Channel used to signal the running gRPC server to shutdown. This needs to be done before
    /// awaiting trying to join [`Self::rpc_server_join_handle`].
    server_abort_tx: mpsc::Sender<()>,
    /// A reference to the associated [`ManagementInterfaceEventBroadcaster`]. This may be used to
    /// broadcast certain events to all subscribers of the management interface.
    broadcast: ManagementInterfaceEventBroadcaster,
}

impl ManagementInterfaceServer {
    pub fn start(
        daemon_tx: DaemonCommandSender,
        rpc_socket_path: impl AsRef<Path>,
        app_upgrade_broadcast: tokio::sync::broadcast::Sender<version::AppUpgradeEvent>,
    ) -> Result<ManagementInterfaceServer, Error> {
        let subscriptions = Arc::<Mutex<Vec<EventsListenerSender>>>::default();

        // NOTE: It is important that the channel buffer size is kept at 0. When sending a signal
        // to abort the gRPC server, the sender can be awaited to know when the gRPC server has
        // received and started processing the shutdown signal.
        let (server_abort_tx, server_abort_rx) = mpsc::channel(0);

        let server = ManagementServiceImpl {
            daemon_tx,
            subscriptions: subscriptions.clone(),
            app_upgrade_broadcast,
        };
        let rpc_server_join_handle = mullvad_management_interface::spawn_rpc_server(
            server,
            async move {
                StreamExt::into_future(server_abort_rx).await;
            },
            &rpc_socket_path,
        )
        .map_err(Error::SetupError)?;

        log::info!(
            "Management interface listening on {}",
            rpc_socket_path.as_ref().display()
        );

        let broadcast = ManagementInterfaceEventBroadcaster { subscriptions };

        Ok(ManagementInterfaceServer {
            rpc_server_join_handle,
            server_abort_tx,
            broadcast,
        })
    }

    /// Wait for the server to shut down gracefully. If that does not happend within
    /// [`RPC_SERVER_SHUTDOWN_TIMEOUT`], the gRPC server is aborted and we yield the async
    /// execution.
    pub async fn stop(mut self) {
        use futures::SinkExt;
        // Send a singal to the underlying RPC server to shut down.
        let _ = self.server_abort_tx.send(()).await;

        match timeout(RPC_SERVER_SHUTDOWN_TIMEOUT, self.rpc_server_join_handle).await {
            // Joining the rpc server handle timed out
            Err(timeout) => {
                log::error!("Timed out while shutting down management server: {timeout}");
            }
            Ok(join_result) => {
                if let Err(_error) = join_result {
                    log::error!("Management server task failed to execute until completion");
                }
            }
        }
    }

    /// Obtain a reference to the associated [`ManagementInterfaceEventBroadcaster`].
    pub const fn notifier(&self) -> &ManagementInterfaceEventBroadcaster {
        &self.broadcast
    }
}

/// A handle that allows broadcasting messages to all subscribers of the management interface.
#[derive(Clone)]
pub struct ManagementInterfaceEventBroadcaster {
    subscriptions: Arc<Mutex<Vec<EventsListenerSender>>>,
}

impl ManagementInterfaceEventBroadcaster {
    fn notify(&self, value: types::DaemonEvent) {
        let mut subscriptions = self.subscriptions.lock().unwrap();
        subscriptions.retain(|tx| tx.send(Ok(value.clone())).is_ok());
    }

    /// Notify that the tunnel state changed.
    ///
    /// Sends a new state update to all `new_state` subscribers of the management interface.
    pub(crate) fn notify_new_state(&self, new_state: TunnelState) {
        self.notify(types::DaemonEvent {
            event: Some(daemon_event::Event::TunnelState(types::TunnelState::from(
                new_state,
            ))),
        })
    }

    /// Notify that the settings changed.
    ///
    /// Sends settings to all `settings` subscribers of the management interface.
    pub(crate) fn notify_settings(&self, settings: Settings) {
        log::debug!("Broadcasting new settings");
        self.notify(types::DaemonEvent {
            event: Some(daemon_event::Event::Settings(types::Settings::from(
                &settings,
            ))),
        })
    }

    /// Notify that the relay list changed.
    ///
    /// Sends relays to all subscribers of the management interface.
    pub(crate) fn notify_relay_list(&self, relay_list: RelayList) {
        log::debug!("Broadcasting new relay list");
        self.notify(types::DaemonEvent {
            event: Some(daemon_event::Event::RelayList(types::RelayList::from(
                relay_list,
            ))),
        })
    }

    /// Notify that info about the latest available app version changed.
    /// Or some flag about the currently running version is changed.
    pub(crate) fn notify_app_version(&self, app_version_info: version::AppVersionInfo) {
        log::debug!("Broadcasting app version info:\n{app_version_info}");
        self.notify(types::DaemonEvent {
            event: Some(daemon_event::Event::VersionInfo(
                types::AppVersionInfo::from(app_version_info),
            )),
        })
    }

    /// Notify that device changed (login, logout, or key rotation).
    pub(crate) fn notify_device_event(&self, device: mullvad_types::device::DeviceEvent) {
        log::debug!("Broadcasting device event");
        self.notify(types::DaemonEvent {
            event: Some(daemon_event::Event::Device(types::DeviceEvent::from(
                device,
            ))),
        })
    }

    /// Notify that a device was revoked using `RemoveDevice`.
    pub(crate) fn notify_remove_device_event(
        &self,
        remove_event: mullvad_types::device::RemoveDeviceEvent,
    ) {
        log::debug!("Broadcasting remove device event");
        self.notify(types::DaemonEvent {
            event: Some(daemon_event::Event::RemoveDevice(
                types::RemoveDeviceEvent::from(remove_event),
            )),
        })
    }

    /// Notify that the api access method changed.
    pub(crate) fn notify_new_access_method_event(
        &self,
        new_access_method: mullvad_types::access_method::AccessMethodSetting,
    ) {
        log::debug!("Broadcasting access method event");
        self.notify(types::DaemonEvent {
            event: Some(daemon_event::Event::NewAccessMethod(
                types::AccessMethodSetting::from(new_access_method),
            )),
        })
    }
}

/// Converts [`crate::Error`] into a tonic status.
fn map_daemon_error(error: crate::Error) -> Status {
    use crate::Error as DaemonError;

    match error {
        DaemonError::RestError(error) => map_rest_error(&error),
        DaemonError::SettingsError(error) => Status::from(error),
        DaemonError::AlreadyLoggedIn => Status::already_exists(error.to_string()),
        DaemonError::LoginError(error) => map_device_error(&error),
        DaemonError::LogoutError(error) => map_device_error(&error),
        DaemonError::KeyRotationError(error) => map_device_error(&error),
        DaemonError::ListDevicesError(error) => map_device_error(&error),
        DaemonError::RemoveDeviceError(error) => map_device_error(&error),
        DaemonError::UpdateDeviceError(error) => map_device_error(&error),
        DaemonError::VoucherSubmission(error) => map_device_error(&error),
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        DaemonError::SplitTunnelError(error) => map_split_tunnel_error(error),
        DaemonError::AccountHistory(error) => map_account_history_error(error),
        DaemonError::NoAccountNumber | DaemonError::NoAccountNumberHistory => {
            Status::unauthenticated(error.to_string())
        }
        DaemonError::VersionCheckError(error) => map_version_check_error(error),
        error => Status::unknown(error.to_string()),
    }
}

#[cfg(windows)]
/// Converts [`talpid_core::split_tunnel::Error`] into a tonic status.
fn map_split_tunnel_error(error: talpid_core::split_tunnel::Error) -> Status {
    use talpid_core::split_tunnel::Error;

    match &error {
        Error::RegisterIps(io_error) | Error::SetConfiguration(io_error) => {
            if io_error.kind() == std::io::ErrorKind::NotFound {
                Status::not_found(format!("{}: {}", error, io_error))
            } else {
                Status::unknown(error.to_string())
            }
        }
        _ => Status::unknown(error.to_string()),
    }
}

#[cfg(target_os = "macos")]
/// Converts [`talpid_core::split_tunnel::Error`] into a tonic status.
fn map_split_tunnel_error(error: talpid_core::split_tunnel::Error) -> Status {
    Status::unknown(error.to_string())
}

/// Converts a REST API error into a tonic status.
fn map_rest_error(error: &RestError) -> Status {
    match error {
        RestError::ApiError(status, message)
            if *status == StatusCode::UNAUTHORIZED || *status == StatusCode::FORBIDDEN =>
        {
            Status::new(Code::Unauthenticated, message)
        }
        RestError::TimeoutError => Status::deadline_exceeded("API request timed out"),
        RestError::HyperError(_) => Status::unavailable("Cannot reach the API"),
        error => Status::unknown(format!("REST error: {error}")),
    }
}

/// Converts an instance of [`crate::device::Error`] into a tonic status.
fn map_device_error(error: &device::Error) -> Status {
    match error {
        device::Error::MaxDevicesReached => Status::new(Code::ResourceExhausted, error.to_string()),
        device::Error::InvalidAccount => Status::new(Code::Unauthenticated, error.to_string()),
        device::Error::InvalidDevice | device::Error::NoDevice => {
            Status::new(Code::NotFound, error.to_string())
        }
        device::Error::InvalidVoucher => Status::new(Code::NotFound, INVALID_VOUCHER_MESSAGE),
        device::Error::UsedVoucher => Status::new(Code::ResourceExhausted, USED_VOUCHER_MESSAGE),
        device::Error::DeviceIoError(_error) => Status::new(Code::Unavailable, error.to_string()),
        device::Error::OtherRestError(error) => map_rest_error(error),
        _ => Status::new(Code::Unknown, error.to_string()),
    }
}

/// Converts an instance of [`crate::account_history::Error`] into a tonic status.
fn map_account_history_error(error: account_history::Error) -> Status {
    match error {
        account_history::Error::Read(..) | account_history::Error::Write(..) => {
            Status::new(Code::FailedPrecondition, error.to_string())
        }
        account_history::Error::Serialize(..) | account_history::Error::WriteCancelled(..) => {
            Status::new(Code::Internal, error.to_string())
        }
    }
}

fn map_version_check_error(error: crate::version::Error) -> Status {
    match error {
        crate::version::Error::Download(..)
        | crate::version::Error::ReadVersionCache(..)
        | crate::version::Error::ApiCheck(..) => Status::unavailable(error.to_string()),
        _ => Status::unknown(error.to_string()),
    }
}

fn map_protobuf_type_err(err: types::FromProtobufTypeError) -> Status {
    match err {
        types::FromProtobufTypeError::InvalidArgument(err) => Status::invalid_argument(err),
    }
}
