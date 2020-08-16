mod config;
mod grf;
mod patching;
mod thor;

use std::fs::File;
use std::io::prelude::*;
use std::io::SeekFrom;
use std::process::Command;
use std::thread;

use config::{parse_configuration, PatcherConfiguration};
use log::{info, trace, warn};
use patching::{apply_patch_to_disk, apply_patch_to_grf};
use thor::ThorArchive;
use url::Url;
use web_view::{Content, Handle, WebView};

const PATCH_LIST_FILE_NAME: &str = "plist.txt";

#[derive(Debug)]
struct PendingPatch {
    info: thor::ThorPatchInfo,
    local_file: File,
}

enum PatchingStatus {
    Ready,
    Error(String),                        // Error message
    DownloadInProgress(usize, usize),     // Downloaded, Total
    InstallationInProgress(usize, usize), // Installed, Total
}

fn main() {
    simple_logger::init().unwrap();
    // Read the JSON contents of the file as an instance of `PatcherConfiguration`.
    let config = match parse_configuration("./rpatchur.json") {
        Some(v) => v,
        None => return,
    };
    let webview = web_view::builder()
        .title("RPatchur")
        .content(Content::Url(config.web.index_url.clone()))
        .size(config.window.width, config.window.height)
        .resizable(config.window.resizable)
        .user_data(config.clone())
        .invoke_handler(|webview, arg| {
            match arg {
                "play" => handle_play(webview),
                "setup" => handle_setup(webview),
                "exit" => handle_exit(webview),
                "cancel_update" => handle_cancel_update(webview),
                "reset_cache" => handle_reset_cache(webview),
                _ => (),
            }
            Ok(())
        })
        .build()
        .unwrap();
    let webview_handle = webview.handle();
    let patching_thread = spawn_patching_thread(webview_handle, config);

    webview.run().unwrap();
    patching_thread.join().unwrap();
}

/// Opens the configured client with arguments, if needed
fn handle_play(webview: &mut WebView<PatcherConfiguration>) {
    let client_path: &String = &webview.user_data().play.path;
    let client_argument: &String = &webview.user_data().play.argument;
    match Command::new(client_path).arg(client_argument).spawn() {
        Ok(child) => trace!("Client started: pid={}", child.id()),
        Err(e) => {
            warn!("Failed to start client: {}", e);
        }
    }
}

/// Opens the configured 'Setup' software
fn handle_setup(webview: &mut WebView<PatcherConfiguration>) {
    let setup_exe: &String = &webview.user_data().setup.path;
    let setup_argument: &String = &webview.user_data().play.argument;
    match Command::new(setup_exe).arg(setup_argument).spawn() {
        Ok(child) => trace!("Setup software started: pid={}", child.id()),
        Err(e) => {
            warn!("Failed to start setup software: {}", e);
        }
    }
}

/// Exits the patcher cleanly
fn handle_exit(webview: &mut WebView<PatcherConfiguration>) {
    webview.exit();
}

/// Cancels the update process
fn handle_cancel_update(_webview: &mut WebView<PatcherConfiguration>) {
    warn!("FIXME: cancel_update");
}

/// Resets the cache used to keep track of already applied patches
fn handle_reset_cache(_webview: &mut WebView<PatcherConfiguration>) {
    warn!("FIXME: reset_cache");
}

fn spawn_patching_thread(
    webview_handle: Handle<PatcherConfiguration>,
    config: PatcherConfiguration,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        trace!("Patching thread started.");
        let report_error = |err_msg| {
            warn!("{}", err_msg);
            dispatch_patching_status(&webview_handle, PatchingStatus::Error(err_msg));
        };
        let patch_url = Url::parse(config.web.patch_url.as_str()).unwrap();
        let patch_list_url = patch_url.join(PATCH_LIST_FILE_NAME).unwrap();
        let resp = match reqwest::blocking::get(patch_list_url) {
            Ok(v) => v,
            Err(e) => {
                report_error(format!("Failed to retrieve the patch list: {}.", e));
                return;
            }
        };
        if !resp.status().is_success() {
            report_error(format!(
                "Patch list '{}' not found on the remote server.",
                PATCH_LIST_FILE_NAME
            ));
            return;
        }
        let patch_index_content = match resp.text() {
            Ok(v) => v,
            Err(_) => return,
        };
        info!("Parsing patch index...");
        let patch_list = thor::patch_list_from_string(patch_index_content.as_str());
        info!("Successfully fetched patch list: {:?}", patch_list);

        // Try fetching patch files
        info!("Downloading patches... ");
        let patch_count = patch_list.len();
        let mut patch_number: usize = 1;
        let mut pending_patch_queue: Vec<PendingPatch> = vec![];
        for patch in patch_list {
            dispatch_patching_status(
                &webview_handle,
                PatchingStatus::DownloadInProgress(patch_number, patch_count),
            );
            patch_number += 1;
            let patch_file_url = match patch_url.join(patch.file_name.as_str()) {
                Ok(v) => v,
                Err(_) => {
                    report_error(format!(
                        "Invalid file name '{}' given in '{}'.",
                        patch.file_name, PATCH_LIST_FILE_NAME
                    ));
                    return;
                }
            };
            let mut tmp_file = tempfile::tempfile().unwrap();
            let mut resp = reqwest::blocking::get(patch_file_url).unwrap();
            if !resp.status().is_success() {
                report_error(format!(
                    "Patch file '{}' not found on the remote server.",
                    patch.file_name
                ));
                return;
            }
            let _bytes_copied = match resp.copy_to(&mut tmp_file) {
                Ok(v) => v,
                Err(e) => {
                    report_error(format!(
                        "Failed to download file '{}': {}.",
                        patch.file_name, e
                    ));
                    return;
                }
            };
            // File's been downloaded, seek to start and add it to the queue
            let _offset = tmp_file.seek(SeekFrom::Start(0));
            pending_patch_queue.push(PendingPatch {
                info: patch,
                local_file: tmp_file,
            });
        }
        info!("Done");
        // Proceed with actual patching
        let current_working_dir = match std::env::current_dir() {
            Ok(v) => v,
            Err(e) => {
                report_error(format!(
                    "Failed to resolve current working directory: {}.",
                    e
                ));
                return;
            }
        };
        info!("Applying patches...");
        let patch_count = pending_patch_queue.len();
        let mut patch_number: usize = 1;
        for pending_patch in pending_patch_queue {
            info!("Processing {}", pending_patch.info.file_name);
            dispatch_patching_status(
                &webview_handle,
                PatchingStatus::InstallationInProgress(patch_number, patch_count),
            );
            patch_number += 1;
            let mut thor_archive = match ThorArchive::new(pending_patch.local_file) {
                Ok(v) => v,
                Err(e) => {
                    report_error(format!(
                        "Cannot read '{}': {}.",
                        pending_patch.info.file_name, e
                    ));
                    return;
                }
            };

            if thor_archive.use_grf_merging() {
                // Patch GRF file
                let patch_target_grf_name = {
                    if thor_archive.target_grf_name().is_empty() {
                        config.client.default_grf_name.clone()
                    } else {
                        thor_archive.target_grf_name()
                    }
                };
                trace!("Target GRF: {:?}", patch_target_grf_name);
                if let Err(e) = apply_patch_to_grf(
                    current_working_dir.join(&patch_target_grf_name),
                    &mut thor_archive,
                ) {
                    report_error(format!(
                        "Failed to patch '{}': {}.",
                        patch_target_grf_name, e
                    ));
                    return;
                }
            } else {
                // Patch root directory
                if let Err(e) = apply_patch_to_disk(&current_working_dir, &mut thor_archive) {
                    report_error(format!(
                        "Failed to apply patch '{}': {}.",
                        pending_patch.info.file_name, e
                    ));
                    return;
                }
            }
        }
        dispatch_patching_status(&webview_handle, PatchingStatus::Ready);
        info!("Patching finished!");
    })
}

fn dispatch_patching_status(webview_handle: &Handle<PatcherConfiguration>, status: PatchingStatus) {
    if let Err(e) = webview_handle.dispatch(move |webview| {
        let result = match status {
            PatchingStatus::Ready => webview.eval("patchingStatusReady()"),
            PatchingStatus::Error(msg) => {
                webview.eval(&format!("patchingStatusError(\"{}\")", msg))
            }
            PatchingStatus::DownloadInProgress(nb_downloaded, nb_total) => webview.eval(&format!(
                "patchingStatusDownloading({}, {})",
                nb_downloaded, nb_total
            )),
            PatchingStatus::InstallationInProgress(nb_installed, nb_total) => webview.eval(
                &format!("patchingStatusInstalling({}, {})", nb_installed, nb_total),
            ),
        };
        if let Err(e) = result {
            warn!("Failed to dispatch patching status: {}.", e);
        }
        Ok(())
    }) {
        warn!("Failed to dispatch patching status: {}.", e);
    }
}
