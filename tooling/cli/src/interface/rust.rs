// Copyright 2019-2022 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{
  collections::HashMap,
  ffi::OsStr,
  fs::{File, FileType},
  io::{Read, Write},
  path::{Path, PathBuf},
  process::{Command, ExitStatus},
  str::FromStr,
  sync::{
    mpsc::{channel, sync_channel},
    Arc, Mutex,
  },
  time::{Duration, Instant},
};

use anyhow::Context;
#[cfg(target_os = "linux")]
use heck::ToKebabCase;
use log::{debug, info};
use notify::{watcher, DebouncedEvent, RecursiveMode, Watcher};
use serde::Deserialize;
use tauri_bundler::{
  AppCategory, BundleBinary, BundleSettings, DebianSettings, MacOsSettings, PackageSettings,
  UpdaterSettings, WindowsSettings,
};

use super::{AppSettings, DevProcess, ExitReason, Interface};
use crate::helpers::{
  app_paths::tauri_dir,
  config::{reload as reload_config, wix_settings, Config},
};

mod cargo_config;
mod desktop;
pub mod manifest;
use cargo_config::Config as CargoConfig;
use manifest::{rewrite_manifest, Manifest};

#[derive(Debug, Default, Clone)]
pub struct Options {
  pub runner: Option<String>,
  pub debug: bool,
  pub target: Option<String>,
  pub features: Option<Vec<String>>,
  pub args: Vec<String>,
  pub config: Option<String>,
  pub no_watch: bool,
}

impl From<crate::build::Options> for Options {
  fn from(options: crate::build::Options) -> Self {
    Self {
      runner: options.runner,
      debug: options.debug,
      target: options.target,
      features: options.features,
      args: options.args,
      config: options.config,
      no_watch: true,
    }
  }
}

impl From<crate::dev::Options> for Options {
  fn from(options: crate::dev::Options) -> Self {
    Self {
      runner: options.runner,
      debug: !options.release_mode,
      target: options.target,
      features: options.features,
      args: options.args,
      config: options.config,
      no_watch: options.no_watch,
    }
  }
}

#[derive(Debug, Clone)]
pub struct MobileOptions {
  pub debug: bool,
  pub features: Option<Vec<String>>,
  pub args: Vec<String>,
  pub config: Option<String>,
  pub no_watch: bool,
}

#[derive(Debug)]
pub struct Target {
  name: String,
  installed: bool,
}

pub struct Rust {
  app_settings: RustAppSettings,
  config_features: Vec<String>,
  product_name: Option<String>,
  available_targets: Option<Vec<Target>>,
}

impl Interface for Rust {
  type AppSettings = RustAppSettings;

  fn new(config: &Config) -> crate::Result<Self> {
    let manifest = {
      let (tx, rx) = channel();
      let mut watcher = watcher(tx, Duration::from_secs(1)).unwrap();
      watcher.watch(tauri_dir().join("Cargo.toml"), RecursiveMode::Recursive)?;
      let manifest = rewrite_manifest(config)?;
      let now = Instant::now();
      let timeout = Duration::from_secs(2);
      loop {
        if now.elapsed() >= timeout {
          break;
        }
        if let Ok(DebouncedEvent::NoticeWrite(_)) = rx.try_recv() {
          break;
        }
      }
      manifest
    };

    if let Some(minimum_system_version) = &config.tauri.bundle.macos.minimum_system_version {
      std::env::set_var("MACOSX_DEPLOYMENT_TARGET", minimum_system_version);
    }

    let app_settings = RustAppSettings::new(config, manifest)?;

    Ok(Self {
      app_settings,
      config_features: config.build.features.clone().unwrap_or_default(),
      product_name: config.package.product_name.clone(),
      available_targets: None,
    })
  }

  fn app_settings(&self) -> &Self::AppSettings {
    &self.app_settings
  }

  fn build(&mut self, mut options: Options) -> crate::Result<()> {
    options
      .features
      .get_or_insert(Vec::new())
      .push("custom-protocol".into());
    desktop::build(
      options,
      &self.app_settings,
      self.product_name.clone(),
      &mut self.available_targets,
      self.config_features.clone(),
    )?;
    Ok(())
  }

  fn dev<F: Fn(ExitStatus, ExitReason) + Send + Sync + 'static>(
    &mut self,
    mut options: Options,
    on_exit: F,
  ) -> crate::Result<()> {
    let on_exit = Arc::new(on_exit);

    let run_args = dev_options(
      &mut options.args,
      &mut options.features,
      self.app_settings.manifest.features(),
    );

    if options.no_watch {
      let (tx, rx) = sync_channel(1);
      self.run_dev(options, run_args, move |status, reason| {
        tx.send(()).unwrap();
        on_exit(status, reason)
      })?;

      rx.recv().unwrap();
      Ok(())
    } else {
      let config = options.config.clone();
      let run = Arc::new(|rust: &mut Rust| {
        let on_exit = on_exit.clone();
        rust.run_dev(options.clone(), run_args.clone(), move |status, reason| {
          on_exit(status, reason)
        })
      });
      self.run_dev_watcher(config, run)
    }
  }

  fn mobile_dev<R: Fn(MobileOptions) -> crate::Result<Box<dyn DevProcess>>>(
    &mut self,
    mut options: MobileOptions,
    runner: R,
  ) -> crate::Result<()> {
    dev_options(
      &mut options.args,
      &mut options.features,
      self.app_settings.manifest.features(),
    );

    if options.no_watch {
      runner(options)?;
      Ok(())
    } else {
      let config = options.config.clone();
      let run = Arc::new(|_rust: &mut Rust| runner(options.clone()));
      self.run_dev_watcher(config, run)
    }
  }
}

fn lookup<F: FnMut(FileType, PathBuf)>(dir: &Path, mut f: F) {
  let mut default_gitignore = std::env::temp_dir();
  default_gitignore.push(".tauri-dev");
  let _ = std::fs::create_dir_all(&default_gitignore);
  default_gitignore.push(".gitignore");
  if !default_gitignore.exists() {
    if let Ok(mut file) = std::fs::File::create(default_gitignore.clone()) {
      let _ = file.write_all(crate::dev::TAURI_DEV_WATCHER_GITIGNORE);
    }
  }

  let mut builder = ignore::WalkBuilder::new(dir);
  builder.add_custom_ignore_filename(".taurignore");
  let _ = builder.add_ignore(default_gitignore);
  if let Ok(ignore_file) = std::env::var("TAURI_DEV_WATCHER_IGNORE_FILE") {
    builder.add_ignore(ignore_file);
  }
  builder.require_git(false).ignore(false).max_depth(Some(1));

  for entry in builder.build().flatten() {
    f(entry.file_type().unwrap(), dir.join(entry.path()));
  }
}

fn dev_options(
  args: &mut Vec<String>,
  features: &mut Option<Vec<String>>,
  manifest_features: HashMap<String, Vec<String>>,
) -> Vec<String> {
  if !args.contains(&"--no-default-features".into()) {
    let enable_features: Vec<String> = manifest_features
      .get("default")
      .cloned()
      .unwrap_or_default()
      .into_iter()
      .filter(|feature| {
        if let Some(manifest_feature) = manifest_features.get(feature) {
          !manifest_feature.contains(&"tauri/custom-protocol".into())
        } else {
          feature != "tauri/custom-protocol"
        }
      })
      .collect();
    args.push("--no-default-features".into());
    if !enable_features.is_empty() {
      features.get_or_insert(Vec::new()).extend(enable_features);
    }
  }

  let mut dev_args = Vec::new();
  let mut run_args = Vec::new();
  let mut reached_run_args = false;
  for arg in args.clone() {
    if reached_run_args {
      run_args.push(arg);
    } else if arg == "--" {
      reached_run_args = true;
    } else {
      dev_args.push(arg);
    }
  }
  *args = dev_args;
  run_args
}

impl Rust {
  fn run_dev<F: Fn(ExitStatus, ExitReason) + Send + Sync + 'static>(
    &mut self,
    options: Options,
    run_args: Vec<String>,
    on_exit: F,
  ) -> crate::Result<Box<dyn DevProcess>> {
    desktop::run_dev(
      options,
      run_args,
      &mut self.available_targets,
      self.config_features.clone(),
      &self.app_settings,
      self.product_name.clone(),
      on_exit,
    )
    .map(|c| Box::new(c) as Box<dyn DevProcess>)
  }

  fn run_dev_watcher<F: Fn(&mut Rust) -> crate::Result<Box<dyn DevProcess>>>(
    &mut self,
    config: Option<String>,
    run: Arc<F>,
  ) -> crate::Result<()> {
    let child = run(self)?;

    let process = Arc::new(Mutex::new(child));
    let (tx, rx) = channel();
    let tauri_path = tauri_dir();
    let workspace_path = get_workspace_dir()?;

    let watch_folders = if tauri_path == workspace_path {
      vec![tauri_path]
    } else {
      let cargo_settings = CargoSettings::load(&workspace_path)?;
      cargo_settings
        .workspace
        .as_ref()
        .map(|w| {
          w.members
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|p| workspace_path.join(p))
            .collect()
        })
        .unwrap_or_else(|| vec![tauri_path])
    };

    let mut watcher = watcher(tx, Duration::from_secs(1)).unwrap();
    for path in watch_folders {
      info!("Watching {} for changes...", path.display());
      lookup(&path, |file_type, p| {
        if p != path {
          debug!("Watching {} for changes...", p.display());
          let _ = watcher.watch(
            p,
            if file_type.is_dir() {
              RecursiveMode::Recursive
            } else {
              RecursiveMode::NonRecursive
            },
          );
        }
      });
    }

    loop {
      let run = run.clone();
      if let Ok(event) = rx.recv() {
        let event_path = match event {
          DebouncedEvent::Create(path) => Some(path),
          DebouncedEvent::Remove(path) => Some(path),
          DebouncedEvent::Rename(_, dest) => Some(dest),
          DebouncedEvent::Write(path) => Some(path),
          _ => None,
        };

        if let Some(event_path) = event_path {
          if event_path.file_name() == Some(OsStr::new("tauri.conf.json")) {
            let config = reload_config(config.as_deref())?;
            self.app_settings.manifest =
              rewrite_manifest(config.lock().unwrap().as_ref().unwrap())?;
          } else {
            // When tauri.conf.json is changed, rewrite_manifest will be called
            // which will trigger the watcher again
            // So the app should only be started when a file other than tauri.conf.json is changed
            let mut p = process.lock().unwrap();
            p.kill().with_context(|| "failed to kill app process")?;
            // wait for the process to exit
            loop {
              if let Ok(Some(_)) = p.try_wait() {
                break;
              }
            }
            *p = run(self)?;
          }
        }
      }
    }
  }
}

/// The `workspace` section of the app configuration (read from Cargo.toml).
#[derive(Clone, Debug, Deserialize)]
struct WorkspaceSettings {
  /// the workspace members.
  members: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize)]
struct BinarySettings {
  name: String,
  path: Option<String>,
}

/// The package settings.
#[derive(Debug, Clone, Deserialize)]
pub struct CargoPackageSettings {
  /// the package's name.
  pub name: Option<String>,
  /// the package's version.
  pub version: Option<String>,
  /// the package's description.
  pub description: Option<String>,
  /// the package's homepage.
  pub homepage: Option<String>,
  /// the package's authors.
  pub authors: Option<Vec<String>>,
  /// the default binary to run.
  pub default_run: Option<String>,
}

/// The Cargo settings (Cargo.toml root descriptor).
#[derive(Clone, Debug, Deserialize)]
struct CargoSettings {
  /// the package settings.
  ///
  /// it's optional because ancestor workspace Cargo.toml files may not have package info.
  package: Option<CargoPackageSettings>,
  /// the workspace settings.
  ///
  /// it's present if the read Cargo.toml belongs to a workspace root.
  workspace: Option<WorkspaceSettings>,
  /// the binary targets configuration.
  bin: Option<Vec<BinarySettings>>,
}

impl CargoSettings {
  /// Try to load a set of CargoSettings from a "Cargo.toml" file in the specified directory.
  fn load(dir: &Path) -> crate::Result<Self> {
    let toml_path = dir.join("Cargo.toml");
    let mut toml_str = String::new();
    let mut toml_file = File::open(toml_path).with_context(|| "failed to open Cargo.toml")?;
    toml_file
      .read_to_string(&mut toml_str)
      .with_context(|| "failed to read Cargo.toml")?;
    toml::from_str(&toml_str)
      .with_context(|| "failed to parse Cargo.toml")
      .map_err(Into::into)
  }
}

pub struct RustAppSettings {
  manifest: Manifest,
  cargo_settings: CargoSettings,
  cargo_package_settings: CargoPackageSettings,
  package_settings: PackageSettings,
  cargo_config: CargoConfig,
}

impl AppSettings for RustAppSettings {
  fn get_package_settings(&self) -> PackageSettings {
    self.package_settings.clone()
  }

  fn get_bundle_settings(
    &self,
    config: &Config,
    features: &[String],
  ) -> crate::Result<BundleSettings> {
    tauri_config_to_bundle_settings(
      &self.manifest,
      features,
      config.tauri.bundle.clone(),
      config.tauri.system_tray.clone(),
      config.tauri.updater.clone(),
    )
  }

  fn app_binary_path(&self, options: &Options) -> crate::Result<PathBuf> {
    let bin_name = self
      .cargo_package_settings()
      .name
      .clone()
      .expect("Cargo manifest must have the `package.name` field");

    let out_dir = self
      .out_dir(options.target.clone(), options.debug)
      .with_context(|| "failed to get project out directory")?;
    let target: String = if let Some(target) = options.target.clone() {
      target
    } else {
      tauri_utils::platform::target_triple()?
    };

    let binary_extension: String = if target.contains("windows") {
      "exe"
    } else {
      ""
    }
    .into();

    Ok(out_dir.join(bin_name).with_extension(&binary_extension))
  }

  fn get_binaries(&self, config: &Config, target: &str) -> crate::Result<Vec<BundleBinary>> {
    let mut binaries: Vec<BundleBinary> = vec![];

    let binary_extension: String = if target.contains("windows") {
      ".exe"
    } else {
      ""
    }
    .into();

    if let Some(bin) = &self.cargo_settings.bin {
      let default_run = self
        .package_settings
        .default_run
        .clone()
        .unwrap_or_else(|| "".to_string());
      for binary in bin {
        binaries.push(
          if Some(&binary.name) == self.cargo_package_settings.name.as_ref()
            || binary.name.as_str() == default_run
          {
            BundleBinary::new(
              format!(
                "{}{}",
                config
                  .package
                  .binary_name()
                  .unwrap_or_else(|| binary.name.clone()),
                &binary_extension
              ),
              true,
            )
          } else {
            BundleBinary::new(
              format!("{}{}", binary.name.clone(), &binary_extension),
              false,
            )
          }
          .set_src_path(binary.path.clone()),
        )
      }
    }

    let mut bins_path = tauri_dir();
    bins_path.push("src/bin");
    if let Ok(fs_bins) = std::fs::read_dir(bins_path) {
      for entry in fs_bins {
        let path = entry?.path();
        if let Some(name) = path.file_stem() {
          let bin_exists = binaries.iter().any(|bin| {
            bin.name() == name || path.ends_with(bin.src_path().unwrap_or(&"".to_string()))
          });
          if !bin_exists {
            binaries.push(BundleBinary::new(
              format!("{}{}", name.to_string_lossy(), &binary_extension),
              false,
            ))
          }
        }
      }
    }

    if let Some(default_run) = self.package_settings.default_run.as_ref() {
      match binaries.iter_mut().find(|bin| bin.name() == default_run) {
        Some(bin) => {
          if let Some(bin_name) = config.package.binary_name() {
            bin.set_name(bin_name);
          }
        }
        None => {
          binaries.push(BundleBinary::new(
            format!(
              "{}{}",
              config
                .package
                .binary_name()
                .unwrap_or_else(|| default_run.to_string()),
              &binary_extension
            ),
            true,
          ));
        }
      }
    }

    match binaries.len() {
      0 => binaries.push(BundleBinary::new(
        #[cfg(target_os = "linux")]
        self.package_settings.product_name.to_kebab_case(),
        #[cfg(not(target_os = "linux"))]
        format!(
          "{}{}",
          self.package_settings.product_name.clone(),
          &binary_extension
        ),
        true,
      )),
      1 => binaries.get_mut(0).unwrap().set_main(true),
      _ => {}
    }

    Ok(binaries)
  }
}

impl RustAppSettings {
  pub fn new(config: &Config, manifest: Manifest) -> crate::Result<Self> {
    let cargo_settings =
      CargoSettings::load(&tauri_dir()).with_context(|| "failed to load cargo settings")?;
    let cargo_package_settings = match &cargo_settings.package {
      Some(package_info) => package_info.clone(),
      None => {
        return Err(anyhow::anyhow!(
          "No package info in the config file".to_owned(),
        ))
      }
    };

    let package_settings = PackageSettings {
      product_name: config.package.product_name.clone().unwrap_or_else(|| {
        cargo_package_settings
          .name
          .clone()
          .expect("Cargo manifest must have the `package.name` field")
      }),
      version: config.package.version.clone().unwrap_or_else(|| {
        cargo_package_settings
          .version
          .clone()
          .expect("Cargo manifest must have the `package.version` field")
      }),
      description: cargo_package_settings
        .description
        .clone()
        .unwrap_or_default(),
      homepage: cargo_package_settings.homepage.clone(),
      authors: cargo_package_settings.authors.clone(),
      default_run: cargo_package_settings.default_run.clone(),
    };

    let cargo_config = CargoConfig::load(&tauri_dir())?;

    Ok(Self {
      manifest,
      cargo_settings,
      cargo_package_settings,
      package_settings,
      cargo_config,
    })
  }

  pub fn cargo_package_settings(&self) -> &CargoPackageSettings {
    &self.cargo_package_settings
  }

  pub fn out_dir(&self, target: Option<String>, debug: bool) -> crate::Result<PathBuf> {
    get_target_dir(
      target
        .as_deref()
        .or_else(|| self.cargo_config.build().target()),
      !debug,
    )
  }
}

#[derive(Deserialize)]
struct CargoMetadata {
  target_directory: PathBuf,
  workspace_root: PathBuf,
}

fn get_cargo_metadata() -> crate::Result<CargoMetadata> {
  let output = Command::new("cargo")
    .args(["metadata", "--no-deps", "--format-version", "1"])
    .current_dir(tauri_dir())
    .output()?;

  if !output.status.success() {
    return Err(anyhow::anyhow!(
      "cargo metadata command exited with a non zero exit code: {}",
      String::from_utf8(output.stderr)?
    ));
  }

  Ok(serde_json::from_slice(&output.stdout)?)
}

/// This function determines the 'target' directory and suffixes it with 'release' or 'debug'
/// to determine where the compiled binary will be located.
fn get_target_dir(target: Option<&str>, is_release: bool) -> crate::Result<PathBuf> {
  let mut path = get_cargo_metadata()
    .with_context(|| "failed to get cargo metadata")?
    .target_directory;

  if let Some(triple) = target {
    path.push(triple);
  }

  path.push(if is_release { "release" } else { "debug" });

  Ok(path)
}

/// Executes `cargo metadata` to get the workspace directory.
pub fn get_workspace_dir() -> crate::Result<PathBuf> {
  Ok(
    get_cargo_metadata()
      .with_context(|| "failed to get cargo metadata")?
      .workspace_root,
  )
}

#[allow(unused_variables)]
fn tauri_config_to_bundle_settings(
  manifest: &Manifest,
  features: &[String],
  config: crate::helpers::config::BundleConfig,
  system_tray_config: Option<crate::helpers::config::SystemTrayConfig>,
  updater_config: crate::helpers::config::UpdaterConfig,
) -> crate::Result<BundleSettings> {
  let enabled_features = manifest.all_enabled_features(features);

  #[cfg(windows)]
  let windows_icon_path = PathBuf::from(
    config
      .icon
      .iter()
      .find(|i| i.ends_with(".ico"))
      .cloned()
      .expect("the bundle config must have a `.ico` icon"),
  );
  #[cfg(not(windows))]
  let windows_icon_path = PathBuf::from("");

  #[allow(unused_mut)]
  let mut resources = config.resources.unwrap_or_default();
  #[allow(unused_mut)]
  let mut depends = config.deb.depends.unwrap_or_default();

  #[cfg(target_os = "linux")]
  {
    if let Some(system_tray_config) = &system_tray_config {
      let tray = std::env::var("TAURI_TRAY").unwrap_or_else(|_| "ayatana".to_string());
      if tray == "ayatana" {
        depends.push("libayatana-appindicator3-1".into());
      } else {
        depends.push("libappindicator3-1".into());
      }
    }

    // provides `libwebkit2gtk-4.0.so.37` and all `4.0` versions have the -37 package name
    depends.push("libwebkit2gtk-4.0-37".to_string());
    depends.push("libgtk-3-0".to_string());
  }

  #[cfg(windows)]
  {
    if let Some(webview_fixed_runtime_path) = &config.windows.webview_fixed_runtime_path {
      resources.push(webview_fixed_runtime_path.display().to_string());
    } else if let crate::helpers::config::WebviewInstallMode::FixedRuntime { path } =
      &config.windows.webview_install_mode
    {
      resources.push(path.display().to_string());
    }
  }

  let signing_identity = match std::env::var_os("APPLE_SIGNING_IDENTITY") {
    Some(signing_identity) => Some(
      signing_identity
        .to_str()
        .expect("failed to convert APPLE_SIGNING_IDENTITY to string")
        .to_string(),
    ),
    None => config.macos.signing_identity,
  };

  let provider_short_name = match std::env::var_os("APPLE_PROVIDER_SHORT_NAME") {
    Some(provider_short_name) => Some(
      provider_short_name
        .to_str()
        .expect("failed to convert APPLE_PROVIDER_SHORT_NAME to string")
        .to_string(),
    ),
    None => config.macos.provider_short_name,
  };

  Ok(BundleSettings {
    identifier: Some(config.identifier),
    icon: Some(config.icon),
    resources: if resources.is_empty() {
      None
    } else {
      Some(resources)
    },
    copyright: config.copyright,
    category: match config.category {
      Some(category) => Some(AppCategory::from_str(&category).map_err(|e| match e {
        Some(e) => anyhow::anyhow!("invalid category, did you mean `{}`?", e),
        None => anyhow::anyhow!("invalid category"),
      })?),
      None => None,
    },
    short_description: config.short_description,
    long_description: config.long_description,
    external_bin: config.external_bin,
    deb: DebianSettings {
      depends: if depends.is_empty() {
        None
      } else {
        Some(depends)
      },
      files: config.deb.files,
    },
    macos: MacOsSettings {
      frameworks: config.macos.frameworks,
      minimum_system_version: config.macos.minimum_system_version,
      license: config.macos.license,
      exception_domain: config.macos.exception_domain,
      signing_identity,
      provider_short_name,
      entitlements: config.macos.entitlements,
      info_plist_path: {
        let path = tauri_dir().join("Info.plist");
        if path.exists() {
          Some(path)
        } else {
          None
        }
      },
    },
    windows: WindowsSettings {
      timestamp_url: config.windows.timestamp_url,
      tsp: config.windows.tsp,
      digest_algorithm: config.windows.digest_algorithm,
      certificate_thumbprint: config.windows.certificate_thumbprint,
      wix: config.windows.wix.map(|w| {
        let mut wix = wix_settings(w);
        wix.license = wix.license.map(|l| tauri_dir().join(l));
        wix
      }),
      icon_path: windows_icon_path,
      webview_install_mode: config.windows.webview_install_mode,
      webview_fixed_runtime_path: config.windows.webview_fixed_runtime_path,
      allow_downgrades: config.windows.allow_downgrades,
    },
    updater: Some(UpdaterSettings {
      active: updater_config.active,
      // we set it to true by default we shouldn't have to use
      // unwrap_or as we have a default value but used to prevent any failing
      dialog: updater_config.dialog,
      pubkey: updater_config.pubkey,
      endpoints: updater_config
        .endpoints
        .map(|endpoints| endpoints.iter().map(|e| e.to_string()).collect()),
      msiexec_args: Some(updater_config.windows.install_mode.msiexec_args()),
    }),
    ..Default::default()
  })
}
