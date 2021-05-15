use std::cmp;
use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::io::BufRead;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;
use std::thread;

use ansi_term::Colour::*;
use crossbeam::thread as cb_thread;
use glob::glob;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use log::debug;
use simple_logger::SimpleLogger;

type CrossbeamChannel<T> = (
    crossbeam::channel::Sender<T>,
    crossbeam::channel::Receiver<T>,
);

/// Executable file work unit for a worker thread to process
#[derive(Debug)]
struct ExecFileWork {
    /// AUR package name
    #[allow(clippy::rc_buffer)]
    package: Arc<String>,

    // Executable filepath
    #[allow(clippy::rc_buffer)]
    exec_filepath: Arc<String>,

    /// True if this is the last executable filepath for the package (used to report progress)
    package_last: bool,
}

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
        .args(&["-Qi", "python"])
        .env("LANG", "C")
        .output()?;

    if !output.status.success() {
        anyhow::bail!("Failed to query Python version with pacman",);
    }

    let version_line = output
        .stdout
        .lines()
        .filter_map(Result::ok)
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
    let output = Command::new("pacman").args(&["-Qoq", path]).output()?;

    Ok(output
        .stdout
        .lines()
        .map(std::result::Result::unwrap)
        .collect())
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
    let output = Command::new("pacman").args(&["-Qqm"]).output()?;

    if !output.status.success() {
        anyhow::bail!("Failed to list packages with pacman",);
    }

    Ok(output
        .stdout
        .lines()
        .map(std::result::Result::unwrap)
        .collect())
}

fn get_package_executable_files(package: &str) -> anyhow::Result<Vec<String>> {
    let mut files = Vec::new();

    let output = Command::new("pacman").args(&["-Ql", package]).output()?;

    if !output.status.success() {
        anyhow::bail!("Failed to list files for package '{}' with pacman", package);
    }

    for line in output.stdout.lines() {
        let line = line?;
        let path = line
            .split(' ')
            .nth(1)
            .ok_or_else(|| {
                anyhow::anyhow!("Unexpected pacman output: unable to parse package file list")
            })?
            .to_string();
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_e) => continue,
        };
        if metadata.file_type().is_file() && ((metadata.permissions().mode() & 0o111) != 0) {
            files.push(path);
        }
    }

    Ok(files)
}

fn get_missing_dependencies(exec_file: &str) -> anyhow::Result<Vec<String>> {
    let mut missing_deps = Vec::new();

    let output = Command::new("ldd")
        .env("LANG", "C")
        .args(&[exec_file])
        .output()?;

    if output.status.success() {
        for missing_dep in output
            .stdout
            .lines()
            .map(std::result::Result::unwrap)
            .filter(|l| l.ends_with("=> not found"))
            .map(|l| l.split(' ').next().unwrap().trim_start().to_string())
        {
            missing_deps.push(missing_dep);
        }
    }

    Ok(missing_deps)
}

fn get_sd_enabled_service_links() -> anyhow::Result<VecDeque<String>> {
    let mut service_links = VecDeque::new();

    let mut dirs_content = [
        glob("/etc/systemd/system/*.target.*"),
        glob("/etc/systemd/user/*.target.*"),
    ];
    for dir_content in dirs_content.iter_mut().flatten() {
        for base_dir in dir_content.flatten() {
            for file in std::fs::read_dir(base_dir.as_path())
                .unwrap()
                .map(Result::unwrap)
            {
                if file.file_type()?.is_symlink() {
                    service_links.push_back(file.path().into_os_string().into_string().unwrap());
                }
            }
        }
    }

    Ok(service_links)
}

fn is_valid_link(link: &str) -> anyhow::Result<bool> {
    let mut target = link.to_string();
    loop {
        target = fs::read_link(target)?
            .into_os_string()
            .into_string()
            .unwrap();
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
            anyhow::bail!("Unexpected file type for '{}'", target);
        }
    }
}

fn main() {
    // Init logger
    SimpleLogger::new().init().unwrap();

    // Python broken packages channel
    let (python_broken_packages_tx, python_broken_packages_rx) = crossbeam::unbounded();
    thread::Builder::new()
        .spawn(move || {
            let to_send = match get_python_version() {
                Ok(current_python_version) => {
                    debug!("Python version: {}", current_python_version);
                    let broken_python_packages =
                        get_broken_python_packages(&current_python_version);
                    match broken_python_packages {
                        Ok(broken_python_packages) => broken_python_packages,
                        Err(err) => {
                            eprintln!("Failed to list Python packages: {}", err);
                            Vec::<(String, String)>::new()
                        }
                    }
                }
                Err(err) => {
                    eprintln!("Failed to get Python version: {}", err);
                    Vec::<(String, String)>::new()
                }
            };
            python_broken_packages_tx.send(to_send).unwrap();
        })
        .unwrap();

    // Get usable core count
    let cpu_count = num_cpus::get();

    // Get package names
    let aur_packages = get_aur_packages().unwrap();

    // Get systemd enabled services
    let enabled_sd_service_links = get_sd_enabled_service_links().unwrap();
    let mut broken_sd_service_links: VecDeque<String> = VecDeque::new();

    // Init progressbar
    let progress = ProgressBar::with_draw_target(
        (aur_packages.len() + enabled_sd_service_links.len()) as u64,
        ProgressDrawTarget::stderr(),
    );
    progress.set_style(ProgressStyle::default_bar().template("Analyzing {wide_bar} {pos}/{len}"));

    // Missing deps channel
    let (missing_deps_tx, missing_deps_rx) = crossbeam::unbounded();

    cb_thread::scope(|scope| {
        // Executable file channel
        let (exec_files_tx, exec_files_rx): CrossbeamChannel<ExecFileWork> = crossbeam::unbounded();

        // Executable files to missing deps workers
        for _ in 0..cpu_count {
            let exec_files_rx = exec_files_rx.clone();
            let missing_deps_tx = missing_deps_tx.clone();
            let progress = progress.clone();
            scope.spawn(move |_| {
                while let Ok(exec_file_work) = exec_files_rx.recv() {
                    debug!("exec_files_rx => {:?}", &exec_file_work);
                    let missing_deps = get_missing_dependencies(&exec_file_work.exec_filepath);
                    match missing_deps {
                        Ok(missing_deps) => {
                            for missing_dep in missing_deps {
                                let to_send = (
                                    Arc::clone(&exec_file_work.package),
                                    Arc::clone(&exec_file_work.exec_filepath),
                                    missing_dep,
                                );
                                debug!("{:?} => missing_deps_tx", &to_send);
                                if missing_deps_tx.send(to_send).is_err() {
                                    break;
                                }
                            }
                        }
                        Err(err) => {
                            eprintln!(
                                "Failed to get missing dependencies for path '{}': {}",
                                &exec_file_work.exec_filepath, err
                            );
                        }
                    }
                    if exec_file_work.package_last {
                        progress.inc(1);
                    }
                }
            });
        }

        // Drop this end of the channel, workers have their own clone
        drop(missing_deps_tx);

        cb_thread::scope(|scope| {
            // Package name channel
            let (package_tx, package_rx): CrossbeamChannel<Arc<String>> = crossbeam::unbounded();

            // Package name to executable files workers
            let worker_count = cmp::min(cpu_count, aur_packages.len());
            for _ in 0..worker_count {
                let package_rx = package_rx.clone();
                let exec_files_tx = exec_files_tx.clone();
                let progress = progress.clone();
                scope.spawn(move |_| {
                    while let Ok(package) = package_rx.recv() {
                        debug!("package_rx => {:?}", package);
                        let exec_files = match get_package_executable_files(&package) {
                            Ok(exec_files) => exec_files,
                            Err(err) => {
                                eprintln!(
                                    "Failed to get executable files of package '{}': {}",
                                    &package, err
                                );
                                progress.inc(1);
                                continue;
                            }
                        };
                        if exec_files.is_empty() {
                            progress.inc(1);
                            continue;
                        }
                        for (i, exec_file) in exec_files.iter().enumerate() {
                            let to_send = ExecFileWork {
                                package: Arc::clone(&package),
                                exec_filepath: Arc::new(exec_file.to_string()),
                                package_last: i == exec_files.len() - 1,
                            };
                            debug!("{:?} => exec_files_tx", &to_send);
                            if exec_files_tx.send(to_send).is_err() {
                                break;
                            }
                        }
                    }
                });
            }

            // Drop this end of the channel, workers have their own clone
            drop(exec_files_tx);

            // Send package names
            for aur_package in aur_packages {
                debug!("{:?} => package_tx", aur_package);
                package_tx.send(Arc::new(aur_package)).unwrap();
            }
        })
        .unwrap();

        // We don't bother to use a worker thread for this, the overhead is not worth it
        for enabled_sd_service_link in enabled_sd_service_links {
            if !is_valid_link(&enabled_sd_service_link).unwrap() {
                broken_sd_service_links.push_back(enabled_sd_service_link);
            }
            progress.inc(1);
        }
    })
    .unwrap();

    progress.finish_and_clear();

    for (package, file, missing_dep) in missing_deps_rx.iter() {
        println!(
            "{}",
            Yellow.paint(format!(
                "File '{}' from package '{}' is missing dependency '{}'",
                file, package, missing_dep
            ))
        );
    }

    if let Ok(broken_python_packages) = python_broken_packages_rx.recv() {
        for (broken_python_package, dir) in broken_python_packages {
            println!(
                "{}",
                Yellow.paint(format!(
                    "Package '{}' has files in directory '{}' that are ignored by the current Python interpreter",
                    broken_python_package, dir
                ))
            );
        }
    }

    for broken_sd_service_link in broken_sd_service_links {
        println!(
            "{}",
            Yellow.paint(format!(
                "Systemd enabled service has broken link in '{}'",
                &broken_sd_service_link,
            ))
        );
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs::{File, Permissions};
    use std::io::Write;
    use std::path::PathBuf;

    use tempdir::TempDir;

    use super::*;

    fn update_path(dir: &str) -> std::ffi::OsString {
        let path_orig = env::var_os("PATH").unwrap();

        let mut paths_vec = env::split_paths(&path_orig).collect::<Vec<_>>();
        paths_vec.insert(0, PathBuf::from(dir));

        let paths = env::join_paths(paths_vec).unwrap();
        env::set_var("PATH", &paths);

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

        let tmp_dir = TempDir::new("").unwrap();

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
            .set_permissions(Permissions::from_mode(0o777))
            .unwrap();
        drop(fake_ldd_file);

        let path_orig = update_path(tmp_dir.path().to_str().unwrap());

        let missing_deps = get_missing_dependencies("dummy");
        assert!(missing_deps.is_ok());
        assert_eq!(
            missing_deps.unwrap(),
            [
                "libavdevice.so.57",
                "libavfilter.so.6",
                "libavformat.so.57",
                "libavcodec.so.57",
                "libavresample.so.3",
                "libpostproc.so.54",
                "libswresample.so.2",
                "libswscale.so.4",
                "libavutil.so.55"
            ]
        );

        env::set_var("PATH", &path_orig);
    }
}
