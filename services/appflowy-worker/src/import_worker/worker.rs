use crate::error::ImportError;
use crate::import_worker::unzip::unzip_async;
use crate::s3_client::S3StreamResponse;
use anyhow::anyhow;
use async_zip::base::read::stream::ZipFileReader;
use aws_sdk_s3::primitives::ByteStream;
use bytes::Bytes;
use collab::core::origin::CollabOrigin;
use collab::entity::EncodedCollab;

use collab_database::workspace_database::WorkspaceDatabaseBody;

use collab_entity::CollabType;
use collab_folder::Folder;
use collab_importer::imported_collab::ImportType;
use collab_importer::notion::page::CollabResource;
use collab_importer::notion::NotionImporter;
use collab_importer::util::FileId;
use database::collab::{insert_into_af_collab_bulk_for_user, select_blob_from_af_collab};
use futures::stream::FuturesUnordered;
use futures::{stream, StreamExt};
use redis::aio::ConnectionManager;
use redis::streams::{
  StreamClaimOptions, StreamClaimReply, StreamId, StreamPendingReply, StreamReadOptions,
  StreamReadReply,
};
use redis::{AsyncCommands, RedisResult, Value};
use serde::{Deserialize, Serialize};
use serde_json::from_str;
use sqlx::types::chrono;
use sqlx::{PgPool, Pool, Postgres};
use std::collections::HashMap;
use std::env::temp_dir;
use std::fs::Permissions;
use std::ops::DerefMut;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;

use crate::import_worker::report::{ImportNotifier, ImportProgress, ImportResultBuilder};
use database::workspace::{
  select_workspace_database_storage_id, update_import_task_status, update_workspace_status,
};
use database_entity::dto::CollabParams;

use crate::s3_client::S3Client;
use database::collab::mem_cache::{cache_exp_secs_from_collab_type, CollabMemCache};
use tokio::task::spawn_local;
use tokio::time::interval;
use tracing::{error, info, trace, warn};
use uuid::Uuid;

const GROUP_NAME: &str = "import_task_group";
const CONSUMER_NAME: &str = "appflowy_worker";
pub async fn run_import_worker(
  pg_pool: PgPool,
  mut redis_client: ConnectionManager,
  s3_client: Arc<dyn S3Client>,
  notifier: Arc<dyn ImportNotifier>,
  stream_name: &str,
  tick_interval_secs: u64,
) -> Result<(), ImportError> {
  info!("Starting importer worker");
  if let Err(err) = ensure_consumer_group(stream_name, GROUP_NAME, &mut redis_client)
    .await
    .map_err(ImportError::Internal)
  {
    error!("Failed to ensure consumer group: {:?}", err);
  }

  process_un_acked_tasks(
    &mut redis_client,
    &s3_client,
    &pg_pool,
    stream_name,
    GROUP_NAME,
    CONSUMER_NAME,
    notifier.clone(),
  )
  .await;

  process_upcoming_tasks(
    &mut redis_client,
    &s3_client,
    pg_pool,
    stream_name,
    GROUP_NAME,
    CONSUMER_NAME,
    notifier.clone(),
    tick_interval_secs,
  )
  .await?;

  Ok(())
}

async fn process_un_acked_tasks(
  redis_client: &mut ConnectionManager,
  s3_client: &Arc<dyn S3Client>,
  pg_pool: &PgPool,
  stream_name: &str,
  group_name: &str,
  consumer_name: &str,
  notifier: Arc<dyn ImportNotifier>,
) {
  // when server restarts, we need to check if there are any unacknowledged tasks
  match get_un_ack_tasks(stream_name, group_name, consumer_name, redis_client).await {
    Ok(un_ack_tasks) => {
      info!("Found {} unacknowledged tasks", un_ack_tasks.len());
      for un_ack_task in un_ack_tasks {
        // Ignore the error here since the consume task will handle the error
        let _ = consume_task(
          stream_name,
          group_name,
          un_ack_task.task,
          &un_ack_task.stream_id.id,
          redis_client,
          s3_client,
          pg_pool,
          notifier.clone(),
        )
        .await;
      }
    },
    Err(err) => error!("Failed to get unacknowledged tasks: {:?}", err),
  }
}

#[allow(clippy::too_many_arguments)]
async fn process_upcoming_tasks(
  redis_client: &mut ConnectionManager,
  s3_client: &Arc<dyn S3Client>,
  pg_pool: PgPool,
  stream_name: &str,
  group_name: &str,
  consumer_name: &str,
  notifier: Arc<dyn ImportNotifier>,
  interval_secs: u64,
) -> Result<(), ImportError> {
  let options = StreamReadOptions::default()
    .group(group_name, consumer_name)
    .count(3);
  let mut interval = interval(Duration::from_secs(interval_secs));
  interval.tick().await;

  loop {
    interval.tick().await;
    let tasks: StreamReadReply = match redis_client
      .xread_options(&[stream_name], &[">"], &options)
      .await
    {
      Ok(tasks) => tasks,
      Err(err) => {
        error!("Failed to read tasks from Redis stream: {:?}", err);
        continue;
      },
    };

    let mut task_handlers = FuturesUnordered::new();
    for stream_key in tasks.keys {
      // For each stream key, iterate through the stream entries
      for stream_id in stream_key.ids {
        match ImportTask::try_from(&stream_id) {
          Ok(import_task) => {
            let entry_id = stream_id.id.clone();
            let mut cloned_redis_client = redis_client.clone();
            let cloned_s3_client = s3_client.clone();
            let pg_pool = pg_pool.clone();
            let notifier = notifier.clone();
            let stream_name = stream_name.to_string();
            let group_name = group_name.to_string();
            task_handlers.push(spawn_local(async move {
              consume_task(
                &stream_name,
                &group_name,
                import_task,
                &entry_id,
                &mut cloned_redis_client,
                &cloned_s3_client,
                &pg_pool,
                notifier,
              )
              .await?;
              Ok::<(), ImportError>(())
            }));
          },
          Err(err) => {
            error!("Failed to deserialize task: {:?}", err);
          },
        }
      }
    }

    while let Some(result) = task_handlers.next().await {
      match result {
        Ok(Ok(())) => trace!("Task completed successfully"),
        Ok(Err(e)) => error!("Task failed: {:?}", e),
        Err(e) => error!("Runtime error: {:?}", e),
      }
    }
  }
}

#[allow(clippy::too_many_arguments)]
async fn consume_task(
  stream_name: &str,
  group_name: &str,
  import_task: ImportTask,
  entry_id: &String,
  redis_client: &mut ConnectionManager,
  s3_client: &Arc<dyn S3Client>,
  pg_pool: &Pool<Postgres>,
  notifier: Arc<dyn ImportNotifier>,
) -> Result<(), ImportError> {
  process_task(import_task, s3_client, redis_client, pg_pool, notifier).await?;
  let _: () = redis_client
    .xack(stream_name, group_name, &[entry_id])
    .await
    .map_err(|e| {
      error!("Failed to acknowledge task: {:?}", e);
      ImportError::Internal(e.into())
    })?;
  Ok::<_, ImportError>(())
}

async fn process_task(
  import_task: ImportTask,
  s3_client: &Arc<dyn S3Client>,
  redis_client: &mut ConnectionManager,
  pg_pool: &PgPool,
  notifier: Arc<dyn ImportNotifier>,
) -> Result<(), ImportError> {
  trace!("Processing task: {:?}", import_task);
  match import_task {
    ImportTask::Notion(task) => {
      // 1. unzip file to temp dir
      let unzip_dir_path = download_zip_file(&task, s3_client).await?;
      // 2. import zip
      let result =
        process_unzip_file(&task, &unzip_dir_path, pg_pool, redis_client, s3_client).await;
      // 3. delete zip file regardless of success or failure
      match fs::remove_dir_all(unzip_dir_path).await {
        Ok(_) => trace!("[Import]: {} deleted unzip file", task.workspace_id),
        Err(err) => error!("Failed to delete unzip file: {:?}", err),
      }
      // 4. notify import result
      trace!(
        "[Import]: {}:{} import result: {:?}",
        task.workspace_id,
        task.task_id,
        result
      );
      notify_user(&task, result, notifier).await?;
      // 5. remove file from S3
      if let Err(err) = s3_client.delete_blob(task.s3_key.as_str()).await {
        error!("Failed to delete zip file from S3: {:?}", err);
      }
      Ok(())
    },
    ImportTask::Custom(value) => {
      trace!("Custom task: {:?}", value);
      match value.get("workspace_id").and_then(|v| v.as_str()) {
        None => {
          warn!("Missing workspace_id in custom task");
        },
        Some(workspace_id) => {
          let result = ImportResultBuilder::new(workspace_id.to_string()).build();
          notifier
            .notify_progress(ImportProgress::Finished(result))
            .await;
        },
      }
      Ok(())
    },
  }
}

async fn download_zip_file(
  import_task: &NotionImportTask,
  s3_client: &Arc<dyn S3Client>,
) -> Result<PathBuf, ImportError> {
  let S3StreamResponse {
    stream,
    content_type: _,
  } = s3_client
    .get_blob(import_task.s3_key.as_str())
    .await
    .map_err(|err| ImportError::Internal(err.into()))?;

  let zip_reader = ZipFileReader::new(stream);
  let unique_file_name = uuid::Uuid::new_v4().to_string();
  let output_file_path = temp_dir().join(unique_file_name);
  fs::create_dir_all(&output_file_path)
    .await
    .map_err(|err| ImportError::Internal(err.into()))?;

  fs::set_permissions(&output_file_path, Permissions::from_mode(0o777))
    .await
    .map_err(|err| {
      ImportError::Internal(anyhow!("Failed to set permissions for temp dir: {:?}", err))
    })?;

  let unzip_file = unzip_async(zip_reader, output_file_path)
    .await
    .map_err(ImportError::Internal)?;
  Ok(unzip_file.unzip_dir_path)
}

async fn process_unzip_file(
  import_task: &NotionImportTask,
  unzip_dir_path: &PathBuf,
  pg_pool: &PgPool,
  redis_client: &mut ConnectionManager,
  s3_client: &Arc<dyn S3Client>,
) -> Result<(), ImportError> {
  let notion_importer = NotionImporter::new(
    unzip_dir_path,
    import_task.workspace_id.clone(),
    import_task.host.clone(),
  )
  .map_err(ImportError::ImportCollabError)?;

  let imported = notion_importer
    .import()
    .await
    .map_err(ImportError::ImportCollabError)?;
  let nested_views = imported.build_nested_views(import_task.uid).await;
  trace!(
    "[Import]: {} imported nested views:{}",
    import_task.workspace_id,
    nested_views
  );

  // 1. Open the workspace folder
  let folder_collab =
    get_encode_collab_from_bytes(&imported.workspace_id, &CollabType::Folder, pg_pool).await?;
  let mut folder = Folder::from_collab_doc_state(
    import_task.uid,
    CollabOrigin::Server,
    folder_collab.into(),
    &imported.workspace_id,
    vec![],
  )
  .map_err(|err| ImportError::CannotOpenWorkspace(err.to_string()))?;

  // 2. Insert collabs' views into the folder
  trace!(
    "[Import]: {} insert views:{} to folder",
    import_task.workspace_id,
    nested_views.len()
  );
  folder.insert_nested_views(nested_views.into_inner());

  let mut resources = vec![];
  let mut collab_params_list = vec![];
  let mut database_view_ids_by_database_id: HashMap<String, Vec<String>> = HashMap::new();
  let mem_cache = CollabMemCache::new(redis_client.clone());
  let timestamp = chrono::Utc::now().timestamp();

  // 3. Collect all collabs and resources
  let mut stream = imported.into_collab_stream().await;
  while let Some(imported_collab) = stream.next().await {
    trace!(
      "[Import]: {} imported collab: {}",
      import_task.workspace_id,
      imported_collab
    );
    resources.push(imported_collab.resource);
    collab_params_list.extend(
      imported_collab
        .collabs
        .into_iter()
        .map(|imported_collab| CollabParams {
          object_id: imported_collab.object_id,
          collab_type: imported_collab.collab_type,
          embeddings: None,
          encoded_collab_v1: Bytes::from(imported_collab.encoded_collab.encode_to_bytes().unwrap()),
        })
        .collect::<Vec<_>>(),
    );

    match imported_collab.import_type {
      ImportType::Database {
        database_id,
        view_ids,
      } => {
        database_view_ids_by_database_id.insert(database_id, view_ids);
      },
      ImportType::Document => {
        // do nothing
      },
    }
  }

  let w_database_id = select_workspace_database_storage_id(pg_pool, &import_task.workspace_id)
    .await
    .map_err(|err| {
      ImportError::Internal(anyhow!(
        "Failed to select workspace database storage id: {:?}",
        err
      ))
    })
    .map(|id| id.to_string())?;

  // 4. Edit workspace database collab and then encode workspace database collab
  if !database_view_ids_by_database_id.is_empty() {
    let w_db_collab =
      get_encode_collab_from_bytes(&w_database_id, &CollabType::WorkspaceDatabase, pg_pool).await?;
    let mut w_database = WorkspaceDatabaseBody::from_collab_doc_state(
      &w_database_id,
      CollabOrigin::Server,
      w_db_collab.into(),
    )
    .map_err(|err| ImportError::CannotOpenWorkspace(err.to_string()))?;
    w_database.batch_add_database(database_view_ids_by_database_id);

    let w_database_collab = w_database.encode_collab_v1().map_err(|err| {
      ImportError::Internal(anyhow!(
        "Failed to encode workspace database collab: {:?}",
        err
      ))
    })?;
    // Update the workspace database cache because newly created workspace databases are cached in Redis.
    mem_cache
      .insert_encode_collab(
        &w_database_id,
        w_database_collab.clone(),
        timestamp,
        cache_exp_secs_from_collab_type(&CollabType::WorkspaceDatabase),
      )
      .await;

    trace!(
      "[Import]: {} did encode workspace database collab",
      import_task.workspace_id
    );
    let w_database_collab_params = CollabParams {
      object_id: w_database_id.clone(),
      collab_type: CollabType::WorkspaceDatabase,
      embeddings: None,
      encoded_collab_v1: Bytes::from(w_database_collab.encode_to_bytes().unwrap()),
    };
    collab_params_list.push(w_database_collab_params);
  }

  // 5. Encode Folder
  let folder_collab = folder
    .encode_collab_v1(|collab| CollabType::Folder.validate_require_data(collab))
    .map_err(|err| ImportError::Internal(err.into()))?;

  // Update the folder cache because newly created folders are cached in Redis.
  // Other collaboration objects do not use caching yet, so there is no need to insert them into Redis.
  mem_cache
    .insert_encode_collab(
      &import_task.workspace_id,
      folder_collab.clone(),
      timestamp,
      cache_exp_secs_from_collab_type(&CollabType::Folder),
    )
    .await;

  let folder_collab_params = CollabParams {
    object_id: import_task.workspace_id.clone(),
    collab_type: CollabType::Folder,
    embeddings: None,
    encoded_collab_v1: Bytes::from(folder_collab.encode_to_bytes().unwrap()),
  };
  trace!(
    "[Import]: {} did encode folder collab",
    import_task.workspace_id
  );
  collab_params_list.push(folder_collab_params);

  // 6. Start a transaction to insert all collabs
  let mut transaction = pg_pool.begin().await.map_err(|err| {
    ImportError::Internal(anyhow!(
      "Failed to start transaction when importing data: {:?}",
      err
    ))
  })?;

  trace!(
    "[Import]: {} insert collabs into database",
    import_task.workspace_id
  );

  // 7. write all collab to disk
  insert_into_af_collab_bulk_for_user(
    &mut transaction,
    &import_task.uid,
    &import_task.workspace_id,
    &collab_params_list,
  )
  .await
  .map_err(|err| {
    ImportError::Internal(anyhow!(
      "Failed to insert collabs into database when importing data: {:?}",
      err
    ))
  })?;

  trace!(
    "[Import]: {} update task:{} status to completed",
    import_task.workspace_id,
    import_task.task_id,
  );
  update_import_task_status(&import_task.task_id, 1, transaction.deref_mut())
    .await
    .map_err(|err| {
      ImportError::Internal(anyhow!(
        "Failed to update import task status when importing data: {:?}",
        err
      ))
    })?;

  trace!(
    "[Import]: {} set is_initialized to true",
    import_task.workspace_id,
  );
  update_workspace_status(transaction.deref_mut(), &import_task.workspace_id, true)
    .await
    .map_err(|err| {
      ImportError::Internal(anyhow!(
        "Failed to update workspace status when importing data: {:?}",
        err
      ))
    })?;

  let result = transaction.commit().await.map_err(|err| {
    ImportError::Internal(anyhow!(
      "Failed to commit transaction when importing data: {:?}",
      err
    ))
  });

  if result.is_err() {
    // remove cache in redis
    let _ = mem_cache.remove_encode_collab(&w_database_id).await;
    let _ = mem_cache
      .remove_encode_collab(&import_task.workspace_id)
      .await;

    return result;
  }

  // 7. after inserting all collabs, upload all files to S3
  trace!("[Import]: {} upload files to s3", import_task.workspace_id,);
  batch_upload_files_to_s3(&import_task.workspace_id, s3_client, resources)
    .await
    .map_err(|err| ImportError::Internal(anyhow!("Failed to upload files to S3: {:?}", err)))?;

  Ok(())
}

async fn notify_user(
  _import_task: &NotionImportTask,
  _result: Result<(), ImportError>,
  _notifier: Arc<dyn ImportNotifier>,
) -> Result<(), ImportError> {
  // send email
  Ok(())
}

pub async fn batch_upload_files_to_s3(
  workspace_id: &str,
  client: &Arc<dyn S3Client>,
  collab_resources: Vec<CollabResource>,
) -> Result<(), anyhow::Error> {
  // Flatten the collab_resources into an iterator of (workspace_id, object_id, file_path)
  let file_tasks = collab_resources
    .into_iter()
    .flat_map(|resource| {
      let object_id = resource.object_id;
      resource
        .files
        .into_iter()
        .map(move |file| (object_id.clone(), file))
    })
    .collect::<Vec<(String, String)>>();

  // Create a stream of upload tasks
  let upload_stream = stream::iter(file_tasks.into_iter().map(
    |(object_id, file_path)| async move {
      match upload_file_to_s3(client, workspace_id, &object_id, &file_path).await {
        Ok(_) => {
          trace!("Successfully uploaded: {}", file_path);
          Ok(())
        },
        Err(e) => {
          error!("Failed to upload {}: {:?}", file_path, e);
          Err(e)
        },
      }
    },
  ))
  .buffer_unordered(5);
  let results: Vec<_> = upload_stream.collect().await;
  let errors: Vec<_> = results.into_iter().filter_map(Result::err).collect();
  if errors.is_empty() {
    Ok(())
  } else {
    Err(anyhow!("Some uploads failed: {:?}", errors))
  }
}

async fn upload_file_to_s3(
  client: &Arc<dyn S3Client>,
  workspace_id: &str,
  object_id: &str,
  file_path: &str,
) -> Result<(), anyhow::Error> {
  let path = Path::new(file_path);
  if !path.exists() {
    return Err(anyhow!("File does not exist: {:?}", path));
  }
  let file_id = FileId::from_path(&path.to_path_buf()).await?;
  let mime_type = mime_guess::from_path(file_path).first_or_octet_stream();
  let object_key = format!("{}/{}/{}", workspace_id, object_id, file_id);
  let byte_stream = ByteStream::from_path(path).await?;
  client
    .put_blob(&object_key, byte_stream, Some(mime_type.as_ref()))
    .await?;
  Ok(())
}

async fn get_encode_collab_from_bytes(
  object_id: &str,
  collab_type: &CollabType,
  pg_pool: &PgPool,
) -> Result<EncodedCollab, ImportError> {
  let bytes = select_blob_from_af_collab(pg_pool, collab_type, object_id)
    .await
    .map_err(|err| ImportError::Internal(err.into()))?;
  tokio::task::spawn_blocking(move || match EncodedCollab::decode_from_bytes(&bytes) {
    Ok(encoded_collab) => Ok(encoded_collab),
    Err(err) => Err(ImportError::Internal(anyhow!(
      "Failed to decode collab from bytes: {:?}",
      err
    ))),
  })
  .await
  .map_err(|err| ImportError::Internal(err.into()))?
}

/// Ensure the consumer group exists, if not, create it.
async fn ensure_consumer_group(
  stream_key: &str,
  group_name: &str,
  redis_client: &mut ConnectionManager,
) -> Result<(), anyhow::Error> {
  let result: RedisResult<()> = redis_client
    .xgroup_create_mkstream(stream_key, group_name, "0")
    .await;

  if let Err(redis_error) = result {
    if let Some(code) = redis_error.code() {
      if code == "BUSYGROUP" {
        return Ok(()); // Group already exists, considered as success.
      }
    }
    error!("Error when creating consumer group: {:?}", redis_error);
    return Err(redis_error.into());
  }

  Ok(())
}

struct UnAckTask {
  stream_id: StreamId,
  task: ImportTask,
}

async fn get_un_ack_tasks(
  stream_key: &str,
  group_name: &str,
  consumer_name: &str,
  redis_client: &mut ConnectionManager,
) -> Result<Vec<UnAckTask>, anyhow::Error> {
  let reply: StreamPendingReply = redis_client.xpending(stream_key, group_name).await?;
  match reply {
    StreamPendingReply::Empty => Ok(vec![]),
    StreamPendingReply::Data(pending) => {
      let opts = StreamClaimOptions::default()
        .idle(500)
        .with_force()
        .retry(2);

      // If the start_id and end_id are the same, we only need to claim one message.
      let mut ids = Vec::with_capacity(2);
      ids.push(pending.start_id.clone());
      if pending.start_id != pending.end_id {
        ids.push(pending.end_id);
      }

      let result: StreamClaimReply = redis_client
        .xclaim_options(stream_key, group_name, consumer_name, 500, &ids, opts)
        .await?;

      let tasks = result
        .ids
        .into_iter()
        .filter_map(|stream_id| {
          ImportTask::try_from(&stream_id)
            .map(|task| UnAckTask { stream_id, task })
            .ok()
        })
        .collect::<Vec<_>>();

      trace!("Claimed tasks: {}", tasks.len());
      Ok(tasks)
    },
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotionImportTask {
  pub uid: i64,
  pub task_id: Uuid,
  pub user_uuid: String,
  pub workspace_id: String,
  pub s3_key: String,
  pub host: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ImportTask {
  Notion(NotionImportTask),
  Custom(serde_json::Value),
}

impl TryFrom<&StreamId> for ImportTask {
  type Error = ImportError;

  fn try_from(stream_id: &StreamId) -> Result<Self, Self::Error> {
    let task_str = match stream_id.map.get("task") {
      Some(value) => match value {
        Value::Data(data) => String::from_utf8_lossy(data).to_string(),
        _ => {
          error!("Unexpected value type for task field: {:?}", value);
          return Err(ImportError::Internal(anyhow!(
            "Unexpected value type for task field: {:?}",
            value
          )));
        },
      },
      None => {
        error!("Task field not found in Redis stream entry");
        return Err(ImportError::Internal(anyhow!(
          "Task field not found in Redis stream entry"
        )));
      },
    };

    from_str::<ImportTask>(&task_str).map_err(|err| ImportError::Internal(err.into()))
  }
}
