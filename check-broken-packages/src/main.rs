use std::cmp;
use std::collections::VecDeque;
use std::fs;
use std::io::BufRead;
use std::iter::FromIterator;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::Arc;

use crossbeam::thread;
use log::debug;

type CrossbeamChannel<T> = (
    crossbeam::channel::Sender<T>,
    crossbeam::channel::Receiver<T>,
);

fn get_aur_packages() -> Vec<String> {
    let output = Command::new("pacman").args(&["-Qqm"]).output().unwrap();

    if !output.status.success() {
        panic!();
    }

    Vec::from_iter(output.stdout.lines().map(std::result::Result::unwrap))
}

fn get_package_executable_files(package: &str) -> VecDeque<String> {
    let mut files = VecDeque::new();

    let output = Command::new("pacman")
        .args(&["-Ql", package])
        .output()
        .unwrap();

    if !output.status.success() {
        panic!();
    }

    for line in output.stdout.lines() {
        let line = line.unwrap();
        let path = line.split(' ').nth(1).unwrap().to_string();
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_e) => continue,
        };
        if metadata.file_type().is_file() && ((metadata.permissions().mode() & 0o111) != 0) {
            files.push_back(path);
        }
    }

    files
}

fn get_missing_dependencies(exec_file: &str) -> VecDeque<String> {
    let mut missing_deps = VecDeque::new();

    let output = Command::new("ldd").args(&[exec_file]).output().unwrap();

    if output.status.success() {
        for missing_dep in output
            .stdout
            .lines()
            .map(std::result::Result::unwrap)
            .filter(|l| l.ends_with("=> not found"))
            .map(|l| l.split(' ').next().unwrap().trim_start().to_string())
        {
            missing_deps.push_back(missing_dep);
        }
    }

    missing_deps
}

fn main() {
    let cpu_count = num_cpus::get();

    // Missing deps channel
    let (missing_deps_tx, missing_deps_rx) = crossbeam::unbounded();

    thread::scope(|scope| {
        // Executable file channel
        let (exec_files_tx, exec_files_rx): CrossbeamChannel<(Arc<String>, Arc<String>)> =
            crossbeam::unbounded();

        // Executable files to missing deps workers
        for _ in 0..cpu_count {
            let exec_files_rx = exec_files_rx.clone();
            let missing_deps_tx = missing_deps_tx.clone();
            scope.spawn(move |_| {
                while let Ok((package, file)) = exec_files_rx.recv() {
                    debug!("exec_files_rx => {:?}", (&package, &file));
                    let missing_deps = get_missing_dependencies(&file);
                    for missing_dep in missing_deps {
                        let to_send = (Arc::clone(&package), Arc::clone(&file), missing_dep);
                        debug!("{:?} => missing_deps_tx", &to_send);
                        if missing_deps_tx.send(to_send).is_err() {
                            break;
                        }
                    }
                }
            });
        }

        // Drop this end of the channel, workers have their own clone
        drop(missing_deps_tx);

        thread::scope(|scope| {
            // Get package names
            let aur_packages = get_aur_packages();

            // Package name channel
            let (package_tx, package_rx): CrossbeamChannel<Arc<String>> = crossbeam::unbounded();

            // Package name to executable files workers
            let worker_count = cmp::min(cpu_count, aur_packages.len());
            for _ in 0..worker_count {
                let package_rx = package_rx.clone();
                let exec_files_tx = exec_files_tx.clone();
                scope.spawn(move |_| {
                    while let Ok(package) = package_rx.recv() {
                        debug!("package_rx => {:?}", package);
                        let exec_files = get_package_executable_files(&package);
                        for exec_file in exec_files {
                            let to_send = (Arc::clone(&package), Arc::new(exec_file));
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
    })
    .unwrap();

    for (package, file, missing_dep) in missing_deps_rx.iter() {
        println!(
            "File '{}' from package '{}' is missing dependency '{}'",
            file, package, missing_dep
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

        assert_eq!(
            get_missing_dependencies("dummy"),
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
