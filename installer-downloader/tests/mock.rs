#![cfg(any(target_os = "windows", target_os = "macos"))]

//! This module contains fake/mock implementations of different updater/installer traits

use installer_downloader::delegate::{AppDelegate, AppDelegateQueue, ErrorMessage};
use installer_downloader::environment::{Architecture, Environment};
use installer_downloader::temp::DirectoryProvider;
use installer_downloader::ui_downloader::UiAppDownloaderParameters;
use mullvad_update::app::{
    AppCache, AppDownloader, DownloadError, DownloadedInstaller, VerifiedInstaller,
};
use mullvad_update::fetch::ProgressUpdater;
use mullvad_update::format::{Response, SignedResponse};
use mullvad_update::version::{Version, VersionInfo, VersionParameters};
use mullvad_update::version_provider::VersionInfoProvider;
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, LazyLock, Mutex};
use std::vec::Vec;

/// Fake version info provider
#[derive(Default)]
pub struct FakeVersionInfoProvider {
    pub fail_fetching: Arc<AtomicBool>,
    pub dump_metadata_to_file: Option<PathBuf>,
}

pub static FAKE_VERSION: LazyLock<VersionInfo> = LazyLock::new(|| VersionInfo {
    stable: Version {
        version: "2025.1".parse().unwrap(),
        urls: vec!["https://mullvad.net/fakeapp".to_owned()],
        size: 1234,
        changelog: "a changelog".to_owned(),
        sha256: [0u8; 32],
    },
    beta: None,
});

pub const FAKE_ENVIRONMENT: Environment = Environment {
    architecture: Architecture::X86,
};

impl VersionInfoProvider for FakeVersionInfoProvider {
    async fn get_version_info(&self, _params: &VersionParameters) -> anyhow::Result<VersionInfo> {
        if self.fail_fetching.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("Failed to fetch version info");
        }
        Ok(FAKE_VERSION.clone())
    }

    fn set_metadata_dump_path(&mut self, path: PathBuf) {
        self.dump_metadata_to_file = Some(path);
    }
}

pub struct FakeDirectoryProvider<const SUCCEED: bool> {}

#[async_trait::async_trait]
impl<const SUCCEEDED: bool> DirectoryProvider for FakeDirectoryProvider<SUCCEEDED> {
    async fn create_download_dir() -> anyhow::Result<PathBuf> {
        if SUCCEEDED {
            Ok(Path::new("/tmp/fake").to_owned())
        } else {
            anyhow::bail!("Failed to create directory");
        }
    }
}

/// Downloader for which all steps immediately succeed
pub type FakeAppDownloaderHappyPath = FakeAppDownloader<true, true, true>;

/// Cache for which all steps immediately succeed
pub type FakeAppCacheHappyPath = FakeAppCache<true, FakeInstaller<true, true, true>>;

/// Cache for which the verification step fails
pub type FakeAppCacheVerifyFail = FakeAppCache<true, FakeInstaller<true, false, false>>;

/// A cache that returns nothing.
pub type FakeAppCacheEmpty = FakeAppCache<false, FakeInstaller<true, true, true>>;

/// Downloader for which the verification step fails
pub type FakeAppDownloaderVerifyFail = FakeAppDownloader<true, false, false>;

impl<const A: bool, const B: bool, const C: bool> From<UiAppDownloaderParameters<FakeAppDelegate>>
    for FakeAppDownloader<A, B, C>
{
    fn from(params: UiAppDownloaderParameters<FakeAppDelegate>) -> Self {
        FakeAppDownloader { params }
    }
}

/// Fake app downloader
///
/// Parameters:
/// * EXE_SUCCEED - whether fetching the binary succeeds
/// * VERIFY_SUCCEED - whether verifying the binary succeeds
/// * LAUNCH_SUCCEED - whether launching the binary succeeds
pub struct FakeAppDownloader<
    const EXE_SUCCEED: bool,
    const VERIFY_SUCCEED: bool,
    const LAUNCH_SUCCEED: bool,
> {
    params: UiAppDownloaderParameters<FakeAppDelegate>,
}

#[derive(Default, PartialEq, PartialOrd)]
pub struct FakeAppCache<const HAS_APP: bool, Installer: DownloadedInstaller + Clone + Default> {
    _phantom: PhantomData<Installer>,
}

#[derive(Default, Clone, PartialEq, PartialOrd)]
pub struct FakeInstaller<
    const EXE_SUCCEED: bool,
    const VERIFY_SUCCEED: bool,
    const LAUNCH_SUCCEED: bool,
>;

impl<const EXE_SUCCEED: bool, const VERIFY_SUCCEED: bool, const LAUNCH_SUCCEED: bool> AppDownloader
    for FakeAppDownloader<EXE_SUCCEED, VERIFY_SUCCEED, LAUNCH_SUCCEED>
{
    async fn download_executable(mut self) -> Result<impl DownloadedInstaller, DownloadError> {
        self.params.app_progress.set_url(&self.params.app_url);
        self.params.app_progress.clear_progress();
        if EXE_SUCCEED {
            self.params.app_progress.set_progress(1.);
            Ok(FakeInstaller::<EXE_SUCCEED, VERIFY_SUCCEED, LAUNCH_SUCCEED>)
        } else {
            Err(DownloadError::FetchApp(anyhow::anyhow!(
                "fetching app failed"
            )))
        }
    }
}

impl<const HAS_APP: bool, Installer> AppCache for FakeAppCache<HAS_APP, Installer>
where
    Installer: DownloadedInstaller + Clone + Default + PartialEq + PartialOrd,
{
    type Installer = Installer;

    fn new(_directory: PathBuf, _version_params: VersionParameters) -> Self {
        Self::default()
    }

    fn get_cached_installers(self, _metadata: SignedResponse) -> Vec<Self::Installer> {
        if HAS_APP {
            vec![Installer::default()]
        } else {
            vec![]
        }
    }
    #[allow(clippy::manual_async_fn)]
    fn get_metadata(
        &self,
    ) -> impl std::future::Future<Output = anyhow::Result<SignedResponse>> + Send {
        async {
            Ok(SignedResponse {
                signatures: vec![],
                signed: Response::default(),
            })
        }
    }
}

impl<const EXE_SUCCEED: bool, const VERIFY_SUCCEED: bool, const LAUNCH_SUCCEED: bool>
    DownloadedInstaller for FakeInstaller<EXE_SUCCEED, VERIFY_SUCCEED, LAUNCH_SUCCEED>
{
    async fn verify(self) -> Result<impl VerifiedInstaller, DownloadError> {
        if VERIFY_SUCCEED {
            Ok(self)
        } else {
            Err(DownloadError::Verification(anyhow::anyhow!(
                "verification failed"
            )))
        }
    }

    fn version(&self) -> &mullvad_version::Version {
        &mullvad_version::Version {
            year: 2042,
            incremental: 1337,
            pre_stable: None,
            dev: None,
        }
    }
}

impl<const EXE_SUCCEED: bool, const VERIFY_SUCCEED: bool, const LAUNCH_SUCCEED: bool>
    VerifiedInstaller for FakeInstaller<EXE_SUCCEED, VERIFY_SUCCEED, LAUNCH_SUCCEED>
{
    async fn install(self) -> Result<(), DownloadError> {
        if LAUNCH_SUCCEED {
            Ok(())
        } else {
            Err(DownloadError::InstallFailed(io::Error::other(
                "install failed",
            )))
        }
    }
}

/// A fake queue that stores callbacks so that tests can run them later.
#[derive(Clone, Default)]
pub struct FakeQueue {
    callbacks: Arc<Mutex<Vec<MainThreadCallback>>>,
}

pub type MainThreadCallback = Box<dyn FnOnce(&mut FakeAppDelegate) + Send>;

impl FakeQueue {
    /// Run all queued callbacks on the given delegate.
    pub fn run_callbacks(&self, delegate: &mut FakeAppDelegate) {
        let mut callbacks = self.callbacks.lock().unwrap();
        for cb in callbacks.drain(..) {
            cb(delegate);
        }
    }
}

impl AppDelegateQueue<FakeAppDelegate> for FakeQueue {
    fn queue_main<F: FnOnce(&mut FakeAppDelegate) + 'static + Send>(&self, callback: F) {
        self.callbacks.lock().unwrap().push(Box::new(callback));
    }
}

/// A fake [AppDelegate]
#[derive(Default)]
pub struct FakeAppDelegate {
    /// Callback registered by `on_download`
    pub download_callback: Option<Box<dyn Fn() + Send>>,
    /// Callback registered by `on_cancel`
    pub cancel_callback: Option<Box<dyn Fn() + Send>>,
    /// Callback registered by `on_beta_link`
    pub beta_callback: Option<Box<dyn Fn() + Send>>,
    /// Callback registered by `on_stable_link`
    pub stable_callback: Option<Box<dyn Fn() + Send>>,
    /// Callback registered by `on_error_cancel`
    pub error_cancel_callback: Option<Box<dyn Fn() + Send>>,
    /// Callback registered by `on_error_retry`
    pub error_retry_callback: Option<Box<dyn Fn() + Send>>,
    /// State of delegate
    pub state: DelegateState,
    /// Queue used to simulate the main thread
    pub queue: FakeQueue,
}

/// A complete state of the UI, including its call history
#[derive(Default, serde::Serialize)]
pub struct DelegateState {
    pub status_text: String,
    pub download_text: String,
    pub download_button_visible: bool,
    pub cancel_button_visible: bool,
    pub cancel_button_enabled: bool,
    pub download_button_enabled: bool,
    pub download_progress: u32,
    pub download_progress_visible: bool,
    pub beta_text_visible: bool,
    pub stable_text_visible: bool,
    pub error_message_visible: bool,
    pub error_message: ErrorMessage,
    pub quit: bool,
    /// Record of method calls.
    pub call_log: Vec<String>,
}

impl AppDelegate for FakeAppDelegate {
    type Queue = FakeQueue;

    fn on_download<F>(&mut self, callback: F)
    where
        F: Fn() + Send + 'static,
    {
        self.state.call_log.push("on_download".into());
        self.download_callback = Some(Box::new(callback));
    }

    fn on_cancel<F>(&mut self, callback: F)
    where
        F: Fn() + Send + 'static,
    {
        self.state.call_log.push("on_cancel".into());
        self.cancel_callback = Some(Box::new(callback));
    }

    fn on_beta_link<F>(&mut self, callback: F)
    where
        F: Fn() + Send + 'static,
    {
        self.state.call_log.push("on_beta_link".into());
        self.beta_callback = Some(Box::new(callback));
    }

    fn on_stable_link<F>(&mut self, callback: F)
    where
        F: Fn() + Send + 'static,
    {
        self.state.call_log.push("on_stable_link".into());
        self.stable_callback = Some(Box::new(callback));
    }

    fn set_status_text(&mut self, text: &str) {
        self.state
            .call_log
            .push(format!("set_status_text: {}", text));
        self.state.status_text = text.to_owned();
    }

    fn clear_status_text(&mut self) {
        self.state.call_log.push("clear_status_text".into());
        self.state.status_text = "".to_owned();
    }

    fn set_download_text(&mut self, text: &str) {
        self.state
            .call_log
            .push(format!("set_download_text: {}", text));
        self.state.download_text = text.to_owned();
    }

    fn clear_download_text(&mut self) {
        self.state.call_log.push("clear_download_text".into());
        self.state.download_text = "".to_owned();
    }

    fn show_download_progress(&mut self) {
        self.state.call_log.push("show_download_progress".into());
        self.state.download_progress_visible = true;
    }

    fn hide_download_progress(&mut self) {
        self.state.call_log.push("hide_download_progress".into());
        self.state.download_progress_visible = false;
    }

    fn set_download_progress(&mut self, complete: u32) {
        self.state
            .call_log
            .push(format!("set_download_progress: {}", complete));
        self.state.download_progress = complete;
    }

    fn clear_download_progress(&mut self) {
        self.state.call_log.push("clear_download_progress".into());
        self.state.download_progress = 0;
    }

    fn show_download_button(&mut self) {
        self.state.call_log.push("show_download_button".into());
        self.state.download_button_visible = true;
    }

    fn hide_download_button(&mut self) {
        self.state.call_log.push("hide_download_button".into());
        self.state.download_button_visible = false;
    }

    fn enable_download_button(&mut self) {
        self.state.call_log.push("enable_download_button".into());
        self.state.download_button_enabled = true;
    }

    fn disable_download_button(&mut self) {
        self.state.call_log.push("disable_download_button".into());
        self.state.download_button_enabled = false;
    }

    fn show_cancel_button(&mut self) {
        self.state.call_log.push("show_cancel_button".into());
        self.state.cancel_button_visible = true;
    }

    fn hide_cancel_button(&mut self) {
        self.state.call_log.push("hide_cancel_button".into());
        self.state.cancel_button_visible = false;
    }

    fn enable_cancel_button(&mut self) {
        self.state.call_log.push("enable_cancel_button".into());
        self.state.cancel_button_enabled = true;
    }

    fn disable_cancel_button(&mut self) {
        self.state.call_log.push("disable_cancel_button".into());
        self.state.cancel_button_enabled = false;
    }

    fn show_beta_text(&mut self) {
        self.state.call_log.push("show_beta_text".into());
        self.state.beta_text_visible = true;
    }

    fn hide_beta_text(&mut self) {
        self.state.call_log.push("hide_beta_text".into());
        self.state.beta_text_visible = false;
    }

    fn show_stable_text(&mut self) {
        self.state.call_log.push("show_stable_text".into());
        self.state.stable_text_visible = true;
    }

    fn hide_stable_text(&mut self) {
        self.state.call_log.push("hide_stable_text".into());
        self.state.stable_text_visible = false;
    }

    fn show_error_message(&mut self, message: ErrorMessage) {
        self.state.call_log.push(format!(
            "show_error_message: {}. retry: {}. cancel: {}",
            message.status_text, message.retry_button_text, message.cancel_button_text
        ));
        self.state.error_message = message;
        self.state.error_message_visible = true;
    }

    fn hide_error_message(&mut self) {
        self.state.call_log.push("hide_error_message".into());
        self.state.error_message_visible = false;
    }

    fn on_error_message_cancel<F>(&mut self, callback: F)
    where
        F: Fn() + Send + 'static,
    {
        self.state.call_log.push("on_error_message_cancel".into());
        self.error_cancel_callback = Some(Box::new(callback));
    }

    fn on_error_message_retry<F>(&mut self, callback: F)
    where
        F: Fn() + Send + 'static,
    {
        self.state.call_log.push("on_error_message_retry".into());
        self.error_retry_callback = Some(Box::new(callback));
    }

    fn quit(&mut self) {
        self.state.call_log.push("quit".into());
        self.state.quit = true;
    }

    fn queue(&self) -> Self::Queue {
        self.queue.clone()
    }
}
