use std::env;
use std::fmt;
use std::fs;
use std::io::BufRead;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;

use ansi_term::Colour::*;
use anyhow::Context;
use glob::glob;
use indicatif::{ParallelProgressIterator, ProgressBar, ProgressDrawTarget, ProgressStyle};
use rayon::prelude::*;
use simple_logger::SimpleLogger;

struct PythonPackageVersion {
    major: u8,
    minor: u8,
    release: u8,
    package: u8,
}

impl fmt::Display for PythonPackageVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}.{}-{}",
            self.major, self.minor, self.release, self.package
        )
    }
}

fn get_python_version() -> anyhow::Result<PythonPackageVersion> {
    let output = Command::new("pacman")
        .args(["-Qi", "python"])
        .env("LANG", "C")
        .output()?;

    if !output.status.success() {
        anyhow::bail!("Failed to query Python version with pacman",);
    }

    let version_line = output
        .stdout
        .lines()
        .map_while(Result::ok)
        .find(|l| l.starts_with("Version"))
        .ok_or_else(|| anyhow::anyhow!("Unexpected pacman output: unable to find version line"))?;
    let version_str = version_line
        .split(':')
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Unexpected pacman output: unable to parse version line"))?
        .trim_start();

    let mut dot_iter = version_str.split('.');
    let major = u8::from_str(dot_iter.next().ok_or_else(|| {
        anyhow::anyhow!("Unexpected pacman output: unable to parse Python version major part")
    })?)?;
    let minor = u8::from_str(dot_iter.next().ok_or_else(|| {
        anyhow::anyhow!("Unexpected pacman output: unable to parse Python version minor part")
    })?)?;
    let mut dash_iter = dot_iter
        .next()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Unexpected pacman output: unable to parse Python version release/package part",
            )
        })?
        .split('-');
    let release = u8::from_str(dash_iter.next().ok_or_else(|| {
        anyhow::anyhow!("Unexpected pacman output: unable to parse Python version release part")
    })?)?;
    let package = u8::from_str(dash_iter.next().ok_or_else(|| {
        anyhow::anyhow!("Unexpected pacman output: unable to parse Python version package part")
    })?)?;

    Ok(PythonPackageVersion {
        major,
        minor,
        release,
        package,
    })
}

fn get_package_owning_path(path: &str) -> anyhow::Result<Vec<String>> {
    let output = Command::new("pacman")
        .args(["-Qoq", path])
        .env("LANG", "C")
        .output()?;

    Ok(output.stdout.lines().collect::<Result<Vec<String>, _>>()?)
}

fn get_broken_python_packages(
    current_python_version: &PythonPackageVersion,
) -> anyhow::Result<Vec<(String, String)>> {
    let mut packages = Vec::new();

    let current_python_dir = format!(
        "/usr/lib/python{}.{}",
        current_python_version.major, current_python_version.minor
    );

    for python_dir_entry in glob(&format!("/usr/lib/python{}*", current_python_version.major))? {
        let python_dir = python_dir_entry?
            .into_os_string()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Failed to convert OS string to native string"))?;

        if python_dir != current_python_dir {
            let dir_packages = get_package_owning_path(&python_dir)?;
            for package in dir_packages {
                let couple = (package, python_dir.clone());
                if !packages.contains(&couple) {
                    packages.push(couple);
                }
            }
        }
    }

    Ok(packages)
}

fn get_aur_packages() -> anyhow::Result<Vec<String>> {
    let output = Command::new("pacman")
        .arg("-Qqm")
        .env("LANG", "C")
        .output()?;

    Ok(output.stdout.lines().collect::<Result<Vec<String>, _>>()?)
}

fn get_package_executable_files(package: &str) -> anyhow::Result<Vec<PathBuf>> {
    let output = Command::new("pacman")
        .args(["-Ql", package])
        .env("LANG", "C")
        .output()?;

    if !output.status.success() {
        anyhow::bail!("Failed to list files for package {:?} with pacman", package);
    }

    let files = output
        .stdout
        .lines()
        .collect::<Result<Vec<String>, _>>()?
        .into_iter()
        .filter_map(|l| l.split(' ').nth(1).map(PathBuf::from))
        .filter_map(|p| fs::metadata(&p).map(|m| (p, m)).ok())
        .filter_map(|(p, m)| {
            if m.is_symlink() {
                fs::read_link(&p)
                    .ok()
                    .and_then(|p| fs::metadata(&p).map(|m| (p, m)).ok())
            } else {
                Some((p, m))
            }
        })
        .filter(|(_p, m)| m.file_type().is_file() && ((m.permissions().mode() & 0o111) != 0))
        .map(|(p, _m)| p)
        .collect();

    Ok(files)
}

fn get_missing_dependencies(exec_path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let exec_dir = exec_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Unable to get parent dir for path {exec_path:?}"))?;
    let output = Command::new("ldd")
        .arg(exec_path)
        .env("LANG", "C")
        .env("LD_LIBRARY_PATH", exec_dir)
        .output()?;

    let missing_deps = if output.status.success() {
        output
            .stdout
            .lines()
            .collect::<Result<Vec<String>, _>>()?
            .into_iter()
            .filter(|l| l.ends_with("=> not found"))
            .filter_map(|l| l.split(' ').next().map(|s| PathBuf::from(s.trim_start())))
            .collect()
    } else {
        Vec::new()
    };

    Ok(missing_deps)
}

fn get_sd_enabled_service_links() -> anyhow::Result<Vec<PathBuf>> {
    let dirs_content = [
        glob("/etc/systemd/system/*.target.*"),
        glob("/etc/systemd/user/*.target.*"),
    ];

    let service_links: Vec<PathBuf> = dirs_content
        .into_iter()
        .flatten()
        .flatten()
        .collect::<Result<Vec<PathBuf>, _>>()?
        .into_iter()
        .filter_map(|p| fs::read_dir(p.as_path()).ok())
        .flatten()
        .flatten()
        .filter(|f| f.file_type().map_or(false, |f| f.is_symlink()))
        .map(|f| f.path())
        .collect();

    Ok(service_links)
}

fn is_valid_link(link: &Path) -> anyhow::Result<bool> {
    let mut target: PathBuf = link.into();
    loop {
        target = fs::read_link(target)?;
        let metadata = match fs::metadata(&target) {
            Err(_) => {
                return Ok(false);
            }
            Ok(m) => m,
        };

        let ftype = metadata.file_type();
        if ftype.is_file() {
            return Ok(true);
        } else if ftype.is_symlink() {
            continue;
        } else {
            anyhow::bail!("Unexpected file type for {:?}", target);
        }
    }
}

// Exclude executables in commonly used non standard directories,
// likely to also use non standard library locations
const BLACKLISTED_EXE_DIRS: [&str; 2] = ["/opt/", "/usr/share/"];

fn main() -> anyhow::Result<()> {
    // Init logger
    SimpleLogger::new()
        .init()
        .context("Failed to init logger")?;

    let mut packages = None;
    let mut enabled_sd_service_links = None;
    let mut broken_python_packages = None;
    rayon::scope(|scope| {
        scope.spawn(
            // Get package names
            |_| {
                packages = if env::args().len() > 1 {
                    // Take package names fromù command line
                    Some(Ok(env::args().skip(1).collect()))
                } else {
                    // Default to "foreign" (AUR) packages
                    Some(get_aur_packages().context("Unable to get list of AUR packages"))
                }
            },
        );
        scope.spawn(
            // Get systemd enabled services
            |_| {
                enabled_sd_service_links = Some(
                    get_sd_enabled_service_links().context("Unable to Systemd enabled services"),
                )
            },
        );
        scope.spawn(
            // Python broken packages
            |_| {
                broken_python_packages = match get_python_version() {
                    Ok(current_python_version) => {
                        log::debug!("Python version: {}", current_python_version);
                        let broken_python_packages =
                            get_broken_python_packages(&current_python_version);
                        match broken_python_packages {
                            Ok(broken_python_packages) => Some(broken_python_packages),
                            Err(err) => {
                                log::error!("Failed to list Python packages: {err}");
                                Some(Vec::<(String, String)>::new())
                            }
                        }
                    }
                    Err(err) => {
                        log::error!("Failed to get Python version: {err}");
                        Some(Vec::<(String, String)>::new())
                    }
                }
            },
        )
    });
    let packages = packages.unwrap()?;
    let enabled_sd_service_links = enabled_sd_service_links.unwrap()?;
    let broken_python_packages = broken_python_packages.unwrap();

    // Init progressbar
    let progress = ProgressBar::with_draw_target(
        Some((packages.len() + enabled_sd_service_links.len()) as u64),
        ProgressDrawTarget::stderr(),
    );
    progress.set_style(ProgressStyle::default_bar().template("Analyzing {wide_bar} {pos}/{len}")?);

    // Check systemd links
    let broken_sd_service_links: Vec<PathBuf> = progress
        .wrap_iter(enabled_sd_service_links.into_iter())
        .filter(|s| !is_valid_link(s).unwrap_or(true))
        .collect();

    // Check packages
    let missing_deps: Vec<(Arc<String>, Arc<PathBuf>, PathBuf)> = packages
        .into_par_iter()
        .progress_with(progress.clone())
        .map(|p| match get_package_executable_files(&p) {
            Ok(f) => {
                let pa = Arc::new(p);
                f.into_iter()
                    .filter(|f| !BLACKLISTED_EXE_DIRS.iter().any(|d| f.starts_with(d)))
                    .map(|f| (Arc::clone(&pa), f))
                    .collect()
            }
            Err(e) => {
                log::error!("Failed to get package executable files for {p:?}: {e}");
                Vec::new()
            }
        })
        .flatten()
        .map(|(pa, f)| match get_missing_dependencies(&f) {
            Ok(m) => {
                let fa = Arc::new(f);
                m.into_iter()
                    .map(|m| (Arc::clone(&pa), Arc::clone(&fa), m))
                    .collect()
            }
            Err(e) => {
                log::error!(
                    "Failed to get missing dependencies for file {f:?} of package {pa:?}: {e}"
                );
                Vec::new()
            }
        })
        .flatten()
        .collect();

    progress.finish_and_clear();

    for (package, file, missing_dep) in missing_deps.iter() {
        println!(
            "{}",
            Yellow.paint(format!(
                "File {file:?} from package {package:?} is missing dependency {missing_dep:?}"
            ))
        );
    }

    for (broken_python_package, dir) in broken_python_packages {
        println!(
            "{}",
            Yellow.paint(format!(
                "Package {broken_python_package:?} has files in directory {dir:?} that are ignored by the current Python interpreter"
            ))
        );
    }

    for broken_sd_service_link in broken_sd_service_links {
        println!(
            "{}",
            Yellow.paint(format!(
                "Systemd enabled service has broken link in {:?}",
                &broken_sd_service_link,
            ))
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs::{File, Permissions};
    use std::io::Write;
    use std::path::PathBuf;

    use super::*;

    fn update_path(dir: &str) -> std::ffi::OsString {
        let path_orig = env::var_os("PATH").unwrap();

        let mut paths_vec = env::split_paths(&path_orig).collect::<Vec<_>>();
        paths_vec.insert(0, PathBuf::from(dir));

        let paths = env::join_paths(paths_vec).unwrap();
        env::set_var("PATH", paths);

        path_orig
    }

    #[test]
    fn test_get_missing_dependencies() {
        let ldd_output = "	linux-vdso.so.1 (0x00007ffea89a7000)
	libavdevice.so.57 => not found
	libavfilter.so.6 => not found
	libavformat.so.57 => not found
	libavcodec.so.57 => not found
	libavresample.so.3 => not found
	libpostproc.so.54 => not found
	libswresample.so.2 => not found
	libswscale.so.4 => not found
	libavutil.so.55 => not found
	libm.so.6 => /usr/lib/libm.so.6 (0x00007f4bd9cc3000)
	libpthread.so.0 => /usr/lib/libpthread.so.0 (0x00007f4bd9ca2000)
	libc.so.6 => /usr/lib/libc.so.6 (0x00007f4bd9add000)
	/lib64/ld-linux-x86-64.so.2 => /usr/lib64/ld-linux-x86-64.so.2 (0x00007f4bda08d000)
";

        let tmp_dir = tempfile::TempDir::new().unwrap();

        let output_filepath = tmp_dir.path().join("output.txt");
        let mut output_file = File::create(&output_filepath).unwrap();
        output_file.write_all(ldd_output.as_bytes()).unwrap();
        drop(output_file);

        let fake_ldd_filepath = tmp_dir.path().join("ldd");
        let mut fake_ldd_file = File::create(fake_ldd_filepath).unwrap();
        write!(
            &mut fake_ldd_file,
            "#!/bin/sh\ncat {}",
            output_filepath.into_os_string().into_string().unwrap()
        )
        .unwrap();
        fake_ldd_file
            .set_permissions(Permissions::from_mode(0o700))
            .unwrap();
        drop(fake_ldd_file);

        let path_orig = update_path(tmp_dir.path().to_str().unwrap());

        let missing_deps = get_missing_dependencies(Path::new("dummy"));
        assert!(missing_deps.is_ok());
        assert_eq!(
            missing_deps.unwrap(),
            [
                Path::new("libavdevice.so.57"),
                Path::new("libavfilter.so.6"),
                Path::new("libavformat.so.57"),
                Path::new("libavcodec.so.57"),
                Path::new("libavresample.so.3"),
                Path::new("libpostproc.so.54"),
                Path::new("libswresample.so.2"),
                Path::new("libswscale.so.4"),
                Path::new("libavutil.so.55"),
            ]
        );

        env::set_var("PATH", path_orig);
    }
}
