import { StrictMode } from 'react';
import { batch, Provider } from 'react-redux';
import { Router } from 'react-router';
import { bindActionCreators } from 'redux';
import { StyleSheetManager } from 'styled-components';

import { closeToExpiry, hasExpired } from '../shared/account-expiry';
import {
  ILinuxSplitTunnelingApplication,
  ISplitTunnelingApplication,
} from '../shared/application-types';
import { Url } from '../shared/constants';
import {
  AccessMethodSetting,
  AccountNumber,
  BridgeSettings,
  BridgeState,
  CustomProxy,
  DeviceEvent,
  DeviceState,
  IAccountData,
  IAppVersionInfo,
  ICustomList,
  IDevice,
  IDeviceRemoval,
  IDnsOptions,
  ILocation,
  IRelayListWithEndpointData,
  ISettings,
  liftConstraint,
  NewAccessMethodSetting,
  NewCustomList,
  ObfuscationSettings,
  RelaySettings,
  TunnelState,
} from '../shared/daemon-rpc-types';
import { messages, relayLocations } from '../shared/gettext';
import { IGuiSettingsState, SYSTEM_PREFERRED_LOCALE_KEY } from '../shared/gui-settings-state';
import {
  DaemonStatus,
  IChangelog,
  ICurrentAppVersionInfo,
  IHistoryObject,
} from '../shared/ipc-types';
import log, { ConsoleOutput } from '../shared/logging';
import { LogLevel } from '../shared/logging-types';
import { RoutePath } from '../shared/routes';
import { Scheduler } from '../shared/scheduler';
import AppRouter from './components/AppRouter';
import ErrorBoundary from './components/ErrorBoundary';
import KeyboardNavigation from './components/KeyboardNavigation';
import Lang from './components/Lang';
import MacOsScrollbarDetection from './components/MacOsScrollbarDetection';
import { ModalContainer } from './components/Modal';
import { AppContext } from './context';
import { Theme } from './lib/components';
import History, { TransitionType } from './lib/history';
import { loadTranslations } from './lib/load-translations';
import IpcOutput from './lib/logging';
import accountActions from './redux/account/actions';
import { appUpgradeActions } from './redux/app-upgrade/actions';
import connectionActions from './redux/connection/actions';
import settingsActions from './redux/settings/actions';
import configureStore from './redux/store';
import userInterfaceActions from './redux/userinterface/actions';
import versionActions from './redux/version/actions';

const IpcRendererEventChannel = window.ipc;

interface IPreferredLocaleDescriptor {
  name: string;
  code: string;
}

type LoginState = 'none' | 'logging in' | 'creating account' | 'too many devices';

const SUPPORTED_LOCALE_LIST = [
  { name: 'Dansk', code: 'da' },
  { name: 'Deutsch', code: 'de' },
  { name: 'English', code: 'en' },
  { name: 'Español', code: 'es' },
  { name: 'Suomi', code: 'fi' },
  { name: 'Français', code: 'fr' },
  { name: 'Italiano', code: 'it' },
  { name: '日本語', code: 'ja' },
  { name: '한국어', code: 'ko' },
  { name: 'မြန်မာဘာသာ', code: 'my' },
  { name: 'Nederlands', code: 'nl' },
  { name: 'Norsk', code: 'nb' },
  { name: 'Polski', code: 'pl' },
  { name: 'Português', code: 'pt' },
  { name: 'Русский', code: 'ru' },
  { name: 'Svenska', code: 'sv' },
  { name: 'ภาษาไทย', code: 'th' },
  { name: 'Türkçe', code: 'tr' },
  { name: '简体中文', code: 'zh-CN' },
  { name: '繁體中文', code: 'zh-TW' },
];

export default class AppRenderer {
  private history: History;
  private reduxStore = configureStore();
  private reduxActions = {
    account: bindActionCreators(accountActions, this.reduxStore.dispatch),
    appUpgrade: bindActionCreators(appUpgradeActions, this.reduxStore.dispatch),
    connection: bindActionCreators(connectionActions, this.reduxStore.dispatch),
    settings: bindActionCreators(settingsActions, this.reduxStore.dispatch),
    version: bindActionCreators(versionActions, this.reduxStore.dispatch),
    userInterface: bindActionCreators(userInterfaceActions, this.reduxStore.dispatch),
  };

  private location?: Partial<ILocation>;
  private relayList?: IRelayListWithEndpointData;
  private tunnelState!: TunnelState;
  private settings!: ISettings;
  private deviceState?: DeviceState;
  private loginState: LoginState = 'none';
  private previousLoginState: LoginState = 'none';
  private connectedToDaemon = false;

  private loginScheduler = new Scheduler();
  private expiryScheduler = new Scheduler();

  constructor() {
    log.addOutput(new ConsoleOutput(LogLevel.debug));
    log.addOutput(new IpcOutput(LogLevel.debug));

    IpcRendererEventChannel.window.listenShape((windowShapeParams) => {
      if (typeof windowShapeParams.arrowPosition === 'number') {
        this.reduxActions.userInterface.updateWindowArrowPosition(windowShapeParams.arrowPosition);
      }
    });

    IpcRendererEventChannel.daemon.listenConnected(() => {
      void this.onDaemonConnected();
    });

    IpcRendererEventChannel.daemon.listenDisconnected(() => {
      this.onDaemonDisconnected();
    });

    IpcRendererEventChannel.daemon.listenIsPerformingPostUpgrade((isPerformingPostUpgrade) => {
      this.setIsPerformingPostUpgrade(isPerformingPostUpgrade);
    });

    IpcRendererEventChannel.daemon.listenDaemonAllowed((daemonAllowed) => {
      this.reduxActions.userInterface.setDaemonAllowed(daemonAllowed);
    });

    IpcRendererEventChannel.account.listen((newAccountData?: IAccountData) => {
      this.setAccountExpiry(newAccountData?.expiry);
    });

    IpcRendererEventChannel.account.listenDevice((deviceEvent) => {
      this.handleDeviceEvent(deviceEvent);
    });

    IpcRendererEventChannel.account.listenDevices((devices) => {
      this.reduxActions.account.updateDevices(devices);
    });

    IpcRendererEventChannel.accountHistory.listen((newAccountHistory?: AccountNumber) => {
      this.setAccountHistory(newAccountHistory);
    });

    IpcRendererEventChannel.tunnel.listen((newState: TunnelState) => {
      this.setTunnelState(newState);
      this.updateBlockedState(newState);
    });

    IpcRendererEventChannel.settings.listen((newSettings: ISettings) => {
      this.setSettings(newSettings);
      this.updateBlockedState(this.tunnelState);
    });

    IpcRendererEventChannel.settings.listenApiAccessMethodSettingChange((setting) => {
      this.setCurrentApiAccessMethod(setting);
    });

    IpcRendererEventChannel.relays.listen((relayListPair: IRelayListWithEndpointData) => {
      this.setRelayListPair(relayListPair);
    });

    IpcRendererEventChannel.daemon.listenTryStartEvent((status: DaemonStatus) => {
      this.reduxActions.userInterface.setDaemonStatus(status);
    });

    IpcRendererEventChannel.app.listenUpgradeEvent((appUpgradeEvent) => {
      this.reduxActions.appUpgrade.setAppUpgradeEvent(appUpgradeEvent);

      if (appUpgradeEvent.type === 'APP_UPGRADE_STATUS_DOWNLOAD_PROGRESS') {
        this.reduxActions.appUpgrade.setLastProgress(appUpgradeEvent.progress);
      }

      // Ensure progress is updated to 100%, since the daemon doesn't send the last event
      if (
        appUpgradeEvent.type === 'APP_UPGRADE_STATUS_VERIFYING_INSTALLER' ||
        appUpgradeEvent.type === 'APP_UPGRADE_STATUS_VERIFIED_INSTALLER'
      ) {
        this.reduxActions.appUpgrade.setLastProgress(100);
      }

      // Check if the installer should be started automatically
      this.appUpgradeMaybeStartInstaller();
    });

    IpcRendererEventChannel.app.listenUpgradeError((appUpgradeError) => {
      this.reduxActions.appUpgrade.setAppUpgradeError(appUpgradeError);
    });

    IpcRendererEventChannel.currentVersion.listen((currentVersion: ICurrentAppVersionInfo) => {
      this.setCurrentVersion(currentVersion);
    });

    IpcRendererEventChannel.upgradeVersion.listen((upgradeVersion: IAppVersionInfo) => {
      const reduxStore = this.reduxStore.getState();

      const currentSuggestedUpgradeVersion = reduxStore.version.suggestedUpgrade?.version;
      const newSuggestedUpgradeVersion = upgradeVersion.suggestedUpgrade?.version;
      if (
        currentSuggestedUpgradeVersion &&
        currentSuggestedUpgradeVersion !== newSuggestedUpgradeVersion
      ) {
        this.reduxActions.appUpgrade.resetAppUpgrade();
      }

      this.setUpgradeVersion(upgradeVersion);

      // Check if the installer should be started automatically
      this.appUpgradeMaybeStartInstaller();
    });

    IpcRendererEventChannel.guiSettings.listen((guiSettings: IGuiSettingsState) => {
      this.setGuiSettings(guiSettings);
    });

    IpcRendererEventChannel.autoStart.listen((autoStart: boolean) => {
      this.storeAutoStart(autoStart);
    });

    IpcRendererEventChannel.splitTunneling.listen((applications: ISplitTunnelingApplication[]) => {
      this.reduxActions.settings.setSplitTunnelingApplications(applications);
    });

    IpcRendererEventChannel.window.listenFocus((focus: boolean) => {
      this.reduxActions.userInterface.setWindowFocused(focus);
    });

    IpcRendererEventChannel.window.listenMacOsScrollbarVisibility((visibility) => {
      this.reduxActions.userInterface.setMacOsScrollbarVisibility(visibility);
    });

    IpcRendererEventChannel.navigation.listenReset(() => this.history.pop(true));

    IpcRendererEventChannel.app.listenOpenRoute((route: RoutePath) => {
      this.history.push({
        routePath: route,
      });
    });

    // Request the initial state from the main process
    const initialState = IpcRendererEventChannel.state.get();

    this.setLocale(initialState.translations.locale);
    loadTranslations(
      messages,
      initialState.translations.locale,
      initialState.translations.messages,
    );
    loadTranslations(
      relayLocations,
      initialState.translations.locale,
      initialState.translations.relayLocations,
    );

    this.setSettings(initialState.settings);
    this.setIsPerformingPostUpgrade(initialState.isPerformingPostUpgrade);

    if (initialState.daemonAllowed !== undefined) {
      this.reduxActions.userInterface.setDaemonAllowed(initialState.daemonAllowed);
    }

    if (initialState.deviceState) {
      const deviceState = initialState.deviceState;
      this.handleDeviceEvent(
        { type: deviceState.type, deviceState } as DeviceEvent,
        initialState.navigationHistory !== undefined,
      );
    }
    // Login state and account needs to be set before expiry.
    this.setAccountExpiry(initialState.accountData?.expiry);

    this.setAccountHistory(initialState.accountHistory);
    this.setTunnelState(initialState.tunnelState);
    this.updateBlockedState(initialState.tunnelState);

    this.setRelayListPair(initialState.relayList);
    this.setCurrentVersion(initialState.currentVersion);
    this.setUpgradeVersion(initialState.upgradeVersion);
    this.setGuiSettings(initialState.guiSettings);
    this.storeAutoStart(initialState.autoStart);
    this.setChangelog(initialState.changelog);
    this.setCurrentApiAccessMethod(initialState.currentApiAccessMethod);
    this.reduxActions.userInterface.setIsMacOs13OrNewer(initialState.isMacOs13OrNewer);

    if (initialState.macOsScrollbarVisibility !== undefined) {
      this.reduxActions.userInterface.setMacOsScrollbarVisibility(
        initialState.macOsScrollbarVisibility,
      );
    }

    if (initialState.isConnected) {
      void this.onDaemonConnected();
    }

    this.checkContentHeight(false);
    window.addEventListener('resize', () => {
      this.checkContentHeight(true);
    });

    if (initialState.splitTunnelingApplications) {
      this.reduxActions.settings.setSplitTunnelingApplications(
        initialState.splitTunnelingApplications,
      );
    }

    this.updateLocation();

    if (initialState.navigationHistory) {
      // Set last action to POP to trigger automatic scrolling to saved coordinates.
      initialState.navigationHistory.lastAction = 'POP';
      this.history = History.fromSavedHistory(initialState.navigationHistory);
    } else {
      const navigationBase = this.getNavigationBase();
      this.history = new History(navigationBase);
    }

    if (window.env.e2e) {
      // Make the current location available to the tests if running e2e tests
      window.e2e = { location: this.history.location.pathname };
    }
  }

  public renderView() {
    return (
      <StrictMode>
        <AppContext.Provider value={{ app: this }}>
          <Provider store={this.reduxStore}>
            <StyleSheetManager enableVendorPrefixes>
              <Lang>
                <Router history={this.history.asHistory}>
                  <Theme>
                    <ErrorBoundary>
                      <ModalContainer>
                        <KeyboardNavigation>
                          <AppRouter />
                        </KeyboardNavigation>
                        {window.env.platform === 'darwin' && <MacOsScrollbarDetection />}
                      </ModalContainer>
                    </ErrorBoundary>
                  </Theme>
                </Router>
              </Lang>
            </StyleSheetManager>
          </Provider>
        </AppContext.Provider>
      </StrictMode>
    );
  }

  public submitVoucher = (code: string) => IpcRendererEventChannel.account.submitVoucher(code);
  public updateAccountData = () => IpcRendererEventChannel.account.updateData();
  public removeDevice = (device: IDeviceRemoval) =>
    IpcRendererEventChannel.account.removeDevice(device);
  public connectTunnel = () => IpcRendererEventChannel.tunnel.connect();
  public disconnectTunnel = () => IpcRendererEventChannel.tunnel.disconnect();
  public reconnectTunnel = () => IpcRendererEventChannel.tunnel.reconnect();
  public setRelaySettings = (relaySettings: RelaySettings) =>
    IpcRendererEventChannel.settings.setRelaySettings(relaySettings);
  public updateBridgeSettings = (bridgeSettings: BridgeSettings) =>
    IpcRendererEventChannel.settings.updateBridgeSettings(bridgeSettings);
  public setDnsOptions = (dnsOptions: IDnsOptions) =>
    IpcRendererEventChannel.settings.setDnsOptions(dnsOptions);
  public clearAccountHistory = () => IpcRendererEventChannel.accountHistory.clear();
  public setAutoConnect = (value: boolean) =>
    IpcRendererEventChannel.guiSettings.setAutoConnect(value);
  public setEnableSystemNotifications = (value: boolean) =>
    IpcRendererEventChannel.guiSettings.setEnableSystemNotifications(value);
  public setStartMinimized = (value: boolean) =>
    IpcRendererEventChannel.guiSettings.setStartMinimized(value);
  public setMonochromaticIcon = (value: boolean) =>
    IpcRendererEventChannel.guiSettings.setMonochromaticIcon(value);
  public setUnpinnedWindow = (value: boolean) =>
    IpcRendererEventChannel.guiSettings.setUnpinnedWindow(value);
  public getLinuxSplitTunnelingApplications = () =>
    IpcRendererEventChannel.linuxSplitTunneling.getApplications();
  public launchExcludedApplication = (application: ILinuxSplitTunnelingApplication | string) =>
    IpcRendererEventChannel.linuxSplitTunneling.launchApplication(application);
  public setSplitTunnelingState = (state: boolean) =>
    IpcRendererEventChannel.splitTunneling.setState(state);
  public addSplitTunnelingApplication = (application: string | ISplitTunnelingApplication) =>
    IpcRendererEventChannel.splitTunneling.addApplication(application);
  public forgetManuallyAddedSplitTunnelingApplication = (application: ISplitTunnelingApplication) =>
    IpcRendererEventChannel.splitTunneling.forgetManuallyAddedApplication(application);
  public needFullDiskPermissions = () =>
    IpcRendererEventChannel.macOsSplitTunneling.needFullDiskPermissions();
  public setObfuscationSettings = (obfuscationSettings: ObfuscationSettings) =>
    IpcRendererEventChannel.settings.setObfuscationSettings(obfuscationSettings);
  public setEnableDaita = (value: boolean) =>
    IpcRendererEventChannel.settings.setEnableDaita(value);
  public setDaitaDirectOnly = (value: boolean) =>
    IpcRendererEventChannel.settings.setDaitaDirectOnly(value);
  public collectProblemReport = (toRedact: string | undefined) =>
    IpcRendererEventChannel.problemReport.collectLogs(toRedact);
  public viewLog = (path: string) => IpcRendererEventChannel.problemReport.viewLog(path);
  public quit = () => IpcRendererEventChannel.app.quit();
  public openUrl = (url: Url) => IpcRendererEventChannel.app.openUrl(url);
  public getPathBaseName = (path: string) => IpcRendererEventChannel.app.getPathBaseName(path);
  public showOpenDialog = (options: Electron.OpenDialogOptions) =>
    IpcRendererEventChannel.app.showOpenDialog(options);
  public createCustomList = (newCustomList: NewCustomList) =>
    IpcRendererEventChannel.customLists.createCustomList(newCustomList);
  public deleteCustomList = (id: string) =>
    IpcRendererEventChannel.customLists.deleteCustomList(id);
  public updateCustomList = (customList: ICustomList) =>
    IpcRendererEventChannel.customLists.updateCustomList(customList);
  public addApiAccessMethod = (method: NewAccessMethodSetting) =>
    IpcRendererEventChannel.settings.addApiAccessMethod(method);
  public updateApiAccessMethod = (method: AccessMethodSetting) =>
    IpcRendererEventChannel.settings.updateApiAccessMethod(method);
  public removeApiAccessMethod = (id: string) =>
    IpcRendererEventChannel.settings.removeApiAccessMethod(id);
  public setApiAccessMethod = (id: string) =>
    IpcRendererEventChannel.settings.setApiAccessMethod(id);
  public testApiAccessMethodById = (id: string) =>
    IpcRendererEventChannel.settings.testApiAccessMethodById(id);
  public testCustomApiAccessMethod = (method: CustomProxy) =>
    IpcRendererEventChannel.settings.testCustomApiAccessMethod(method);
  public importSettingsFile = (path: string) => IpcRendererEventChannel.settings.importFile(path);
  public importSettingsText = (text: string) => IpcRendererEventChannel.settings.importText(text);
  public clearAllRelayOverrides = () => IpcRendererEventChannel.settings.clearAllRelayOverrides();
  public getMapData = () => IpcRendererEventChannel.map.getData();
  public setAnimateMap = (displayMap: boolean): void =>
    IpcRendererEventChannel.guiSettings.setAnimateMap(displayMap);
  public daemonPrepareRestart = (shutdown: boolean): void => {
    IpcRendererEventChannel.daemon.prepareRestart(shutdown);
  };

  public tryStartDaemon = () => {
    if (window.env.platform === 'win32') IpcRendererEventChannel.daemon.tryStart();
  };

  public appUpgrade = () => {
    const reduxState = this.reduxStore.getState();
    const appUpgradeError = reduxState.appUpgrade.error;

    if (appUpgradeError) {
      this.reduxActions.appUpgrade.resetAppUpgradeError();
    }

    this.reduxActions.appUpgrade.setAppUpgradeEvent({
      type: 'APP_UPGRADE_STATUS_DOWNLOAD_INITIATED',
    });

    IpcRendererEventChannel.app.upgrade();
  };
  public appUpgradeAbort = () => IpcRendererEventChannel.app.upgradeAbort();
  public appUpgradeInstallerStart = () => {
    const reduxState = this.reduxStore.getState();
    const verifiedInstallerPath = reduxState.version.suggestedUpgrade?.verifiedInstallerPath;
    const hasVerifiedInstallerPath =
      typeof verifiedInstallerPath === 'string' && verifiedInstallerPath.length > 0;

    // Ensure we have a the path to the verified installer and that we are not already trying
    // to start the installer.
    if (hasVerifiedInstallerPath) {
      this.reduxActions.appUpgrade.setAppUpgradeEvent({
        type: 'APP_UPGRADE_STATUS_MANUAL_STARTING_INSTALLER',
      });
      this.reduxActions.appUpgrade.resetAppUpgradeError();

      IpcRendererEventChannel.app.upgradeInstallerStart(verifiedInstallerPath);
    }
  };

  public login = async (accountNumber: AccountNumber) => {
    const actions = this.reduxActions;
    actions.account.startLogin(accountNumber);

    log.info('Logging in');

    this.previousLoginState = this.loginState;
    this.loginState = 'logging in';

    const response = await IpcRendererEventChannel.account.login(accountNumber);
    if (response?.type === 'error') {
      if (response.error === 'too-many-devices') {
        try {
          await this.fetchDevices(accountNumber);

          actions.account.loginTooManyDevices();
          this.loginState = 'too many devices';

          this.history.reset(RoutePath.tooManyDevices, { transition: TransitionType.push });
        } catch {
          log.error('Failed to fetch device list');
          actions.account.loginFailed('list-devices');
        }
      } else {
        actions.account.loginFailed(response.error);
      }
    }
  };

  public cancelLogin = (): void => {
    const reduxAccount = this.reduxActions.account;
    reduxAccount.loggedOut();
    this.loginState = 'none';
  };

  public logout = async (transition = TransitionType.dismiss) => {
    try {
      this.history.reset(RoutePath.login, { transition });
      await IpcRendererEventChannel.account.logout();
    } catch (e) {
      const error = e as Error;
      log.info('Failed to logout: ', error.message);
    }
  };

  public leaveRevokedDevice = async () => {
    await this.logout(TransitionType.pop);
    await this.disconnectTunnel();
  };

  public createNewAccount = async () => {
    log.info('Creating account');

    const actions = this.reduxActions;
    actions.account.startCreateAccount();
    this.loginState = 'creating account';

    try {
      await IpcRendererEventChannel.account.create();
      this.redirectToConnect();
    } catch (e) {
      const error = e as Error;
      actions.account.createAccountFailed(error);
    }
  };

  public fetchDevices = async (accountNumber: AccountNumber): Promise<Array<IDevice>> => {
    const devices = await IpcRendererEventChannel.account.listDevices(accountNumber);
    this.reduxActions.account.updateDevices(devices);
    return devices;
  };

  public openUrlWithAuth = async (url: Url): Promise<void> => {
    let token = '';
    try {
      token = await IpcRendererEventChannel.account.getWwwAuthToken();
    } catch (e) {
      const error = e as Error;
      log.error(`Failed to get the WWW auth token: ${error.message}`);
    }
    void this.openUrl(`${url}?token=${token}`);
  };

  public setAllowLan = async (allowLan: boolean) => {
    const actions = this.reduxActions;
    await IpcRendererEventChannel.settings.setAllowLan(allowLan);
    actions.settings.updateAllowLan(allowLan);
  };

  public setShowBetaReleases = async (showBetaReleases: boolean) => {
    const actions = this.reduxActions;
    await IpcRendererEventChannel.settings.setShowBetaReleases(showBetaReleases);
    actions.settings.updateShowBetaReleases(showBetaReleases);
  };

  public setEnableIpv6 = async (enableIpv6: boolean) => {
    const actions = this.reduxActions;
    await IpcRendererEventChannel.settings.setEnableIpv6(enableIpv6);
    actions.settings.updateEnableIpv6(enableIpv6);
  };

  public setBridgeState = async (bridgeState: BridgeState) => {
    const actions = this.reduxActions;
    await IpcRendererEventChannel.settings.setBridgeState(bridgeState);
    actions.settings.updateBridgeState(bridgeState);
  };

  public setBlockWhenDisconnected = async (blockWhenDisconnected: boolean) => {
    const actions = this.reduxActions;
    await IpcRendererEventChannel.settings.setBlockWhenDisconnected(blockWhenDisconnected);
    actions.settings.updateBlockWhenDisconnected(blockWhenDisconnected);
  };

  public setOpenVpnMssfix = async (mssfix?: number) => {
    const actions = this.reduxActions;
    actions.settings.updateOpenVpnMssfix(mssfix);
    await IpcRendererEventChannel.settings.setOpenVpnMssfix(mssfix);
  };

  public setWireguardMtu = async (mtu?: number) => {
    const actions = this.reduxActions;
    actions.settings.updateWireguardMtu(mtu);
    await IpcRendererEventChannel.settings.setWireguardMtu(mtu);
  };

  public setWireguardQuantumResistant = async (quantumResistant?: boolean) => {
    const actions = this.reduxActions;
    actions.settings.updateWireguardQuantumResistant(quantumResistant);
    await IpcRendererEventChannel.settings.setWireguardQuantumResistant(quantumResistant);
  };

  public setAutoStart = (autoStart: boolean): Promise<void> => {
    this.storeAutoStart(autoStart);

    return IpcRendererEventChannel.autoStart.set(autoStart);
  };

  public getSplitTunnelingApplications(updateCache = false) {
    return IpcRendererEventChannel.splitTunneling.getApplications(updateCache);
  }

  public removeSplitTunnelingApplication(application: ISplitTunnelingApplication) {
    void IpcRendererEventChannel.splitTunneling.removeApplication(application);
  }

  public async showLaunchDaemonSettings() {
    await IpcRendererEventChannel.app.showLaunchDaemonSettings();
  }

  public showFullDiskAccessSettings = async () => {
    await IpcRendererEventChannel.app.showFullDiskAccessSettings();
  };

  public async sendProblemReport(
    email: string,
    message: string,
    savedReportId: string,
  ): Promise<void> {
    await IpcRendererEventChannel.problemReport.sendReport({ email, message, savedReportId });
  }

  public getPreferredLocaleList(): IPreferredLocaleDescriptor[] {
    return [
      {
        // TRANSLATORS: The option that represents the active operating system language in the
        // TRANSLATORS: user interface language selection list.
        name: messages.gettext('System default'),
        code: SYSTEM_PREFERRED_LOCALE_KEY,
      },
      ...SUPPORTED_LOCALE_LIST.sort((a, b) => a.name.localeCompare(b.name)),
    ];
  }

  public setPreferredLocale = async (preferredLocale: string): Promise<void> => {
    const translations =
      await IpcRendererEventChannel.guiSettings.setPreferredLocale(preferredLocale);

    // set current locale
    this.setLocale(translations.locale);

    // load translations for new locale
    loadTranslations(messages, translations.locale, translations.messages);
    loadTranslations(relayLocations, translations.locale, translations.relayLocations);
  };

  public getPreferredLocaleDisplayName = (localeCode: string): string => {
    const preferredLocale = this.getPreferredLocaleList().find((item) => item.code === localeCode);

    return preferredLocale ? preferredLocale.name : '';
  };

  public setDisplayedChangelog = (): void => {
    IpcRendererEventChannel.currentVersion.displayedChangelog();
  };

  public setDismissedUpgrade = (): void => {
    IpcRendererEventChannel.upgradeVersion.dismissedUpgrade(
      this.reduxStore.getState().version.suggestedUpgrade?.version ?? '',
    );
  };

  public setNavigationHistory(history: IHistoryObject) {
    IpcRendererEventChannel.navigation.setHistory(history);

    if (window.env.e2e) {
      window.e2e.location = history.entries[history.index].pathname;
    }
  }

  // If the installer has just been downloaded and verified we want to automatically
  // start the installer if the window is focused.
  private appUpgradeMaybeStartInstaller() {
    const reduxState = this.reduxStore.getState();

    const appUpgradeEvent = reduxState.appUpgrade.event;
    const verifiedInstallerPath = reduxState.version.suggestedUpgrade?.verifiedInstallerPath;
    const windowFocused = reduxState.userInterface.windowFocused;

    const hasVerifiedInstallerPath =
      typeof verifiedInstallerPath === 'string' && verifiedInstallerPath.length > 0;

    if (
      hasVerifiedInstallerPath &&
      appUpgradeEvent?.type === 'APP_UPGRADE_STATUS_VERIFIED_INSTALLER'
    ) {
      // Only trigger the installer if the window is focused
      if (windowFocused) {
        this.reduxActions.appUpgrade.setAppUpgradeEvent({
          type: 'APP_UPGRADE_STATUS_AUTOMATIC_STARTING_INSTALLER',
        });
        IpcRendererEventChannel.app.upgradeInstallerStart(verifiedInstallerPath);
      } else {
        // Otherwise, flag this as requiring manual start
        this.reduxActions.appUpgrade.setAppUpgradeEvent({
          type: 'APP_UPGRADE_STATUS_MANUAL_START_INSTALLER',
        });
      }
    }
  }

  private isLoggedIn(): boolean {
    return this.deviceState?.type === 'logged in';
  }

  // Make sure that the content height is correct and log if it isn't. This is mostly for debugging
  // purposes since there's a bug in Electron that causes the app height to be another value than
  // the one we have set.
  // https://github.com/electron/electron/issues/28777
  private checkContentHeight(resize: boolean): void {
    const expectedContentHeight = 568;
    const contentHeight = window.innerHeight;
    if (contentHeight !== expectedContentHeight) {
      log.verbose(
        resize ? 'Resize:' : 'Initial:',
        `Wrong content height: ${contentHeight}, expected ${expectedContentHeight}`,
      );
    }
  }

  private redirectToConnect() {
    // Redirect the user after some time to allow for the 'Logged in' screen to be visible
    this.loginScheduler.schedule(() => this.resetNavigation(), 1000);
  }

  private setLocale(locale: string) {
    this.reduxActions.userInterface.updateLocale(locale);
  }

  private setReduxRelaySettings(relaySettings: RelaySettings) {
    const actions = this.reduxActions;

    if ('normal' in relaySettings) {
      const {
        location,
        openvpnConstraints,
        wireguardConstraints,
        tunnelProtocol,
        providers,
        ownership,
      } = relaySettings.normal;

      actions.settings.updateRelay({
        normal: {
          location: liftConstraint(location),
          providers,
          ownership,
          openvpn: {
            port: liftConstraint(openvpnConstraints.port),
            protocol: liftConstraint(openvpnConstraints.protocol),
          },
          wireguard: {
            port: liftConstraint(wireguardConstraints.port),
            ipVersion: liftConstraint(wireguardConstraints.ipVersion),
            useMultihop: wireguardConstraints.useMultihop,
            entryLocation: liftConstraint(wireguardConstraints.entryLocation),
          },
          tunnelProtocol,
        },
      });
    } else if ('customTunnelEndpoint' in relaySettings) {
      const customTunnelEndpoint = relaySettings.customTunnelEndpoint;
      const config = customTunnelEndpoint.config;

      if ('openvpn' in config) {
        actions.settings.updateRelay({
          customTunnelEndpoint: {
            host: customTunnelEndpoint.host,
            port: config.openvpn.endpoint.port,
            protocol: config.openvpn.endpoint.protocol,
          },
        });
      } else if ('wireguard' in config) {
        // TODO: handle wireguard
      }
    }
  }

  private setBridgeSettings(bridgeSettings: BridgeSettings) {
    const actions = this.reduxActions;

    actions.settings.updateBridgeSettings({
      type: bridgeSettings.type,
      normal: {
        location: liftConstraint(bridgeSettings.normal.location),
        providers: bridgeSettings.normal.providers,
        ownership: bridgeSettings.normal.ownership,
      },
      custom: bridgeSettings.custom,
    });
  }

  private onDaemonConnected() {
    this.connectedToDaemon = true;
    this.reduxActions.userInterface.setConnectedToDaemon(true);
    this.reduxActions.userInterface.setDaemonAllowed(true);
    this.reduxActions.userInterface.setDaemonStatus('running');
    this.resetNavigation();
  }

  private onDaemonDisconnected() {
    this.connectedToDaemon = false;
    this.reduxActions.userInterface.setConnectedToDaemon(false);
    this.reduxActions.userInterface.setDaemonStatus('stopped');
    this.resetNavigation();
  }

  private resetNavigation(replaceRoot?: boolean) {
    if (this.history) {
      const pathname = this.history.location.pathname as RoutePath;
      const nextPath = this.getNavigationBase() as RoutePath;

      if (pathname !== nextPath) {
        const transition = this.getNavigationTransition(pathname, nextPath);
        if (replaceRoot) {
          this.history.replaceRoot(nextPath, { transition });
        } else {
          this.history.reset(nextPath, { transition });
        }
      }
    }
  }

  private getNavigationTransition(prevPath: RoutePath, nextPath: RoutePath) {
    // First level contains the possible next locations and the second level contains the
    // possible current locations.
    const navigationTransitions: Partial<
      Record<RoutePath, Partial<Record<RoutePath | '*', TransitionType>>>
    > = {
      [RoutePath.launch]: {
        [RoutePath.login]: TransitionType.pop,
        [RoutePath.main]: TransitionType.pop,
        '*': TransitionType.dismiss,
      },
      [RoutePath.login]: {
        [RoutePath.launch]: TransitionType.push,
        [RoutePath.main]: TransitionType.pop,
        [RoutePath.deviceRevoked]: TransitionType.pop,
        '*': TransitionType.dismiss,
      },
      [RoutePath.main]: {
        [RoutePath.launch]: TransitionType.push,
        [RoutePath.login]: TransitionType.push,
        [RoutePath.tooManyDevices]: TransitionType.push,
        '*': TransitionType.dismiss,
      },
      [RoutePath.expired]: {
        [RoutePath.launch]: TransitionType.push,
        [RoutePath.login]: TransitionType.push,
        [RoutePath.tooManyDevices]: TransitionType.push,
        '*': TransitionType.dismiss,
      },
      [RoutePath.timeAdded]: {
        [RoutePath.expired]: TransitionType.push,
        [RoutePath.redeemVoucher]: TransitionType.push,
        '*': TransitionType.dismiss,
      },
      [RoutePath.deviceRevoked]: {
        '*': TransitionType.pop,
      },
    };

    return navigationTransitions[nextPath]?.[prevPath] ?? navigationTransitions[nextPath]?.['*'];
  }

  private getNavigationBase(): RoutePath {
    if (this.connectedToDaemon && this.deviceState !== undefined) {
      const loginState = this.reduxStore.getState().account.status;
      const deviceRevoked = loginState.type === 'none' && loginState.deviceRevoked;

      if (deviceRevoked) {
        return RoutePath.deviceRevoked;
      } else if (!this.isLoggedIn()) {
        return RoutePath.login;
      } else if (loginState.type === 'ok' && loginState.expiredState === 'expired') {
        return RoutePath.expired;
      } else if (loginState.type === 'ok' && loginState.expiredState === 'time_added') {
        return RoutePath.timeAdded;
      } else {
        return RoutePath.main;
      }
    } else {
      return RoutePath.launch;
    }
  }

  private setAccountHistory(accountHistory?: AccountNumber) {
    this.reduxActions.account.updateAccountHistory(accountHistory);
  }

  private setTunnelState(tunnelState: TunnelState) {
    const actions = this.reduxActions;

    log.verbose(`Tunnel state: ${tunnelState.state}`);

    this.tunnelState = tunnelState;

    batch(() => {
      switch (tunnelState.state) {
        case 'connecting':
          actions.connection.connecting(tunnelState.details, tunnelState.featureIndicators);
          break;

        case 'connected':
          actions.connection.connected(tunnelState.details, tunnelState.featureIndicators);
          break;

        case 'disconnecting':
          actions.connection.disconnecting(tunnelState.details);
          break;

        case 'disconnected':
          actions.connection.disconnected(tunnelState.lockedDown);
          break;

        case 'error':
          actions.connection.blocked(tunnelState.details);
          break;
      }

      // Update the location when entering a new tunnel state since it's likely changed.
      this.updateLocation();
    });
  }

  private setSettings(newSettings: ISettings) {
    this.settings = newSettings;

    const reduxSettings = this.reduxActions.settings;

    reduxSettings.updateAllowLan(newSettings.allowLan);
    reduxSettings.updateEnableIpv6(newSettings.tunnelOptions.generic.enableIpv6);
    reduxSettings.updateBlockWhenDisconnected(newSettings.blockWhenDisconnected);
    reduxSettings.updateShowBetaReleases(newSettings.showBetaReleases);
    reduxSettings.updateOpenVpnMssfix(newSettings.tunnelOptions.openvpn.mssfix);
    reduxSettings.updateWireguardMtu(newSettings.tunnelOptions.wireguard.mtu);
    reduxSettings.updateWireguardQuantumResistant(
      newSettings.tunnelOptions.wireguard.quantumResistant,
    );
    reduxSettings.updateWireguardDaita(newSettings.tunnelOptions.wireguard.daita);
    reduxSettings.updateBridgeState(newSettings.bridgeState);
    reduxSettings.updateDnsOptions(newSettings.tunnelOptions.dns);
    reduxSettings.updateSplitTunnelingState(newSettings.splitTunnel.enableExclusions);
    reduxSettings.updateObfuscationSettings(newSettings.obfuscationSettings);
    reduxSettings.updateCustomLists(newSettings.customLists);
    reduxSettings.updateApiAccessMethods(newSettings.apiAccessMethods);
    reduxSettings.updateRelayOverrides(newSettings.relayOverrides);

    this.setReduxRelaySettings(newSettings.relaySettings);
    this.setBridgeSettings(newSettings.bridgeSettings);
  }

  private setIsPerformingPostUpgrade(isPerformingPostUpgrade: boolean) {
    this.reduxActions.userInterface.setIsPerformingPostUpgrade(isPerformingPostUpgrade);
  }

  private updateBlockedState(tunnelState: TunnelState) {
    const actions = this.reduxActions.connection;
    switch (tunnelState.state) {
      case 'connecting':
        actions.updateBlockState(true);
        break;

      case 'connected':
        actions.updateBlockState(false);
        break;

      case 'disconnected':
        actions.updateBlockState(tunnelState.lockedDown);
        break;

      case 'disconnecting':
        actions.updateBlockState(true);
        break;

      case 'error':
        actions.updateBlockState(!tunnelState.details.blockingError);
        break;
    }
  }

  private handleDeviceEvent(deviceEvent: DeviceEvent, preventRedirectToConnect?: boolean) {
    const reduxAccount = this.reduxActions.account;

    this.deviceState = deviceEvent.deviceState;

    switch (deviceEvent.type) {
      case 'logged in': {
        const accountNumber = deviceEvent.deviceState.accountAndDevice.accountNumber;
        const device = deviceEvent.deviceState.accountAndDevice.device;

        switch (this.loginState) {
          case 'none':
            reduxAccount.loggedIn(accountNumber, device);
            this.resetNavigation();
            break;
          case 'logging in':
            reduxAccount.loggedIn(accountNumber, device);

            if (this.previousLoginState === 'too many devices') {
              this.resetNavigation();
            } else if (!preventRedirectToConnect) {
              this.redirectToConnect();
            }
            break;
          case 'creating account':
            reduxAccount.accountCreated(accountNumber, device, new Date().toISOString());
            break;
        }
        break;
      }
      case 'logged out':
        this.loginScheduler.cancel();
        reduxAccount.loggedOut();
        this.resetNavigation();
        break;
      case 'revoked': {
        this.loginScheduler.cancel();
        reduxAccount.deviceRevoked();
        this.resetNavigation();
        break;
      }
    }

    this.previousLoginState = this.loginState;
    this.loginState = 'none';
  }

  private setLocation(location: Partial<ILocation>) {
    this.location = location;
    this.propagateLocationToRedux();
  }

  private propagateLocationToRedux() {
    if (this.location) {
      this.reduxActions.connection.newLocation(this.location);
    }
  }

  private setRelayListPair(relayListPair?: IRelayListWithEndpointData) {
    this.relayList = relayListPair;
    this.propagateRelayListPairToRedux();
  }

  private propagateRelayListPairToRedux() {
    if (this.relayList) {
      this.reduxActions.settings.updateRelayLocations(this.relayList.relayList.countries);
      this.reduxActions.settings.updateWireguardEndpointData(this.relayList.wireguardEndpointData);
    }
  }

  private setCurrentVersion(versionInfo: ICurrentAppVersionInfo) {
    this.reduxActions.version.updateVersion(
      versionInfo.gui,
      versionInfo.isConsistent,
      versionInfo.isBeta,
    );
  }

  private setUpgradeVersion(upgradeVersion: IAppVersionInfo) {
    this.reduxActions.version.updateLatest(upgradeVersion);
  }

  private setGuiSettings(guiSettings: IGuiSettingsState) {
    this.reduxActions.settings.updateGuiSettings(guiSettings);
  }

  private setAccountExpiry(expiry?: string) {
    const state = this.reduxStore.getState();
    const previousExpiry = state.account.expiry;

    this.expiryScheduler.cancel();

    if (expiry !== undefined) {
      const expired = hasExpired(expiry);

      // Set state to expired when expiry date passes.
      if (!expired && closeToExpiry(expiry)) {
        const delay = new Date(expiry).getTime() - Date.now() + 1;
        this.expiryScheduler.schedule(() => this.handleExpiry(expiry, true), delay);
      }

      if (expiry !== previousExpiry) {
        this.handleExpiry(expiry, expired);
      }
    } else {
      this.handleExpiry(expiry);
    }
  }

  private handleExpiry(expiry?: string, expired?: boolean) {
    const state = this.reduxStore.getState();
    this.reduxActions.account.updateAccountExpiry(expiry);

    if (
      expiry !== undefined &&
      state.account.status.type === 'ok' &&
      ((state.account.status.expiredState === undefined && expired) ||
        (state.account.status.expiredState === 'expired' && !expired)) &&
      // If the login navigation is already scheduled no navigation is needed
      !this.loginScheduler.isRunning
    ) {
      this.resetNavigation(true);
    }
  }

  private storeAutoStart(autoStart: boolean) {
    this.reduxActions.settings.updateAutoStart(autoStart);
  }

  private setChangelog(changelog: IChangelog) {
    this.reduxActions.userInterface.setChangelog(changelog);
  }

  private updateLocation() {
    switch (this.tunnelState.state) {
      case 'disconnected':
        if (this.tunnelState.location) {
          this.setLocation(this.tunnelState.location);
        }
        break;
      case 'disconnecting':
        if (this.tunnelState.location) {
          this.setLocation(this.tunnelState.location);
        } else {
          // If there's no previous location while disconnecting we remove the location. We keep the
          // coordinates to prevent the map from jumping around.
          const { longitude, latitude } = this.reduxStore.getState().connection;
          this.setLocation({ longitude, latitude });
        }
        break;
      case 'connecting':
      case 'connected': {
        this.setLocation(this.tunnelState.details?.location ?? this.getLocationFromConstraints());
        break;
      }
    }
  }

  private setCurrentApiAccessMethod(method?: AccessMethodSetting) {
    if (method) {
      this.reduxActions.settings.updateCurrentApiAccessMethod(method);
    }
  }

  private getLocationFromConstraints(): Partial<ILocation> {
    const state = this.reduxStore.getState();
    const coordinates = {
      longitude: state.connection.longitude,
      latitude: state.connection.latitude,
    };

    const relaySettings = this.settings.relaySettings;
    if ('normal' in relaySettings) {
      const location = relaySettings.normal.location;
      if (location !== 'any' && 'only' in location) {
        const constraint = location.only;
        const relayLocations = state.settings.relayLocations;

        if ('hostname' in constraint) {
          const country = relayLocations.find(({ code }) => constraint.country === code);
          const city = country?.cities.find(({ code }) => constraint.city === code);

          let entryHostname: string | undefined;
          const multihopConstraint = relaySettings.normal.wireguardConstraints.useMultihop;
          const entryLocationConstraint = relaySettings.normal.wireguardConstraints.entryLocation;
          if (
            multihopConstraint &&
            entryLocationConstraint !== 'any' &&
            'hostname' in entryLocationConstraint.only &&
            entryLocationConstraint.only.hostname.length === 3
          ) {
            entryHostname = entryLocationConstraint.only.hostname;
          }

          return {
            country: country?.name,
            city: city?.name,
            hostname: constraint.hostname,
            entryHostname,
            ...coordinates,
          };
        } else if ('city' in constraint) {
          const country = relayLocations.find(({ code }) => constraint.country === code);
          const city = country?.cities.find(({ code }) => constraint.city === code);

          return { country: country?.name, city: city?.name, ...coordinates };
        } else if ('country' in constraint) {
          const country = relayLocations.find(({ code }) => constraint.country === code);

          return { country: country?.name, ...coordinates };
        }
      }
    }

    return coordinates;
  }
}
