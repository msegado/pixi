use crate::prefix::Prefix;
use crate::progress;
use crate::progress::ProgressBarMessageFormatter;
use futures::{stream, Stream, StreamExt, TryFutureExt, TryStreamExt};
use indexmap::IndexSet;
use indicatif::ProgressBar;
use itertools::Itertools;
use miette::{IntoDiagnostic, WrapErr};
use rattler::install::Transaction;
use rattler_conda_types::{Platform, PrefixRecord, RepoDataRecord};
use rattler_lock::{CondaLock, LockedDependency};
use rip::artifacts::wheel::{InstallPaths, UnpackWheelOptions};
use rip::artifacts::Wheel;
use rip::index::PackageDb;
use rip::python_env::{find_distributions_in_venv, uninstall_distribution, Distribution, WheelTag};
use rip::types::{
    Artifact, ArtifactHashes, ArtifactInfo, ArtifactName, Extra, NormalizedPackageName,
    WheelFilename,
};
use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;
use tokio::task::JoinError;

/// The installer name for pypi packages installed by pixi.
pub(crate) const PIXI_PYPI_INSTALLER: &str = env!("CARGO_PKG_NAME");

/// Installs and/or remove python distributions.
pub async fn update_python_distributions(
    package_db: &PackageDb,
    prefix: &Prefix,
    lock_file: &CondaLock,
    platform: Platform,
    transaction: &Transaction<PrefixRecord, RepoDataRecord>,
) -> miette::Result<()> {
    // Get the python info from the transaction
    let Some(python_info) = transaction.python_info.as_ref() else {
        return Ok(());
    };

    // Determine where packages would have been installed
    let install_paths = InstallPaths::for_venv(
        (
            python_info.short_version.0 as u32,
            python_info.short_version.1 as u32,
            0,
        ),
        platform.is_windows(),
    );

    // Determine the current python distributions in those locations
    let current_python_packages = find_distributions_in_venv(prefix.root(), &install_paths)
        .into_diagnostic()
        .context(
            "failed to locate python packages that have not been installed as conda packages",
        )?;

    // Determine the python packages that are part of the lock-file
    let python_packages = lock_file
        .get_packages_by_platform(platform)
        .filter(|p| p.is_pypi())
        .collect_vec();

    // Determine the python packages to remove before we start installing anything new. If the
    // python version changed between installations we will have to remove any previous distribution
    // regardless.
    let (python_distributions_to_remove, python_distributions_to_install) =
        determine_python_distributions_to_remove_and_install(
            prefix.root(),
            current_python_packages,
            python_packages,
        );

    // Start downloading the python packages that we want in the background.
    let (package_stream, package_stream_pb) =
        stream_python_artifacts(package_db, python_distributions_to_install.clone());

    // Remove python packages that need to be removed
    if !python_distributions_to_remove.is_empty() {
        let site_package_path = install_paths.site_packages();

        for python_distribution in python_distributions_to_remove {
            uninstall_pixi_installed_distribution(prefix, site_package_path, &python_distribution)?;
        }
    }

    // Install the individual python packages that we want
    let package_install_pb = install_python_distributions(
        prefix,
        install_paths,
        &prefix.root().join(&python_info.path),
        package_stream,
    )
    .await?;

    // Clear any pending progress bar
    for pb in package_install_pb
        .into_iter()
        .chain(package_stream_pb.into_iter())
    {
        pb.finish_and_clear();
    }

    Ok(())
}

/// Concurrently installs python wheels as they become available.
async fn install_python_distributions(
    prefix: &Prefix,
    install_paths: InstallPaths,
    python_executable_path: &Path,
    package_stream: impl Stream<Item = miette::Result<(Option<String>, HashSet<Extra>, Wheel)>> + Sized,
) -> miette::Result<Option<ProgressBar>> {
    // Determine the number of packages that we are going to install
    let len = {
        let (lower_bound, upper_bound) = package_stream.size_hint();
        upper_bound.unwrap_or(lower_bound)
    };
    if len == 0 {
        return Ok(None);
    }

    // Create a progress bar to show the progress of the installation
    let pb = progress::global_multi_progress().add(ProgressBar::new(len as u64));
    pb.set_style(progress::default_progress_style());
    pb.set_prefix("unpacking wheels");
    pb.enable_steady_tick(Duration::from_millis(100));

    // Create a message formatter to show the current operation
    let message_formatter = ProgressBarMessageFormatter::new(pb.clone());

    // Concurrently unpack the wheels as they become available in the stream.
    let install_pb = pb.clone();
    package_stream
        .try_for_each_concurrent(Some(20), move |(hash, extras, wheel)| {
            let install_paths = install_paths.clone();
            let root = prefix.root().to_path_buf();
            let message_formatter = message_formatter.clone();
            let pb = install_pb.clone();
            let python_executable_path = python_executable_path.to_owned();
            async move {
                let pb_task = message_formatter.start(wheel.name().to_string()).await;
                let unpack_result = tokio::task::spawn_blocking(move || {
                    wheel
                        .unpack(
                            &root,
                            &install_paths,
                            &python_executable_path,
                            &UnpackWheelOptions {
                                installer: Some(PIXI_PYPI_INSTALLER.into()),
                                extras: Some(extras),
                                ..Default::default()
                            },
                        )
                        .into_diagnostic()
                        .and_then(|unpacked_wheel| {
                            if let Some(hash) = hash {
                                std::fs::write(unpacked_wheel.dist_info.join("HASH"), hash)
                                    .into_diagnostic()
                            } else {
                                Ok(())
                            }
                        })
                })
                .map_err(JoinError::try_into_panic)
                .await;

                pb_task.finish().await;
                pb.inc(1);

                match unpack_result {
                    Ok(unpack_result) => unpack_result,
                    Err(Ok(panic)) => std::panic::resume_unwind(panic),
                    Err(Err(e)) => Err(miette::miette!("{e}")),
                }
            }
        })
        .await?;

    // Update the progress bar
    pb.set_style(progress::finished_progress_style());
    pb.finish();

    Ok(Some(pb))
}

/// Creates a stream which downloads the specified python packages. The stream will download the
/// packages in parallel and yield them as soon as they become available.
fn stream_python_artifacts<'a>(
    package_db: &'a PackageDb,
    packages_to_download: Vec<&'a LockedDependency>,
) -> (
    impl Stream<Item = miette::Result<(Option<String>, HashSet<Extra>, Wheel)>> + 'a,
    Option<ProgressBar>,
) {
    if packages_to_download.is_empty() {
        return (stream::empty().left_stream(), None);
    }

    // Construct a progress bar to provide some indication on what is currently downloading.
    // TODO: It would be much nicer if we can provide more information with regards to the progress.
    //  For instance if we could also show at what speed the downloads are progressing or the total
    //  size of the downloads that would really help the user I think.
    let pb =
        progress::global_multi_progress().add(ProgressBar::new(packages_to_download.len() as u64));
    pb.set_style(progress::default_progress_style());
    pb.set_prefix("acquiring wheels");
    pb.enable_steady_tick(Duration::from_millis(100));

    // Construct a message formatter
    let message_formatter = ProgressBarMessageFormatter::new(pb.clone());

    let stream_pb = pb.clone();
    let total_packages = packages_to_download.len();
    let download_stream = stream::iter(packages_to_download)
        .map(move |package| {
            let pb = stream_pb.clone();
            let message_formatter = message_formatter.clone();
            async move {
                let pip_package = package
                    .as_pypi()
                    .expect("must be a pip package at this point");

                // Determine the filename from the
                let filename = pip_package
                    .url
                    .path_segments()
                    .and_then(|s| s.last())
                    .expect("url is missing a path");
                let name = NormalizedPackageName::from_str(&package.name)
                    .into_diagnostic()
                    .with_context(|| {
                        format!("'{}' is not a valid python package name", &package.name)
                    })?;
                let wheel_name = WheelFilename::from_filename(filename, &name)
                    .expect("failed to convert filename to wheel filename");

                // Log out intent to install this python package.
                tracing::info!("downloading python package {filename}");
                let pb_task = message_formatter.start(filename.to_string()).await;

                // Reconstruct the ArtifactInfo from the data in the lockfile.
                let artifact_info = ArtifactInfo {
                    filename: ArtifactName::Wheel(wheel_name),
                    url: pip_package.url.clone(),
                    hashes: pip_package.hash.as_ref().map(|hash| ArtifactHashes {
                        sha256: hash.sha256().cloned(),
                    }),
                    requires_python: pip_package
                        .requires_python
                        .as_ref()
                        .map(|p| p.parse())
                        .transpose()
                        .expect("the lock file contains an invalid 'requires_python` field"),
                    dist_info_metadata: Default::default(),
                    yanked: Default::default(),
                };

                // TODO: Maybe we should have a cache of wheels separate from the package_db. Since a
                //   wheel can just be identified by its hash or url.
                let wheel: Wheel = package_db.get_wheel(&artifact_info, None).await?;

                // Update the progress bar
                pb_task.finish().await;
                pb.inc(1);
                if pb.position() == total_packages as u64 {
                    pb.set_style(progress::finished_progress_style());
                    pb.finish();
                }

                let hash = pip_package
                    .hash
                    .as_ref()
                    .and_then(|h| h.sha256())
                    .map(|sha256| format!("sha256-{:x}", sha256));

                Ok((
                    hash,
                    pip_package
                        .extras
                        .iter()
                        .filter_map(|e| Extra::from_str(e).ok())
                        .collect(),
                    wheel,
                ))
            }
        })
        .buffer_unordered(20)
        .right_stream();

    (download_stream, Some(pb))
}

/// If there was a previous version of python installed, remove any distribution installed in that
/// environment.
pub fn remove_old_python_distributions(
    prefix: &Prefix,
    platform: Platform,
    transaction: &Transaction<PrefixRecord, RepoDataRecord>,
) -> miette::Result<()> {
    // Determine if the current distribution is the same as the desired distribution.
    let Some(previous_python_installation) = transaction.current_python_info.as_ref() else {
        return Ok(());
    };
    if Some(previous_python_installation.short_version)
        == transaction.python_info.as_ref().map(|p| p.short_version)
    {
        return Ok(());
    }

    // Determine the current python distributions in its install locations
    let python_version = (
        previous_python_installation.short_version.0 as u32,
        previous_python_installation.short_version.1 as u32,
        0,
    );
    let install_paths = InstallPaths::for_venv(python_version, platform.is_windows());

    // Locate the packages that are installed in the previous environment
    let current_python_packages = find_distributions_in_venv(prefix.root(), &install_paths)
        .into_diagnostic()
        .with_context(|| format!("failed to determine the python packages installed for a previous version of python ({}.{})", python_version.0, python_version.1))?
        .into_iter().filter(|d| d.installer.as_deref() != Some("conda") && d.installer.is_some()).collect_vec();

    let pb = progress::global_multi_progress()
        .add(ProgressBar::new(current_python_packages.len() as u64));
    pb.set_style(progress::default_progress_style());
    pb.set_message("removing old python packages");
    pb.enable_steady_tick(Duration::from_millis(100));

    // Remove the python packages
    let site_package_path = install_paths.site_packages();
    for python_package in current_python_packages {
        pb.set_message(format!(
            "{} {}",
            &python_package.name, &python_package.version
        ));

        uninstall_pixi_installed_distribution(prefix, site_package_path, &python_package)?;

        pb.inc(1);
    }

    Ok(())
}

/// Uninstalls a python distribution that was previously installed by pixi.
fn uninstall_pixi_installed_distribution(
    prefix: &Prefix,
    site_package_path: &Path,
    python_package: &Distribution,
) -> miette::Result<()> {
    tracing::info!(
        "uninstalling python package {}-{}",
        &python_package.name,
        &python_package.version
    );
    let relative_dist_info = python_package
        .dist_info
        .strip_prefix(site_package_path)
        .expect("the dist-info path must be a sub-path of the site-packages path");

    // HACK: Also remove the HASH file that pixi writes. Ignore the error if its there. We
    // should probably actually add this file to the RECORD.
    let _ = std::fs::remove_file(prefix.root().join(&python_package.dist_info).join("HASH"));

    uninstall_distribution(&prefix.root().join(site_package_path), relative_dist_info)
        .into_diagnostic()
        .with_context(|| format!("could not uninstall python package {}-{}. Manually remove the `.pixi/env` folder and try again.", &python_package.name, &python_package.version))?;

    Ok(())
}

/// Determine which python packages we can leave untouched and which python packages should be
/// removed.
fn determine_python_distributions_to_remove_and_install<'p>(
    prefix: &Path,
    mut current_python_packages: Vec<Distribution>,
    desired_python_packages: Vec<&'p LockedDependency>,
) -> (Vec<Distribution>, Vec<&'p LockedDependency>) {
    // Determine the artifact tags associated with the locked dependencies.
    let mut desired_python_packages = extract_locked_tags(desired_python_packages);

    // Any package that is currently installed that is not part of the locked dependencies should be
    // removed. So we keep it in the `current_python_packages` list.
    // Any package that is in the currently installed list that is NOT found in the lockfile is
    // retained in the list to mark it for removal.
    current_python_packages.retain(|current_python_packages| {
        if current_python_packages.installer.is_none() {
            // If this package has no installer, we can't make a reliable decision on whether to
            // keep it or not. So we do not uninstall it.
            return false;
        }

        if let Some(found_desired_packages_idx) =
            desired_python_packages
                .iter()
                .position(|(pkg, artifact_name)| {
                    does_installed_match_locked_package(
                        prefix,
                        current_python_packages,
                        (pkg, artifact_name.as_ref()),
                    )
                })
        {
            // Remove from the desired list of packages to install & from the packages to uninstall.
            desired_python_packages.remove(found_desired_packages_idx);
            false
        } else {
            // Only if this package was previously installed by us do we remove it.
            current_python_packages.installer.as_deref() == Some(PIXI_PYPI_INSTALLER)
        }
    });

    (
        current_python_packages,
        desired_python_packages
            .into_iter()
            .map(|(pkg, _)| pkg)
            .collect(),
    )
}

/// Determine the wheel tags for the locked dependencies. These are extracted by looking at the url
/// of the locked dependency. The filename of the URL is converted to a wheel name and the tags are
/// extract from that.
///
/// If the locked dependency is not a wheel distribution `None` is returned for the tags. If the
/// the wheel name could not be parsed `None` is returned for the tags and a warning is emitted.
fn extract_locked_tags(
    desired_python_packages: Vec<&LockedDependency>,
) -> Vec<(&LockedDependency, Option<IndexSet<WheelTag>>)> {
    desired_python_packages
        .into_iter()
        .map(|pkg| {
            // Get the package as a pip package. If the package is not a pip package we can just ignore it.
            let Some(pip) = pkg.as_pypi() else { return (pkg, None); };

            // Extract the filename from the url and the name from the package name.
            let Some(filename) = pip.url.path_segments().and_then(|s| s.last()) else {
                tracing::warn!(
                        "failed to determine the artifact name of the python package {}-{} from url {}: the url has no filename.",
                        &pkg.name, pkg.version, &pip.url);
                return (pkg, None);
            };
            let Ok(name) = NormalizedPackageName::from_str(&pkg.name) else {
                tracing::warn!(
                        "failed to determine the artifact name of the python package {}-{} from url {}: {} is not a valid package name.",
                        &pkg.name, pkg.version, &pip.url, &pkg.name);
                return (pkg, None);
            };

            // Determine the artifact type from the name and filename
            match ArtifactName::from_filename(filename, &name) {
                Ok(ArtifactName::Wheel(name)) => (pkg, Some(IndexSet::from_iter(name.all_tags_iter()))),
                Ok(_) => (pkg, None),
                Err(err) => {
                    tracing::warn!(
                        "failed to determine the artifact name of the python package {}-{}. Could not determine the name from the url {}: {err}",
                        &pkg.name, pkg.version, &pip.url);
                    (pkg, None)
                }
            }
        })
        .collect()
}

/// Returns true if the installed python package matches the locked python package. If that is the
/// case we can assume that the locked python package is already installed.
fn does_installed_match_locked_package(
    prefix_root: &Path,
    installed_python_package: &Distribution,
    locked_python_package: (&LockedDependency, Option<&IndexSet<WheelTag>>),
) -> bool {
    let (pkg, artifact_tags) = locked_python_package;

    // Match on name and version
    if pkg.name != installed_python_package.name.as_str()
        || pep440_rs::Version::from_str(&pkg.version).ok().as_ref()
            != Some(&installed_python_package.version)
    {
        return false;
    }

    // If this distribution is installed with pixi we can assume that there is a URL file that
    // contains the original URL.
    if installed_python_package.installer.as_deref() == Some("pixi") {
        let expected_hash = pkg
            .as_pypi()
            .and_then(|hash| hash.hash.as_ref())
            .and_then(|hash| hash.sha256())
            .map(|sha256| format!("sha256-{:x}", sha256));
        if let Some(expected_hash) = expected_hash {
            let hash_path = prefix_root
                .join(&installed_python_package.dist_info)
                .join("HASH");
            if let Ok(actual_hash) = std::fs::read_to_string(hash_path) {
                return actual_hash == expected_hash;
            }
        }
    }

    // Try to match the tags of both packages. This turns out to be pretty unreliable because
    // there are many WHEELS that do not report the tags of their filename correctly in the
    // WHEEL file.
    match (artifact_tags, &installed_python_package.tags) {
        (None, _) | (_, None) => {
            // One, or both, of the artifacts are not a wheel distribution so we cannot
            // currently compare them. In that case we always just reinstall.
            // TODO: Maybe log some info here?
            // TODO: Add support for more distribution types.
            false
        }
        (Some(locked_tags), Some(installed_tags)) => locked_tags == installed_tags,
    }
}
