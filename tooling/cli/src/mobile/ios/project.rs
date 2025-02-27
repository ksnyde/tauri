// Copyright 2019-2021 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use crate::{helpers::template, Result};
use anyhow::Context;
use cargo_mobile::{
  apple::{
    config::{Config, Metadata},
    deps, rust_version_check,
    target::Target,
  },
  bossy,
  config::app::DEFAULT_ASSET_DIR,
  target::TargetTrait as _,
  util::{self, cli::TextWrapper},
};
use handlebars::Handlebars;
use include_dir::{include_dir, Dir};
use std::{
  ffi::{OsStr, OsString},
  fs::{create_dir_all, OpenOptions},
  path::{Component, PathBuf},
};

const TEMPLATE_DIR: Dir<'_> = include_dir!("templates/mobile/ios");

// unprefixed app_root seems pretty dangerous!!
// TODO: figure out what cargo-mobile meant by that
pub fn gen(
  config: &Config,
  metadata: &Metadata,
  (handlebars, mut map): (Handlebars, template::JsonMap),
  wrapper: &TextWrapper,
  non_interactive: bool,
  reinstall_deps: bool,
) -> Result<()> {
  println!("Installing iOS toolchains...");
  Target::install_all()?;
  rust_version_check(wrapper)?;

  deps::install_all(wrapper, non_interactive, true, reinstall_deps)
    .with_context(|| "failed to install Apple dependencies")?;

  let dest = config.project_dir();
  let rel_prefix = util::relativize_path(config.app().root_dir(), &dest);
  let source_dirs = vec![rel_prefix.join("src")];

  let asset_catalogs = metadata.ios().asset_catalogs().unwrap_or_default();
  let ios_pods = metadata.ios().pods().unwrap_or_default();
  let macos_pods = metadata.macos().pods().unwrap_or_default();

  #[cfg(target_arch = "aarch64")]
  let default_archs = ["arm64", "arm64-sim"];
  #[cfg(not(target_arch = "aarch64"))]
  let default_archs = ["arm64", "x86_64"];

  map.insert("file-groups", &source_dirs);
  map.insert("ios-frameworks", metadata.ios().frameworks());
  map.insert("ios-valid-archs", default_archs);
  map.insert("ios-vendor-frameworks", metadata.ios().vendor_frameworks());
  map.insert("ios-vendor-sdks", metadata.ios().vendor_sdks());
  map.insert("macos-frameworks", metadata.macos().frameworks());
  map.insert(
    "macos-vendor-frameworks",
    metadata.macos().vendor_frameworks(),
  );
  map.insert("macos-vendor-sdks", metadata.macos().vendor_frameworks());
  map.insert("asset-catalogs", asset_catalogs);
  map.insert("ios-pods", ios_pods);
  map.insert("macos-pods", macos_pods);
  map.insert(
    "ios-additional-targets",
    metadata.ios().additional_targets(),
  );
  map.insert(
    "macos-additional-targets",
    metadata.macos().additional_targets(),
  );
  map.insert("ios-pre-build-scripts", metadata.ios().pre_build_scripts());
  map.insert(
    "ios-post-compile-scripts",
    metadata.ios().post_compile_scripts(),
  );
  map.insert(
    "ios-post-build-scripts",
    metadata.ios().post_build_scripts(),
  );
  map.insert(
    "macos-pre-build-scripts",
    metadata.macos().pre_build_scripts(),
  );
  map.insert(
    "macos-post-compile-scripts",
    metadata.macos().post_compile_scripts(),
  );
  map.insert(
    "macos-post-build-scripts",
    metadata.macos().post_build_scripts(),
  );
  map.insert(
    "ios-command-line-arguments",
    metadata.ios().command_line_arguments(),
  );
  map.insert(
    "macos-command-line-arguments",
    metadata.macos().command_line_arguments(),
  );

  let mut created_dirs = Vec::new();
  template::render_with_generator(
    &handlebars,
    map.inner(),
    &TEMPLATE_DIR,
    &dest,
    &mut |path| {
      let mut components: Vec<_> = path.components().collect();
      let mut new_component = None;
      for component in &mut components {
        if let Component::Normal(c) = component {
          let c = c.to_string_lossy();
          if c.contains("{{app.name}}") {
            new_component.replace(OsString::from(
              &c.replace("{{app.name}}", config.app().name()),
            ));
            *component = Component::Normal(new_component.as_ref().unwrap());
            break;
          }
        }
      }
      let path = dest.join(components.iter().collect::<PathBuf>());

      let parent = path.parent().unwrap().to_path_buf();
      if !created_dirs.contains(&parent) {
        create_dir_all(&parent)?;
        created_dirs.push(parent);
      }

      let mut options = OpenOptions::new();
      options.write(true);

      if path.file_name().unwrap() == OsStr::new("BuildTask.kt") || !path.exists() {
        options.create(true).open(path).map(Some)
      } else {
        Ok(None)
      }
    },
  )
  .with_context(|| "failed to process template")?;

  let asset_dir = dest.join(DEFAULT_ASSET_DIR);
  if !asset_dir.is_dir() {
    create_dir_all(&asset_dir).map_err(|cause| {
      anyhow::anyhow!(
        "failed to create asset dir {path}: {cause}",
        path = asset_dir.display()
      )
    })?;
  }

  // Create all asset catalog directories if they don't already exist
  for dir in asset_catalogs {
    std::fs::create_dir_all(dir).map_err(|cause| {
      anyhow::anyhow!(
        "failed to create directory at {path}: {cause}",
        path = dir.display()
      )
    })?;
  }

  // Note that Xcode doesn't always reload the project nicely; reopening is
  // often necessary.
  println!("Generating Xcode project...");
  bossy::Command::impure("xcodegen")
    .with_args(&["generate", "--spec"])
    .with_arg(dest.join("project.yml"))
    .run_and_wait()
    .with_context(|| "failed to run `xcodegen`")?;

  if !ios_pods.is_empty() || !macos_pods.is_empty() {
    bossy::Command::impure_parse("pod install")
      .with_arg(format!("--project-directory={}", dest.display()))
      .run_and_wait()
      .with_context(|| "failed to run `pod install`")?;
  }
  Ok(())
}
