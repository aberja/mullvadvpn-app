//! Client that returns and takes mullvad types as arguments instead of prost-generated types

use crate::types;
#[cfg(not(target_os = "android"))]
use futures::{Stream, StreamExt};
#[cfg(all(daita, not(target_os = "android")))]
use mullvad_types::wireguard::DaitaSettings;
use mullvad_types::{
    access_method::AccessMethodSetting,
    device::{DeviceEvent, RemoveDeviceEvent},
    relay_list::RelayList,
    settings::Settings,
    states::TunnelState,
    version::AppVersionInfo,
};

#[cfg(not(target_os = "android"))]
use mullvad_types::{
    access_method::{self, AccessMethod},
    account::{AccountData, AccountNumber, VoucherSubmission},
    custom_list::{CustomList, Id},
    device::{Device, DeviceId, DeviceState},
    features::FeatureIndicators,
    relay_constraints::{
        AllowedIps, BridgeSettings, BridgeState, ObfuscationSettings, RelayOverride, RelaySettings,
    },
    settings::DnsOptions,
    wireguard::{PublicKey, QuantumResistantState, RotationInterval},
};
#[cfg(not(target_os = "android"))]
use std::{path::Path, str::FromStr};
#[cfg(target_os = "windows")]
use talpid_types::split_tunnel::ExcludedProcess;
#[cfg(not(target_os = "android"))]
use tonic::{Code, Status};

type Error = super::Error;

pub type Result<T> = std::result::Result<T, super::Error>;

#[cfg(not(target_os = "android"))]
#[derive(Debug, Clone)]
pub struct MullvadProxyClient(crate::ManagementServiceClient);

#[derive(Debug)]
pub enum DaemonEvent {
    TunnelState(TunnelState),
    Settings(Settings),
    RelayList(RelayList),
    AppVersionInfo(AppVersionInfo),
    Device(DeviceEvent),
    RemoveDevice(RemoveDeviceEvent),
    NewAccessMethod(AccessMethodSetting),
}

impl TryFrom<types::daemon_event::Event> for DaemonEvent {
    type Error = Error;

    fn try_from(value: types::daemon_event::Event) -> Result<Self> {
        match value {
            types::daemon_event::Event::TunnelState(state) => TunnelState::try_from(state)
                .map(DaemonEvent::TunnelState)
                .map_err(Error::InvalidResponse),
            types::daemon_event::Event::Settings(settings) => Settings::try_from(settings)
                .map(DaemonEvent::Settings)
                .map_err(Error::InvalidResponse),
            types::daemon_event::Event::RelayList(list) => RelayList::try_from(list)
                .map(DaemonEvent::RelayList)
                .map_err(Error::InvalidResponse),
            types::daemon_event::Event::VersionInfo(info) => AppVersionInfo::try_from(info)
                .map(DaemonEvent::AppVersionInfo)
                .map_err(Error::InvalidResponse),
            types::daemon_event::Event::Device(event) => DeviceEvent::try_from(event)
                .map(DaemonEvent::Device)
                .map_err(Error::InvalidResponse),
            types::daemon_event::Event::RemoveDevice(event) => RemoveDeviceEvent::try_from(event)
                .map(DaemonEvent::RemoveDevice)
                .map_err(Error::InvalidResponse),
            types::daemon_event::Event::NewAccessMethod(event) => {
                AccessMethodSetting::try_from(event)
                    .map(DaemonEvent::NewAccessMethod)
                    .map_err(Error::InvalidResponse)
            }
        }
    }
}

#[cfg(not(target_os = "android"))]
impl MullvadProxyClient {
    pub async fn new() -> Result<Self> {
        #[allow(deprecated)]
        super::new_rpc_client().await.map(Self)
    }

    pub fn from_rpc_client(client: crate::ManagementServiceClient) -> Self {
        Self(client)
    }

    pub async fn connect_tunnel(&mut self) -> Result<bool> {
        Ok(self
            .0
            .connect_tunnel(())
            .await
            .map_err(Error::Rpc)?
            .into_inner())
    }

    pub async fn disconnect_tunnel(&mut self) -> Result<bool> {
        Ok(self
            .0
            .disconnect_tunnel(())
            .await
            .map_err(Error::Rpc)?
            .into_inner())
    }

    pub async fn reconnect_tunnel(&mut self) -> Result<bool> {
        Ok(self
            .0
            .reconnect_tunnel(())
            .await
            .map_err(Error::Rpc)?
            .into_inner())
    }

    pub async fn get_tunnel_state(&mut self) -> Result<TunnelState> {
        let state = self
            .0
            .get_tunnel_state(())
            .await
            .map_err(Error::Rpc)?
            .into_inner();
        TunnelState::try_from(state).map_err(Error::InvalidResponse)
    }

    pub async fn events_listen<'a>(
        &mut self,
    ) -> Result<impl Stream<Item = Result<DaemonEvent>> + 'a> {
        let listener = self
            .0
            .events_listen(())
            .await
            .map_err(Error::Rpc)?
            .into_inner();

        Ok(listener.map(|item| {
            let event = item
                .map_err(Error::Rpc)?
                .event
                .ok_or(Error::MissingDaemonEvent)?;
            DaemonEvent::try_from(event)
        }))
    }

    /// DEPRECATED: Prefer to use `prepare_restart_v2`.
    pub async fn prepare_restart(&mut self) -> Result<()> {
        self.0.prepare_restart(()).await.map_err(Error::Rpc)?;
        Ok(())
    }

    /// Tell the daemon to get ready for a restart by securing a user, i.e. putting firewall rules
    /// in place.
    ///
    /// - `shutdown`: Whether the daemon should shutdown immediately after its prepare-for-restart
    ///   routine.
    pub async fn prepare_restart_v2(&mut self, shutdown: bool) -> Result<()> {
        self.0
            .prepare_restart_v2(shutdown)
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn factory_reset(&mut self) -> Result<()> {
        self.0.factory_reset(()).await.map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn get_current_version(&mut self) -> Result<String> {
        Ok(self
            .0
            .get_current_version(())
            .await
            .map_err(Error::Rpc)?
            .into_inner())
    }

    pub async fn get_version_info(&mut self) -> Result<AppVersionInfo> {
        let version_info = self
            .0
            .get_version_info(())
            .await
            .map_err(Error::Rpc)?
            .into_inner();
        AppVersionInfo::try_from(version_info).map_err(Error::InvalidResponse)
    }

    pub async fn get_relay_locations(&mut self) -> Result<RelayList> {
        let list = self
            .0
            .get_relay_locations(())
            .await
            .map_err(Error::Rpc)?
            .into_inner();
        mullvad_types::relay_list::RelayList::try_from(list).map_err(Error::InvalidResponse)
    }

    pub async fn get_api_access_methods(&mut self) -> Result<Vec<AccessMethodSetting>> {
        let access_method_settings = self
            .0
            .get_settings(())
            .await
            .map_err(Error::Rpc)?
            .into_inner()
            .api_access_methods
            .ok_or(Error::ApiAccessMethodSettingsNotFound)
            .and_then(|access_method_settings| {
                access_method::Settings::try_from(access_method_settings)
                    .map_err(Error::InvalidResponse)
            })?;

        Ok(access_method_settings.iter().cloned().collect())
    }

    pub async fn get_api_access_method(
        &mut self,
        id: &access_method::Id,
    ) -> Result<AccessMethodSetting> {
        self.get_api_access_methods()
            .await?
            .into_iter()
            .find(|api_access_method| api_access_method.get_id() == *id)
            .ok_or(Error::ApiAccessMethodNotFound)
    }

    pub async fn get_current_api_access_method(&mut self) -> Result<AccessMethodSetting> {
        self.0
            .get_current_api_access_method(())
            .await
            .map_err(Error::Rpc)
            .map(tonic::Response::into_inner)
            .and_then(|access_method| {
                AccessMethodSetting::try_from(access_method).map_err(Error::InvalidResponse)
            })
    }

    pub async fn test_api_access_method(&mut self, id: access_method::Id) -> Result<bool> {
        let result = self
            .0
            .test_api_access_method_by_id(types::Uuid::from(id))
            .await
            .map_err(Error::Rpc)?;
        Ok(result.into_inner())
    }

    pub async fn test_custom_api_access_method(
        &mut self,
        config: talpid_types::net::proxy::CustomProxy,
    ) -> Result<bool> {
        let result = self
            .0
            .test_custom_api_access_method(types::CustomProxy::from(config))
            .await
            .map_err(Error::Rpc)?;
        Ok(result.into_inner())
    }

    pub async fn update_relay_locations(&mut self) -> Result<()> {
        self.0
            .update_relay_locations(())
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn set_relay_settings(&mut self, update: RelaySettings) -> Result<()> {
        let update = types::RelaySettings::from(update);
        self.0
            .set_relay_settings(update)
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn set_bridge_settings(&mut self, settings: BridgeSettings) -> Result<()> {
        let settings = types::BridgeSettings::from(settings);
        self.0
            .set_bridge_settings(settings)
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn set_bridge_state(&mut self, state: BridgeState) -> Result<()> {
        let state = types::BridgeState::from(state);
        self.0.set_bridge_state(state).await.map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn set_obfuscation_settings(&mut self, settings: ObfuscationSettings) -> Result<()> {
        let settings = types::ObfuscationSettings::from(&settings);
        self.0
            .set_obfuscation_settings(settings)
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn get_settings(&mut self) -> Result<Settings> {
        let settings = self
            .0
            .get_settings(())
            .await
            .map_err(Error::Rpc)?
            .into_inner();
        Settings::try_from(settings).map_err(Error::InvalidResponse)
    }

    pub async fn reset_settings(&mut self) -> Result<()> {
        self.0.reset_settings(()).await.map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn set_allow_lan(&mut self, state: bool) -> Result<()> {
        self.0.set_allow_lan(state).await.map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn set_show_beta_releases(&mut self, state: bool) -> Result<()> {
        self.0
            .set_show_beta_releases(state)
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn set_block_when_disconnected(&mut self, state: bool) -> Result<()> {
        self.0
            .set_block_when_disconnected(state)
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn set_auto_connect(&mut self, state: bool) -> Result<()> {
        self.0.set_auto_connect(state).await.map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn set_openvpn_mssfix(&mut self, mssfix: Option<u16>) -> Result<()> {
        self.0
            .set_openvpn_mssfix(mssfix.map(u32::from).unwrap_or(0))
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn set_wireguard_mtu(&mut self, mtu: Option<u16>) -> Result<()> {
        self.0
            .set_wireguard_mtu(mtu.map(u32::from).unwrap_or(0))
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn set_enable_ipv6(&mut self, state: bool) -> Result<()> {
        self.0.set_enable_ipv6(state).await.map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn set_quantum_resistant_tunnel(
        &mut self,
        state: QuantumResistantState,
    ) -> Result<()> {
        let state = types::QuantumResistantState::from(state);
        self.0
            .set_quantum_resistant_tunnel(state)
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    #[cfg(daita)]
    pub async fn set_enable_daita(&mut self, value: bool) -> Result<()> {
        self.0.set_enable_daita(value).await.map_err(Error::Rpc)?;
        Ok(())
    }

    #[cfg(daita)]
    pub async fn set_daita_direct_only(&mut self, value: bool) -> Result<()> {
        self.0
            .set_daita_direct_only(value)
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    #[cfg(daita)]
    pub async fn set_daita_settings(&mut self, settings: DaitaSettings) -> Result<()> {
        let settings = types::DaitaSettings::from(settings);
        self.0
            .set_daita_settings(settings)
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn set_dns_options(&mut self, options: DnsOptions) -> Result<()> {
        let options = types::DnsOptions::from(&options);
        self.0.set_dns_options(options).await.map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn set_relay_override(&mut self, relay_override: RelayOverride) -> Result<()> {
        let r#override = types::RelayOverride::from(relay_override);
        self.0
            .set_relay_override(r#override)
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn clear_all_relay_overrides(&mut self) -> Result<()> {
        self.0
            .clear_all_relay_overrides(())
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn create_new_account(&mut self) -> Result<AccountNumber> {
        Ok(self
            .0
            .create_new_account(())
            .await
            .map_err(map_device_error)?
            .into_inner())
    }

    pub async fn login_account(&mut self, account: AccountNumber) -> Result<()> {
        self.0
            .login_account(account)
            .await
            .map_err(map_device_error)?;
        Ok(())
    }

    pub async fn logout_account(&mut self) -> Result<()> {
        self.0.logout_account(()).await.map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn get_account_data(&mut self, account: AccountNumber) -> Result<AccountData> {
        let data = self
            .0
            .get_account_data(account)
            .await
            .map_err(Error::Rpc)?
            .into_inner();
        AccountData::try_from(data).map_err(Error::InvalidResponse)
    }

    pub async fn get_account_history(&mut self) -> Result<Option<AccountNumber>> {
        let history = self
            .0
            .get_account_history(())
            .await
            .map_err(Error::Rpc)?
            .into_inner();
        Ok(history.number)
    }

    pub async fn clear_account_history(&mut self) -> Result<()> {
        self.0.clear_account_history(()).await.map_err(Error::Rpc)?;
        Ok(())
    }

    // get_www_auth_token

    pub async fn submit_voucher(&mut self, voucher: String) -> Result<VoucherSubmission> {
        let result = self
            .0
            .submit_voucher(voucher)
            .await
            .map_err(|error| match error.code() {
                Code::NotFound => Error::InvalidVoucher,
                Code::ResourceExhausted => Error::UsedVoucher,
                _other => Error::Rpc(error),
            })?
            .into_inner();
        VoucherSubmission::try_from(result).map_err(Error::InvalidResponse)
    }

    pub async fn get_device(&mut self) -> Result<DeviceState> {
        let state = self
            .0
            .get_device(())
            .await
            .map_err(map_device_error)?
            .into_inner();
        DeviceState::try_from(state).map_err(Error::InvalidResponse)
    }

    pub async fn update_device(&mut self) -> Result<()> {
        self.0.update_device(()).await.map_err(map_device_error)?;
        Ok(())
    }

    pub async fn list_devices(&mut self, account: AccountNumber) -> Result<Vec<Device>> {
        let list = self
            .0
            .list_devices(account)
            .await
            .map_err(map_device_error)?
            .into_inner();
        list.devices
            .into_iter()
            .map(|d| Device::try_from(d).map_err(Error::InvalidResponse))
            .collect::<Result<_>>()
    }

    pub async fn remove_device(
        &mut self,
        account: AccountNumber,
        device_id: DeviceId,
    ) -> Result<()> {
        self.0
            .remove_device(types::DeviceRemoval {
                account_number: account,
                device_id,
            })
            .await
            .map_err(map_device_error)?;
        Ok(())
    }

    pub async fn set_wireguard_rotation_interval(
        &mut self,
        interval: RotationInterval,
    ) -> Result<()> {
        let duration = types::Duration::try_from(*interval.as_duration())
            .map_err(|_| Error::DurationTooLarge)?;
        self.0
            .set_wireguard_rotation_interval(duration)
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn reset_wireguard_rotation_interval(&mut self) -> Result<()> {
        self.0
            .reset_wireguard_rotation_interval(())
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn rotate_wireguard_key(&mut self) -> Result<()> {
        self.0.rotate_wireguard_key(()).await.map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn get_wireguard_key(&mut self) -> Result<PublicKey> {
        let key = self
            .0
            .get_wireguard_key(())
            .await
            .map_err(Error::Rpc)?
            .into_inner();
        PublicKey::try_from(key).map_err(Error::InvalidResponse)
    }

    pub async fn create_custom_list(&mut self, name: String) -> Result<Id> {
        let request = types::NewCustomList {
            name,
            locations: Vec::new(),
        };
        let id = self
            .0
            .create_custom_list(request)
            .await
            .map_err(map_custom_list_error)?
            .into_inner();
        Id::from_str(&id).map_err(|_| Error::CustomListListNotFound)
    }

    pub async fn delete_custom_list(&mut self, id: Id) -> Result<()> {
        self.0
            .delete_custom_list(id.to_string())
            .await
            .map_err(map_custom_list_error)?;
        Ok(())
    }

    pub async fn update_custom_list(&mut self, custom_list: CustomList) -> Result<()> {
        self.0
            .update_custom_list(types::CustomList::from(custom_list))
            .await
            .map_err(map_custom_list_error)?;
        Ok(())
    }

    /// Remove all custom lists.
    pub async fn clear_custom_lists(&mut self) -> Result<()> {
        self.0
            .clear_custom_lists(())
            .await
            .map_err(map_custom_list_error)?;
        Ok(())
    }

    pub async fn add_access_method(
        &mut self,
        name: String,
        enabled: bool,
        access_method: AccessMethod,
    ) -> Result<()> {
        let request = types::NewAccessMethodSetting {
            name,
            enabled,
            access_method: Some(types::AccessMethod::from(access_method)),
        };
        self.0
            .add_api_access_method(request)
            .await
            .map_err(Error::Rpc)
            .map(drop)
    }

    pub async fn remove_access_method(
        &mut self,
        api_access_method: access_method::Id,
    ) -> Result<()> {
        self.0
            .remove_api_access_method(types::Uuid::from(api_access_method))
            .await
            .map_err(Error::Rpc)
            .map(drop)
    }

    pub async fn update_access_method(
        &mut self,
        access_method_update: AccessMethodSetting,
    ) -> Result<()> {
        self.0
            .update_api_access_method(types::AccessMethodSetting::from(access_method_update))
            .await
            .map_err(Error::Rpc)
            .map(drop)
    }

    /// Remove all custom API access methods.
    pub async fn clear_custom_access_methods(&mut self) -> Result<()> {
        self.0
            .clear_custom_api_access_methods(())
            .await
            .map_err(Error::Rpc)
            .map(drop)
    }

    /// Set the [`AccessMethod`] which `AccessModeSelector` should pick.
    pub async fn set_access_method(&mut self, api_access_method: access_method::Id) -> Result<()> {
        self.0
            .set_api_access_method(types::Uuid::from(api_access_method))
            .await
            .map_err(Error::Rpc)
            .map(drop)
    }

    pub async fn get_split_tunnel_processes(&mut self) -> Result<Vec<i32>> {
        use futures::TryStreamExt;

        let procs = self
            .0
            .get_split_tunnel_processes(())
            .await
            .map_err(Error::Rpc)?
            .into_inner();
        procs.try_collect().await.map_err(Error::Rpc)
    }

    pub async fn add_split_tunnel_process(&mut self, pid: i32) -> Result<()> {
        self.0
            .add_split_tunnel_process(pid)
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn remove_split_tunnel_process(&mut self, pid: i32) -> Result<()> {
        self.0
            .remove_split_tunnel_process(pid)
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn clear_split_tunnel_processes(&mut self) -> Result<()> {
        self.0
            .clear_split_tunnel_processes(())
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn add_split_tunnel_app<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        let path = path.as_ref().to_str().ok_or(Error::PathMustBeUtf8)?;
        self.0
            .add_split_tunnel_app(path.to_owned())
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn remove_split_tunnel_app<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        let path = path.as_ref().to_str().ok_or(Error::PathMustBeUtf8)?;
        self.0
            .remove_split_tunnel_app(path.to_owned())
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn clear_split_tunnel_apps(&mut self) -> Result<()> {
        self.0
            .clear_split_tunnel_apps(())
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn set_split_tunnel_state(&mut self, state: bool) -> Result<()> {
        self.0
            .set_split_tunnel_state(state)
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }

    #[cfg(target_os = "windows")]
    pub async fn get_excluded_processes(&mut self) -> Result<Vec<ExcludedProcess>> {
        let procs = self
            .0
            .get_excluded_processes(())
            .await
            .map_err(Error::Rpc)?
            .into_inner();
        Ok(procs
            .processes
            .into_iter()
            .map(ExcludedProcess::from)
            .collect::<Vec<_>>())
    }

    // check_volumes

    pub async fn apply_json_settings(&mut self, blob: String) -> Result<()> {
        self.0.apply_json_settings(blob).await.map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn export_json_settings(&mut self) -> Result<String> {
        let blob = self.0.export_json_settings(()).await.map_err(Error::Rpc)?;
        Ok(blob.into_inner())
    }

    pub async fn get_feature_indicators(&mut self) -> Result<FeatureIndicators> {
        self.0
            .get_feature_indicators(())
            .await
            .map_err(Error::Rpc)
            .map(|response| response.into_inner())
            .map(FeatureIndicators::from)
    }

    // Debug features
    pub async fn disable_relay(&mut self, relay: String) -> Result<()> {
        self.0.disable_relay(relay).await.map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn enable_relay(&mut self, relay: String) -> Result<()> {
        self.0.enable_relay(relay).await.map_err(Error::Rpc)?;
        Ok(())
    }

    pub async fn set_wireguard_allowed_ips(&mut self, allowed_ips: AllowedIps) -> Result<()> {
        self.0
            .set_wireguard_allowed_ips(types::AllowedIpsList {
                values: allowed_ips.0.iter().map(ToString::to_string).collect(),
            })
            .await
            .map_err(Error::Rpc)?;
        Ok(())
    }
}

#[cfg(not(target_os = "android"))]
fn map_device_error(status: Status) -> Error {
    match status.code() {
        Code::ResourceExhausted => Error::TooManyDevices,
        Code::Unauthenticated => Error::InvalidAccount,
        Code::AlreadyExists => Error::AlreadyLoggedIn,
        Code::NotFound => Error::DeviceNotFound,
        _other => Error::Rpc(status),
    }
}

#[cfg(not(target_os = "android"))]
fn map_custom_list_error(status: Status) -> Error {
    match status.code() {
        Code::NotFound => {
            if status.details() == crate::CUSTOM_LIST_LIST_NOT_FOUND_DETAILS {
                Error::CustomListListNotFound
            } else {
                Error::Rpc(status)
            }
        }
        Code::AlreadyExists => {
            if status.details() == crate::CUSTOM_LIST_LIST_EXISTS_DETAILS {
                Error::CustomListExists
            } else {
                Error::Rpc(status)
            }
        }
        _other => Error::Rpc(status),
    }
}
