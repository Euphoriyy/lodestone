use std::{collections::HashSet, path::PathBuf};

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Multipart, Path},
    routing::{delete, get, put},
    Json, Router,
};
use axum_auth::AuthBearer;
use color_eyre::eyre::{eyre, Context};
use fs_extra::TransitProcess;
use headers::HeaderMap;
use reqwest::header::CONTENT_LENGTH;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tracing::debug;
use ts_rs::TS;
use walkdir::WalkDir;

use crate::{
    auth::user::UserAction,
    error::{Error, ErrorKind},
    events::{
        new_fs_event, CausedBy, Event, EventInner, FSOperation, FSTarget, ProgressionEndValue,
        ProgressionEvent, ProgressionEventInner,
    },
    traits::t_configurable::TConfigurable,
    types::{InstanceUuid, Snowflake},
    util::{
        format_byte, format_byte_download, list_dir, rand_alphanumeric, scoped_join_win_safe,
        unzip_file_async, zip_files_async, UnzipOption,
    },
    AppState,
};

// list of protected file extension that cannot be modified
static PROTECTED_EXTENSIONS: [&str; 10] = [
    "jar",
    "lua",
    "sh",
    "exe",
    "bat",
    "cmd",
    "msi",
    "lodestone_config",
    "out",
    "inf",
];

static PROTECTED_DIR_NAME: [&str; 1] = ["mods"];

fn is_path_protected(path: impl AsRef<std::path::Path>) -> bool {
    let path = path.as_ref();
    if path.is_dir() {
        path.file_name()
            .and_then(|s| s.to_str().map(|s| PROTECTED_DIR_NAME.contains(&s)))
            .unwrap_or(true)
    } else if let Some(ext) = path.extension() {
        ext.to_str()
            .map(|s| PROTECTED_EXTENSIONS.contains(&s))
            .unwrap_or(true)
    } else {
        true
    }
}

use super::{global_fs::FileEntry, util::decode_base64};

async fn list_instance_files(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path((uuid, base64_relative_path)): Path<(InstanceUuid, String)>,
    AuthBearer(token): AuthBearer,
) -> Result<Json<Vec<FileEntry>>, Error> {
    let relative_path = decode_base64(&base64_relative_path)?;
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;

    requester.try_action(&UserAction::ReadInstanceFile(uuid.clone()))?;
    let instances = state.instances.lock().await;
    let instance = instances.get(&uuid).ok_or_else(|| Error {
        kind: ErrorKind::NotFound,
        source: eyre!("Instance not found"),
    })?;
    let root = instance.path().await;
    drop(instances);
    let path = scoped_join_win_safe(&root, relative_path)?;

    let ret: Vec<FileEntry> = list_dir(&path, None)
        .await?
        .iter()
        .map(move |p| {
            // remove the root path from the file path
            let mut r: FileEntry = p.as_path().into();
            r.path = p.strip_prefix(&root).unwrap().to_str().unwrap().to_string();
            r
        })
        .collect();
    let caused_by = CausedBy::User {
        user_id: requester.uid,
        user_name: requester.username,
    };
    state.event_broadcaster.send(new_fs_event(
        FSOperation::Read,
        FSTarget::Directory(path),
        caused_by,
    ));
    Ok(Json(ret))
}

async fn read_instance_file(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path((uuid, base64_relative_path)): Path<(InstanceUuid, String)>,
    AuthBearer(token): AuthBearer,
) -> Result<String, Error> {
    let relative_path = decode_base64(&base64_relative_path)?;
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;
    requester.try_action(&UserAction::ReadInstanceFile(uuid.clone()))?;
    let instances = state.instances.lock().await;
    let instance = instances.get(&uuid).ok_or_else(|| Error {
        kind: ErrorKind::NotFound,
        source: eyre!("Instance not found"),
    })?;
    let root = instance.path().await;
    drop(instances);
    let path = scoped_join_win_safe(root, relative_path)?;

    let ret = crate::util::fs::read_to_string(&path).await?;
    let caused_by = CausedBy::User {
        user_id: requester.uid,
        user_name: requester.username,
    };
    state.event_broadcaster.send(new_fs_event(
        FSOperation::Read,
        FSTarget::File(path),
        caused_by,
    ));
    Ok(ret)
}

async fn write_instance_file(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path((uuid, base64_relative_path)): Path<(InstanceUuid, String)>,
    AuthBearer(token): AuthBearer,
    body: Bytes,
) -> Result<Json<()>, Error> {
    let relative_path = decode_base64(&base64_relative_path)?;
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;
    requester.try_action(&UserAction::WriteInstanceFile(uuid.clone()))?;
    let instances = state.instances.lock().await;
    let instance = instances.get(&uuid).ok_or_else(|| Error {
        kind: ErrorKind::NotFound,
        source: eyre!("Instance not found"),
    })?;
    let root = instance.path().await;
    drop(instances);
    let path = scoped_join_win_safe(root, relative_path)?;
    // if target has a protected extension, or no extension, deny
    if !requester.can_perform_action(&UserAction::WriteGlobalFile) && is_path_protected(&path) {
        return Err(Error {
            kind: ErrorKind::PermissionDenied,
            source: eyre!("You don't have permission to write to this file"),
        });
    }
    // create the file if it doesn't exist
    crate::util::fs::write_all(&path, body).await?;

    let caused_by = CausedBy::User {
        user_id: requester.uid,
        user_name: requester.username,
    };
    state.event_broadcaster.send(new_fs_event(
        FSOperation::Write,
        FSTarget::File(path),
        caused_by,
    ));
    Ok(Json(()))
}

async fn make_instance_directory(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path((uuid, base64_relative_path)): Path<(InstanceUuid, String)>,
    AuthBearer(token): AuthBearer,
) -> Result<Json<()>, Error> {
    let relative_path = decode_base64(&base64_relative_path)?;
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;
    requester.try_action(&UserAction::WriteInstanceFile(uuid.clone()))?;
    let instances = state.instances.lock().await;
    let instance = instances.get(&uuid).ok_or_else(|| Error {
        kind: ErrorKind::NotFound,
        source: eyre!("Instance not found"),
    })?;
    let root = instance.path().await;
    drop(instances);
    let path = scoped_join_win_safe(root, relative_path)?;
    // create the file if it doesn't exist
    crate::util::fs::create_dir_all(&path).await?;

    let caused_by = CausedBy::User {
        user_id: requester.uid,
        user_name: requester.username,
    };
    state.event_broadcaster.send(new_fs_event(
        FSOperation::Create,
        FSTarget::Directory(path),
        caused_by,
    ));
    Ok(Json(()))
}

#[derive(Deserialize, TS)]
#[ts(export)]
struct CopyInstanceFileRequest {
    relative_paths_source: Vec<PathBuf>,
    relative_path_dest: PathBuf,
}

async fn copy_instance_files(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(uuid): Path<InstanceUuid>,
    AuthBearer(token): AuthBearer,
    Json(CopyInstanceFileRequest {
        relative_paths_source,
        relative_path_dest,
    }): Json<CopyInstanceFileRequest>,
) -> Result<Json<()>, Error> {
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;
    requester.try_action(&UserAction::WriteInstanceFile(uuid.clone()))?;
    let instances = state.instances.lock().await;
    let instance = instances.get(&uuid).ok_or_else(|| Error {
        kind: ErrorKind::NotFound,
        source: eyre!("Instance not found"),
    })?;
    let root = instance.path().await;
    drop(instances);
    // join each path to the root
    let paths_source = relative_paths_source
        .iter()
        .map(|p| scoped_join_win_safe(root.clone(), p))
        .collect::<Result<Vec<_>, _>>()?;

    let path_dest = scoped_join_win_safe(root, &relative_path_dest)?;

    if !requester.can_perform_action(&UserAction::WriteGlobalFile) && is_path_protected(&path_dest)
    {
        return Err(Error {
            kind: ErrorKind::PermissionDenied,
            source: eyre!("You don't have permission to write to this file"),
        });
    }

    let event_broadcaster = state.event_broadcaster.clone();

    tokio::task::spawn_blocking(move || {
        let caused_by = CausedBy::User {
            user_id: requester.uid,
            user_name: requester.username,
        };
        let event_id = Snowflake::default();

        let mut first = true;

        let mut threshold = 500000_u64;

        let mut elapsed_bytes = 0_u64;
        let mut last_progression = 0_u64;

        let handle = |process_info: TransitProcess| {
            if first {
                threshold = process_info.total_bytes / 100;
                event_broadcaster.send(Event {
                    event_inner: EventInner::ProgressionEvent(ProgressionEvent {
                        event_id,
                        progression_event_inner: ProgressionEventInner::ProgressionStart {
                            progression_name: "Copying file(s)".to_string(),
                            producer_id: None,
                            total: Some(process_info.total_bytes as f64),
                            inner: None,
                        },
                    }),
                    details: "".to_string(),
                    snowflake: Snowflake::default(),
                    caused_by: caused_by.clone(),
                });
                first = false;
                elapsed_bytes = process_info.copied_bytes;
            } else {
                elapsed_bytes = process_info.copied_bytes;
                let progression = elapsed_bytes / threshold;
                if progression > last_progression {
                    last_progression = progression;
                    event_broadcaster.send(Event {
                        event_inner: EventInner::ProgressionEvent(ProgressionEvent {
                            event_id,
                            progression_event_inner: ProgressionEventInner::ProgressionUpdate {
                                progress_message: format!(
                                    "Copying file {}, {}",
                                    process_info.file_name,
                                    format_byte_download(
                                        process_info.copied_bytes,
                                        process_info.total_bytes
                                    )
                                ),
                                progress: threshold as f64,
                            },
                        }),
                        details: "".to_string(),
                        snowflake: Snowflake::default(),
                        caused_by: caused_by.clone(),
                    });
                }
            }
            fs_extra::dir::TransitProcessResult::SkipAll
        };
        debug!("Copying {:?} to {:?}", paths_source, path_dest);
        if let Err(e) = fs_extra::copy_items_with_progress(
            &paths_source,
            &path_dest,
            &fs_extra::dir::CopyOptions::new(),
            handle,
        ) {
            debug!("Error copying file(s): {}", e);
            event_broadcaster.send(Event {
                event_inner: EventInner::ProgressionEvent(ProgressionEvent {
                    event_id,
                    progression_event_inner: ProgressionEventInner::ProgressionEnd {
                        success: false,
                        message: Some(format!("Error copying file(s): {}", e)),
                        inner: Some(ProgressionEndValue::FSOperationCompleted {
                            instance_uuid: uuid,
                            success: false,
                            message: format!("Error copying file(s): {}", e),
                        }),
                    },
                }),
                details: "".to_string(),
                snowflake: Snowflake::default(),
                caused_by: caused_by.clone(),
            });
        } else {
            event_broadcaster.send(Event {
                event_inner: EventInner::ProgressionEvent(ProgressionEvent {
                    event_id,
                    progression_event_inner: ProgressionEventInner::ProgressionEnd {
                        success: true,
                        message: None,
                        inner: Some(ProgressionEndValue::FSOperationCompleted {
                            instance_uuid: uuid,
                            success: true,
                            message: "File(s) copied successfully".to_string(),
                        }),
                    },
                }),
                details: "".to_string(),
                snowflake: Snowflake::default(),
                caused_by: caused_by.clone(),
            });
        }
    });
    Ok(Json(()))
}

async fn move_instance_file(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path((uuid, base64_relative_path_source, base64_relative_path_dest)): Path<(
        InstanceUuid,
        String,
        String,
    )>,
    AuthBearer(token): AuthBearer,
) -> Result<Json<()>, Error> {
    let relative_path_source = decode_base64(&base64_relative_path_source)?;
    let relative_path_dest = decode_base64(&base64_relative_path_dest)?;
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;
    requester.try_action(&UserAction::WriteInstanceFile(uuid.clone()))?;
    let instances = state.instances.lock().await;
    let instance = instances.get(&uuid).ok_or_else(|| Error {
        kind: ErrorKind::NotFound,
        source: eyre!("Instance not found"),
    })?;
    let root = instance.path().await;
    drop(instances);
    let path_source = scoped_join_win_safe(&root, relative_path_source)?;
    let path_dest = scoped_join_win_safe(&root, relative_path_dest)?;

    if !requester.can_perform_action(&UserAction::WriteInstanceFile(uuid.clone()))
        && (is_path_protected(&path_source) || is_path_protected(&path_dest))
    {
        return Err(Error {
            kind: ErrorKind::PermissionDenied,
            source: eyre!("File extension is protected"),
        });
    }
    crate::util::fs::rename(&path_source, &path_dest).await?;

    let caused_by = CausedBy::User {
        user_id: requester.uid,
        user_name: requester.username,
    };

    state.event_broadcaster.send(new_fs_event(
        FSOperation::Move {
            source: path_source.clone(),
        },
        FSTarget::File(path_source),
        caused_by,
    ));

    Ok(Json(()))
}

async fn remove_instance_file(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path((uuid, base64_relative_path)): Path<(InstanceUuid, String)>,
    AuthBearer(token): AuthBearer,
) -> Result<Json<()>, Error> {
    let relative_path = decode_base64(&base64_relative_path)?;
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;
    requester.try_action(&UserAction::WriteInstanceFile(uuid.clone()))?;
    let instances = state.instances.lock().await;
    let instance = instances.get(&uuid).ok_or_else(|| Error {
        kind: ErrorKind::NotFound,
        source: eyre!("Instance not found"),
    })?;
    let root = instance.path().await;
    drop(instances);
    let path = scoped_join_win_safe(root, relative_path)?;
    // if target has a protected extension, or no extension, deny
    if !requester.can_perform_action(&UserAction::WriteGlobalFile) && is_path_protected(&path) {
        return Err(Error {
            kind: ErrorKind::PermissionDenied,
            source: eyre!("File extension is protected"),
        });
    }

    crate::util::fs::remove_file(&path).await?;

    let caused_by = CausedBy::User {
        user_id: requester.uid,
        user_name: requester.username,
    };
    state.event_broadcaster.send(new_fs_event(
        FSOperation::Delete,
        FSTarget::File(path),
        caused_by,
    ));
    Ok(Json(()))
}

async fn remove_instance_dir(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path((uuid, base64_relative_path)): Path<(InstanceUuid, String)>,
    AuthBearer(token): AuthBearer,
) -> Result<Json<()>, Error> {
    let relative_path = decode_base64(&base64_relative_path)?;
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;
    requester.try_action(&UserAction::WriteInstanceFile(uuid.clone()))?;
    let instances = state.instances.lock().await;
    let instance = instances.get(&uuid).ok_or_else(|| Error {
        kind: ErrorKind::NotFound,
        source: eyre!("Instance not found"),
    })?;
    let root = instance.path().await;
    drop(instances);
    let path = scoped_join_win_safe(&root, relative_path)?;
    if path == root {
        return Err(Error {
            kind: ErrorKind::PermissionDenied,
            source: eyre!("Cannot delete instance root"),
        });
    }
    // if target has a protected extension, or no extension, deny
    if !requester.can_perform_action(&UserAction::WriteGlobalFile) && is_path_protected(&path) {
        return Err(Error {
            kind: ErrorKind::PermissionDenied,
            source: eyre!("File extension is protected"),
        });
    }

    if requester.can_perform_action(&UserAction::WriteGlobalFile) {
        crate::util::fs::remove_dir_all(&path).await?;
    } else {
        // recursively access all files in the directory and check if they are protected
        for entry in WalkDir::new(path.clone()) {
            let entry =
                entry.context("Failed to walk directory while scanning for protected files")?;
            if entry.file_type().is_file() && is_path_protected(entry.path()) {
                return Err(Error {
                    kind: ErrorKind::PermissionDenied,
                    source: eyre!("File extension is protected"),
                });
            }
        }
        crate::util::fs::remove_dir_all(&path).await?;
    }

    let caused_by = CausedBy::User {
        user_id: requester.uid,
        user_name: requester.username,
    };
    state.event_broadcaster.send(new_fs_event(
        FSOperation::Delete,
        FSTarget::Directory(path),
        caused_by,
    ));
    Ok(Json(()))
}

async fn new_instance_file(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path((uuid, base64_relative_path)): Path<(InstanceUuid, String)>,
    AuthBearer(token): AuthBearer,
) -> Result<Json<()>, Error> {
    let relative_path = decode_base64(&base64_relative_path)?;
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;
    requester.try_action(&UserAction::WriteInstanceFile(uuid.clone()))?;
    let instances = state.instances.lock().await;
    let instance = instances.get(&uuid).ok_or_else(|| Error {
        kind: ErrorKind::NotFound,
        source: eyre!("Instance not found"),
    })?;
    let root = instance.path().await;
    drop(instances);
    let path = scoped_join_win_safe(root, relative_path)?;
    // if target has a protected extension, or no extension, deny
    if !requester.can_perform_action(&UserAction::WriteGlobalFile) && is_path_protected(&path) {
        return Err(Error {
            kind: ErrorKind::PermissionDenied,
            source: eyre!("File extension is protected"),
        });
    }

    crate::util::fs::create(&path).await?;

    let caused_by = CausedBy::User {
        user_id: requester.uid,
        user_name: requester.username,
    };
    state.event_broadcaster.send(new_fs_event(
        FSOperation::Create,
        FSTarget::File(path),
        caused_by,
    ));
    Ok(Json(()))
}

async fn download_instance_file(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path((uuid, base64_relative_path)): Path<(InstanceUuid, String)>,
    AuthBearer(token): AuthBearer,
) -> Result<String, Error> {
    let relative_path = decode_base64(&base64_relative_path)?;
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;
    requester.try_action(&UserAction::ReadInstanceFile(uuid.clone()))?;
    let instances = state.instances.lock().await;
    let instance = instances.get(&uuid).ok_or_else(|| Error {
        kind: ErrorKind::NotFound,
        source: eyre!("Instance not found"),
    })?;
    let root = instance.path().await;
    drop(instances);
    let path = scoped_join_win_safe(&root, relative_path)?;

    let key = rand_alphanumeric(32);
    state
        .download_urls
        .lock()
        .await
        .insert(key.clone(), path.clone());

    state.download_urls.lock().await.get(&key).unwrap();

    let caused_by = CausedBy::User {
        user_id: requester.uid,
        user_name: requester.username,
    };
    state.event_broadcaster.send(new_fs_event(
        FSOperation::Download,
        FSTarget::File(path),
        caused_by,
    ));
    Ok(key)
}

async fn upload_instance_file(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path((uuid, base64_relative_path)): Path<(InstanceUuid, String)>,
    headers: HeaderMap,
    AuthBearer(token): AuthBearer,
    mut multipart: Multipart,
) -> Result<Json<()>, Error> {
    let relative_path = decode_base64(&base64_relative_path)?;
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;
    requester.try_action(&UserAction::WriteInstanceFile(uuid.clone()))?;
    let instances = state.instances.lock().await;
    let instance = instances.get(&uuid).ok_or_else(|| Error {
        kind: ErrorKind::NotFound,
        source: eyre!("Instance not found"),
    })?;
    let root = instance.path().await;
    drop(instances);
    let path_to_dir = scoped_join_win_safe(&root, relative_path)?;
    crate::util::fs::create_dir_all(&path_to_dir).await?;

    let event_id = Snowflake::default();
    let total = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<f64>().ok());
    state.event_broadcaster.send(Event {
        event_inner: EventInner::ProgressionEvent(ProgressionEvent {
            event_id,
            progression_event_inner: ProgressionEventInner::ProgressionStart {
                progression_name: "Uploading files".to_string(),
                producer_id: None,
                total,
                inner: None,
            },
        }),
        details: "".to_string(),
        snowflake: Snowflake::default(),
        caused_by: CausedBy::User {
            user_id: requester.uid.clone(),
            user_name: requester.username.clone(),
        },
    });
    while let Ok(Some(mut field)) = multipart.next_field().await {
        let name = field.file_name().ok_or_else(|| Error {
            kind: ErrorKind::BadRequest,
            source: eyre!("Missing file name"),
        })?;
        let name = sanitize_filename::sanitize(name);
        let path = scoped_join_win_safe(&path_to_dir, &name)?;
        // if the file has a protected extension, or no extension, deny
        if !requester.can_perform_action(&UserAction::WriteGlobalFile) && is_path_protected(&path) {
            return Err(Error {
                kind: ErrorKind::PermissionDenied,
                source: eyre!("File extension is protected"),
            });
        }
        let path = if path.exists() {
            // add a postfix to the file name
            let mut postfix = 1;
            // get the file name without the extension
            let file_name = path.file_stem().unwrap().to_str().unwrap().to_string();
            loop {
                let new_path = path.with_file_name(format!(
                    "{}_{}.{}",
                    file_name,
                    postfix,
                    path.extension().unwrap().to_str().unwrap()
                ));
                if !new_path.exists() {
                    break new_path;
                }
                postfix += 1;
            }
        } else {
            path
        };
        let mut file = crate::util::fs::create(&path).await?;

        let threshold = total.unwrap_or(500000.0) / 100.0;

        let mut elapsed_bytes = 0_u64;
        let mut last_progression = 0_u64;

        while let Some(chunk) = field.chunk().await.map_err(|e| {
            std::fs::remove_file(&path).ok();
            state.event_broadcaster.send(Event {
                event_inner: EventInner::ProgressionEvent(ProgressionEvent {
                    event_id,
                    progression_event_inner: ProgressionEventInner::ProgressionEnd {
                        success: false,
                        message: Some(e.to_string()),
                        inner: None,
                    },
                }),
                details: "".to_string(),
                snowflake: Snowflake::default(),
                caused_by: CausedBy::User {
                    user_id: requester.uid.clone(),
                    user_name: requester.username.clone(),
                },
            });
            Err::<(), axum::extract::multipart::MultipartError>(e)
                .context("Failed to read chunk")
                .unwrap_err()
        })? {
            elapsed_bytes += chunk.len() as u64;
            let progression = (elapsed_bytes as f64 / threshold).floor() as u64;
            if progression > last_progression {
                last_progression = progression;
                state.event_broadcaster.send(Event {
                    event_inner: EventInner::ProgressionEvent(ProgressionEvent {
                        event_id,
                        progression_event_inner: ProgressionEventInner::ProgressionUpdate {
                            progress_message: if let Some(total) = total {
                                format!(
                                    "Uploading {name}, {}",
                                    format_byte_download(elapsed_bytes, total as u64)
                                )
                            } else {
                                format!("Uploading {name}, {} uploaded", format_byte(elapsed_bytes))
                            },
                            progress: threshold,
                        },
                    }),
                    details: "".to_string(),
                    snowflake: Snowflake::default(),
                    caused_by: CausedBy::User {
                        user_id: requester.uid.clone(),
                        user_name: requester.username.clone(),
                    },
                });
            }
            file.write_all(&chunk).await.map_err(|e| {
                std::fs::remove_file(&path).ok();
                state.event_broadcaster.send(Event {
                    event_inner: EventInner::ProgressionEvent(ProgressionEvent {
                        event_id,
                        progression_event_inner: ProgressionEventInner::ProgressionEnd {
                            success: false,
                            message: Some(e.to_string()),
                            inner: None,
                        },
                    }),
                    details: "".to_string(),
                    snowflake: Snowflake::default(),
                    caused_by: CausedBy::User {
                        user_id: requester.uid.clone(),
                        user_name: requester.username.clone(),
                    },
                });
                Err::<(), std::io::Error>(e)
                    .context("Failed to write chunk")
                    .unwrap_err()
            })?;
        }

        let caused_by = CausedBy::User {
            user_id: requester.uid.clone(),
            user_name: requester.username.clone(),
        };
        state.event_broadcaster.send(new_fs_event(
            FSOperation::Upload,
            FSTarget::File(path),
            caused_by,
        ));
    }
    state.event_broadcaster.send(Event {
        event_inner: EventInner::ProgressionEvent(ProgressionEvent {
            event_id,
            progression_event_inner: ProgressionEventInner::ProgressionEnd {
                success: true,
                message: Some("Upload complete".to_string()),
                inner: None,
            },
        }),
        details: "".to_string(),
        snowflake: Snowflake::default(),
        caused_by: CausedBy::User {
            user_id: requester.uid.clone(),
            user_name: requester.username.clone(),
        },
    });
    Ok(Json(()))
}

pub async fn unzip_instance_file(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path((uuid, base64_relative_path)): Path<(InstanceUuid, String)>,
    AuthBearer(token): AuthBearer,
    Json(unzip_option): Json<UnzipOption>,
) -> Result<Json<()>, Error> {
    let relative_path = decode_base64(&base64_relative_path)?;
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;
    requester.try_action(&UserAction::WriteInstanceFile(uuid.clone()))?;
    let instances = state.instances.lock().await;
    let instance = instances.get(&uuid).ok_or_else(|| Error {
        kind: ErrorKind::NotFound,
        source: eyre!("Instance not found"),
    })?;
    let root = instance.path().await;
    drop(instances);
    let path_to_zip_file = scoped_join_win_safe(root, &relative_path)?;

    if let UnzipOption::ToDir(ref dir) = unzip_option {
        if !requester.can_perform_action(&UserAction::WriteGlobalFile) && is_path_protected(dir) {
            return Err(Error {
                kind: ErrorKind::PermissionDenied,
                source: eyre!("Destination is protected"),
            });
        }
    }
    let event_broadcaster = state.event_broadcaster.clone();
    tokio::spawn(async move {
        let event_id = Snowflake::default();
        let caused_by = CausedBy::User {
            user_id: requester.uid.clone(),
            user_name: requester.username.clone(),
        };

        event_broadcaster.send(Event {
            event_inner: EventInner::ProgressionEvent(ProgressionEvent {
                event_id,
                progression_event_inner: ProgressionEventInner::ProgressionStart {
                    progression_name: format!("Unzipping {}", relative_path),
                    producer_id: None,
                    total: None,
                    inner: None,
                },
            }),
            details: "".to_string(),
            snowflake: Snowflake::default(),
            caused_by: CausedBy::User {
                user_id: requester.uid.clone(),
                user_name: requester.username.clone(),
            },
        });

        if let Err(e) = unzip_file_async(path_to_zip_file, unzip_option).await {
            event_broadcaster.send(Event {
                event_inner: EventInner::ProgressionEvent(ProgressionEvent {
                    event_id,
                    progression_event_inner: ProgressionEventInner::ProgressionEnd {
                        success: true,
                        message: Some(format!("Unzip failed: {}", e)),
                        inner: Some(ProgressionEndValue::FSOperationCompleted {
                            instance_uuid: uuid,
                            success: false,
                            message: format!("Unzipping {} failed : {e}", relative_path),
                        }),
                    },
                }),
                details: "".to_string(),
                snowflake: Snowflake::default(),
                caused_by,
            });
        } else {
            event_broadcaster.send(Event {
                event_inner: EventInner::ProgressionEvent(ProgressionEvent {
                    event_id,
                    progression_event_inner: ProgressionEventInner::ProgressionEnd {
                        success: true,
                        message: Some("Unzip complete".to_string()),
                        inner: Some(ProgressionEndValue::FSOperationCompleted {
                            instance_uuid: uuid,
                            success: true,
                            message: format!("Unzipping {} complete", relative_path),
                        }),
                    },
                }),
                details: "".to_string(),
                snowflake: Snowflake::default(),
                caused_by,
            });
        }
    });

    Ok(Json(()))
}

#[derive(Deserialize)]
struct ZipRequest {
    target_relative_paths: Vec<PathBuf>,
    destination_relative_path: PathBuf,
}

async fn zip_instance_files(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(uuid): Path<InstanceUuid>,
    AuthBearer(token): AuthBearer,
    Json(zip_request): Json<ZipRequest>,
) -> Result<Json<PathBuf>, Error> {
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;
    requester.try_action(&UserAction::WriteInstanceFile(uuid.clone()))?;
    let instances = state.instances.lock().await;
    let instance = instances.get(&uuid).ok_or_else(|| Error {
        kind: ErrorKind::NotFound,
        source: eyre!("Instance not found"),
    })?;
    let root = instance.path().await;
    drop(instances);
    let ZipRequest {
        mut target_relative_paths,
        mut destination_relative_path,
    } = zip_request;

    // apply scoped_join_win_safe to all paths
    for path in &mut target_relative_paths {
        *path = scoped_join_win_safe(&root, &*path)?;
    }
    destination_relative_path = scoped_join_win_safe(&root, &destination_relative_path)?;

    if !requester.can_perform_action(&UserAction::ReadGlobalFile)
        && is_path_protected(&destination_relative_path)
    {
        return Err(Error {
            kind: ErrorKind::PermissionDenied,
            source: eyre!("Destination is protected"),
        });
    }

    let ret = zip_files_async(&target_relative_paths, destination_relative_path).await?;
    // remove root from path
    let ret = ret.strip_prefix(&root).unwrap().to_path_buf();

    Ok(Json(ret))
}

pub fn get_instance_fs_routes(state: AppState) -> Router {
    Router::new()
        .route(
            "/instance/:uuid/fs/:base64_relative_path/ls",
            get(list_instance_files),
        )
        .route(
            "/instance/:uuid/fs/:base64_relative_path/read",
            get(read_instance_file),
        )
        .route(
            "/instance/:uuid/fs/:base64_relative_path/write",
            put(write_instance_file),
        )
        .route(
            "/instance/:uuid/fs/:base64_relative_path/mkdir",
            put(make_instance_directory),
        )
        .route("/instance/:uuid/fs/cpr", put(copy_instance_files))
        .route(
            "/instance/:uuid/fs/:base64_relative_path/move/:base64_relative_path_dest",
            put(move_instance_file),
        )
        .route(
            "/instance/:uuid/fs/:base64_relative_path/rm",
            delete(remove_instance_file),
        )
        .route(
            "/instance/:uuid/fs/:base64_relative_path/rmdir",
            delete(remove_instance_dir),
        )
        .route(
            "/instance/:uuid/fs/:base64_relative_path/new",
            put(new_instance_file),
        )
        .route(
            "/instance/:uuid/fs/:base64_relative_path/download",
            get(download_instance_file),
        )
        .route(
            "/instance/:uuid/fs/:base64_relative_path/upload",
            put(upload_instance_file),
        )
        .layer(DefaultBodyLimit::disable())
        .route(
            "/instance/:uuid/fs/:base64_relative_path/unzip",
            put(unzip_instance_file),
        )
        .route("/instance/:uuid/fs/zip", put(zip_instance_files))
        .with_state(state)
}