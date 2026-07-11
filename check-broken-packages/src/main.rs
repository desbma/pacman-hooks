//! Check for broken arch packages

use std::{
    env,
    ffi::OsStr,
    fmt, fs,
    fs::File,
    io::{BufRead as _, Read as _},
    os::unix::{ffi::OsStrExt as _, fs::PermissionsExt as _},
    path::{Path, PathBuf},
    process::Command,
    str::{self, FromStr as _, Utf8Error},
};

use anyhow::Context as _;
use colored::Colorize as _;
use glob::glob;
use indicatif::{ParallelProgressIterator as _, ProgressBar, ProgressDrawTarget, ProgressStyle};
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
        anyhow::bail!("Failed to query Python version with pacman");
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

// Exclude executables in commonly used non standard directories,
// likely to also use non standard library locations
const BLACKLISTED_EXE_DIRS: [&str; 2] = ["/opt/", "/usr/share/"];

fn is_executable_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|m| m.file_type().is_file() && ((m.permissions().mode() & 0o111) != 0))
}

/// List executable files of the given packages, defaulting to all "foreign" (AUR) packages
fn get_packages_executable_files(packages: &[String]) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let output = Command::new("pacman")
        .arg(if packages.is_empty() { "-Qlm" } else { "-Ql" })
        .args(packages)
        .env("LANG", "C")
        .output()?;

    if !output.status.success() {
        anyhow::bail!("Failed to list package files with pacman");
    }

    // Packages own a lot of files, so filter lines in parallel, and only allocate for the few
    // executable ones
    let files = output
        .stdout
        .par_split(|b| *b == b'\n')
        .filter_map(|line| {
            let mut fields = line.splitn(2, |b| *b == b' ');
            Some((fields.next()?, fields.next()?))
        })
        // Directories are listed with a trailing separator
        .filter(|(_package, path)| !path.ends_with(b"/"))
        .map(|(package, path)| (package, Path::new(OsStr::from_bytes(path))))
        .filter(|(_package, path)| !BLACKLISTED_EXE_DIRS.iter().any(|d| path.starts_with(d)))
        .filter(|(_package, path)| is_executable_file(path))
        .map(|(package, path)| Ok((str::from_utf8(package)?.to_owned(), path.to_owned())))
        .collect::<Result<_, Utf8Error>>()?;

    Ok(files)
}

const LDD_PATH: &str = "/usr/bin/ldd";

/// Get the dynamic loaders of the local architectures, from the ldd script that lists them
fn get_loaders() -> anyhow::Result<Vec<PathBuf>> {
    let ldd = fs::read_to_string(LDD_PATH)?;

    let loaders = ldd
        .lines()
        .find_map(|l| l.strip_prefix("RTLDLIST="))
        .ok_or_else(|| anyhow::anyhow!("Unable to find loaders in {LDD_PATH:?}"))?
        .trim_matches('"')
        .split_whitespace()
        .map(PathBuf::from)
        .collect();

    Ok(loaders)
}

fn parse_missing_dependencies(loader_output: &[u8]) -> anyhow::Result<Vec<PathBuf>> {
    Ok(str::from_utf8(loader_output)?
        .lines()
        .filter_map(|l| l.trim_start().strip_suffix(" => not found"))
        .map(PathBuf::from)
        .collect())
}

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

fn is_elf(path: &Path) -> anyhow::Result<bool> {
    let mut magic = [0; ELF_MAGIC.len()];
    let read = File::open(path)?.read(&mut magic)?;
    Ok(read == magic.len() && magic == ELF_MAGIC)
}

fn get_missing_dependencies(exec_path: &Path, loaders: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    // Skip scripts early, before probing loaders with as many process spawns
    if !is_elf(exec_path)? {
        return Ok(Vec::new());
    }

    let exec_dir = exec_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Unable to get parent dir for path {exec_path:?}"))?;

    // Select the loader that accepts this executable, this also excludes statically linked
    // binaries and foreign architectures, for which tracing would fail or crash the loader
    let Some(loader) = loaders.iter().find(|loader| {
        Command::new(loader)
            .arg("--verify")
            .arg(exec_path)
            .output()
            .is_ok_and(|o| matches!(o.status.code(), Some(0 | 2)))
    }) else {
        return Ok(Vec::new());
    };

    // Trace dependencies without running the executable, this is what ldd does internally
    let output = Command::new(loader)
        .arg(exec_path)
        .env("LANG", "C")
        .env("LD_TRACE_LOADED_OBJECTS", "1")
        .env("LD_LIBRARY_PATH", exec_dir)
        .output()?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    parse_missing_dependencies(&output.stdout)
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
        .filter(|e| e.file_type().is_ok_and(|f| f.is_symlink()))
        .map(|f| f.path())
        .collect();

    Ok(service_links)
}

/// List broken Python packages, logging errors instead of failing the whole run
fn get_broken_python_packages_or_log() -> Vec<(String, String)> {
    get_python_version()
        .inspect(|version| log::debug!("Python version: {version}"))
        .inspect_err(|err| log::error!("Failed to get Python version: {err}"))
        .ok()
        .and_then(|version| {
            get_broken_python_packages(&version)
                .inspect_err(|err| log::error!("Failed to list Python packages: {err}"))
                .ok()
        })
        .unwrap_or_default()
}

fn main() -> anyhow::Result<()> {
    // Init logger
    SimpleLogger::new()
        .init()
        .context("Failed to init logger")?;

    // Take package names from command line, default to "foreign" (AUR) packages
    let packages: Vec<String> = env::args().skip(1).collect();

    let (package_files, (enabled_sd_service_links, broken_python_packages)) = rayon::join(
        || {
            get_packages_executable_files(&packages)
                .context("Unable to get package executable files")
        },
        || {
            rayon::join(
                || get_sd_enabled_service_links().context("Unable to Systemd enabled services"),
                get_broken_python_packages_or_log,
            )
        },
    );
    let package_files = package_files?;
    let enabled_sd_service_links = enabled_sd_service_links?;

    // Init progressbar
    let progress = ProgressBar::with_draw_target(
        Some((package_files.len() + enabled_sd_service_links.len()) as u64),
        ProgressDrawTarget::stderr(),
    );
    progress.set_style(ProgressStyle::default_bar().template("Analyzing {wide_bar} {pos}/{len}")?);

    // Check systemd links
    let broken_sd_service_links: Vec<PathBuf> = progress
        .wrap_iter(enabled_sd_service_links.into_iter())
        .filter(|link| !link.is_file())
        .collect();

    // Check packages
    let loaders = get_loaders().context("Unable to get dynamic loaders")?;
    let missing_deps: Vec<(String, PathBuf, PathBuf)> = package_files
        .into_par_iter()
        .progress_with(progress.clone())
        .flat_map_iter(|(package, file)| {
            get_missing_dependencies(&file, &loaders)
                .unwrap_or_else(|e| {
                    log::error!(
                        "Failed to get missing dependencies for file {file:?} of package {package:?}: {e}"
                    );
                    Vec::new()
                })
                .into_iter()
                .map(move |m| (package.clone(), file.clone(), m))
        })
        .collect();

    progress.finish_and_clear();

    for (package, file, missing_dep) in &missing_deps {
        println!(
            "{}",
            format!("File {file:?} from package {package:?} is missing dependency {missing_dep:?}")
                .yellow()
        );
    }

    for (broken_python_package, dir) in broken_python_packages {
        println!(
            "{}",
            format!(
                "Package {broken_python_package:?} has files in directory {dir:?} that are ignored by the current Python interpreter"
            )
            .yellow()
        );
    }

    for broken_sd_service_link in broken_sd_service_links {
        println!(
            "{}",
            format!("Systemd enabled service has broken link in {broken_sd_service_link:?}")
                .yellow()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_dependencies() {
        let loader_output = "	linux-vdso.so.1 (0x00007ffea89a7000)
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

        assert_eq!(
            parse_missing_dependencies(loader_output.as_bytes()).unwrap(),
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
    }

    #[test]
    fn missing_dependencies_from_loader() {
        let loaders = get_loaders().unwrap();
        assert!(!loaders.is_empty());

        // This test binary runs, so all its dependencies are satisfied
        assert_eq!(
            get_missing_dependencies(&env::current_exe().unwrap(), &loaders).unwrap(),
            Vec::<PathBuf>::new()
        );

        // ldd is a shell script, no loader accepts it
        assert_eq!(
            get_missing_dependencies(Path::new(LDD_PATH), &loaders).unwrap(),
            Vec::<PathBuf>::new()
        );
    }
}
