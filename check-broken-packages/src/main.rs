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

fn get_aur_packages() -> Vec<String> {
    let output = Command::new("pacman").args(&["-Qqm"]).output().unwrap();

    if !output.status.success() {
        panic!();
    }

    Vec::from_iter(output.stdout.lines().map(std::result::Result::unwrap))
}

fn get_package_executable_files(package: &String) -> VecDeque<String> {
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

fn get_missing_dependencies(binary: &String) -> VecDeque<String> {
    let mut missing_deps = VecDeque::new();

    let output = Command::new("ldd").args(&[binary]).output().unwrap();

    if output.status.success() {
        for line in output.stdout.lines() {
            let line = line.unwrap();
            if line.ends_with("=> not found") {
                let dep = line.split(' ').next().unwrap().trim_start().to_string();
                missing_deps.push_back(dep);
            }
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
        let (exec_files_tx, exec_files_rx): (
            crossbeam::channel::Sender<(Arc<String>, Arc<String>)>,
            crossbeam::channel::Receiver<(Arc<String>, Arc<String>)>,
        ) = crossbeam::unbounded();

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
            let (package_tx, package_rx): (
                crossbeam::channel::Sender<Arc<String>>,
                crossbeam::channel::Receiver<Arc<String>>,
            ) = crossbeam::unbounded();

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
