use crate::{
  entities::{
    app::{AppPB, CreateAppParams, *},
    trash::TrashType,
  },
  errors::*,
  event_map::{FolderCouldServiceV1, WorkspaceUser},
  notification::*,
  services::{
    persistence::{AppChangeset, FolderPersistence, FolderPersistenceTransaction},
    TrashController, TrashEvent,
  },
};

use folder_model::AppRevision;
use futures::{FutureExt, StreamExt};
use std::{collections::HashSet, sync::Arc};

pub(crate) struct AppController {
  user: Arc<dyn WorkspaceUser>,
  persistence: Arc<FolderPersistence>,
  trash_controller: Arc<TrashController>,
  cloud_service: Arc<dyn FolderCouldServiceV1>,
}

impl AppController {
  pub(crate) fn new(
    user: Arc<dyn WorkspaceUser>,
    persistence: Arc<FolderPersistence>,
    trash_can: Arc<TrashController>,
    cloud_service: Arc<dyn FolderCouldServiceV1>,
  ) -> Self {
    Self {
      user,
      persistence,
      trash_controller: trash_can,
      cloud_service,
    }
  }

  pub fn initialize(&self) -> Result<(), FlowyError> {
    self.listen_trash_controller_event();
    Ok(())
  }

  #[tracing::instrument(level = "debug", skip(self, params), fields(name = %params.name) err)]
  pub(crate) async fn create_app_from_params(
    &self,
    params: CreateAppParams,
  ) -> Result<AppPB, FlowyError> {
    let app = self.create_app_on_server(params).await?;
    self.create_app_on_local(app).await
  }

  pub(crate) async fn create_app_on_local(&self, app: AppRevision) -> Result<AppPB, FlowyError> {
    self
      .persistence
      .begin_transaction(|transaction| {
        transaction.create_app(app.clone())?;
        notify_apps_changed(
          &app.workspace_id,
          self.trash_controller.clone(),
          &transaction,
        )?;
        Ok(())
      })
      .await?;
    Ok(app.into())
  }

  pub(crate) async fn read_app(&self, params: AppIdPB) -> Result<Option<AppRevision>, FlowyError> {
    let app = self
      .persistence
      .begin_transaction(|transaction| {
        let app = transaction.read_app(&params.value)?;
        let trash_ids = self.trash_controller.read_trash_ids(&transaction)?;
        if trash_ids.contains(&app.id) {
          return Ok(None);
        }
        Ok(Some(app))
      })
      .await?;
    Ok(app)
  }

  pub(crate) async fn update_app(&self, params: UpdateAppParams) -> Result<(), FlowyError> {
    let changeset = AppChangeset::new(params.clone());
    let app_id = changeset.id.clone();

    let app: AppPB = self
      .persistence
      .begin_transaction(|transaction| {
        transaction.update_app(changeset)?;
        let app = transaction.read_app(&app_id)?;
        Ok(app)
      })
      .await?
      .into();
    send_notification(&app_id, FolderNotification::DidUpdateApp)
      .payload(app)
      .send();
    self.update_app_on_server(params)?;
    Ok(())
  }

  pub(crate) async fn move_app(&self, app_id: &str, from: usize, to: usize) -> FlowyResult<()> {
    self
      .persistence
      .begin_transaction(|transaction| {
        transaction.move_app(app_id, from, to)?;
        let app = transaction.read_app(app_id)?;
        notify_apps_changed(
          &app.workspace_id,
          self.trash_controller.clone(),
          &transaction,
        )?;
        Ok(())
      })
      .await?;
    Ok(())
  }

  pub(crate) async fn read_local_apps(
    &self,
    ids: Vec<String>,
  ) -> Result<Vec<AppRevision>, FlowyError> {
    let app_revs = self
      .persistence
      .begin_transaction(|transaction| {
        let mut apps = vec![];
        for id in ids {
          apps.push(transaction.read_app(&id)?);
        }
        Ok(apps)
      })
      .await?;
    Ok(app_revs)
  }
}

impl AppController {
  #[tracing::instrument(level = "trace", skip(self), err)]
  async fn create_app_on_server(&self, params: CreateAppParams) -> Result<AppRevision, FlowyError> {
    let token = self.user.token()?;
    let app = self.cloud_service.create_app(&token, params).await?;
    Ok(app)
  }

  #[tracing::instrument(level = "trace", skip(self), err)]
  fn update_app_on_server(&self, params: UpdateAppParams) -> Result<(), FlowyError> {
    let token = self.user.token()?;
    let server = self.cloud_service.clone();
    tokio::spawn(async move {
      match server.update_app(&token, params).await {
        Ok(_) => {},
        Err(e) => {
          // TODO: retry?
          log::error!("Update app failed: {:?}", e);
        },
      }
    });
    Ok(())
  }

  fn listen_trash_controller_event(&self) {
    let mut rx = self.trash_controller.subscribe();
    let persistence = self.persistence.clone();
    let trash_controller = self.trash_controller.clone();
    let _ = tokio::spawn(async move {
      loop {
        let mut stream = Box::pin(rx.recv().into_stream().filter_map(|result| async move {
          match result {
            Ok(event) => event.select(TrashType::TrashApp),
            Err(_e) => None,
          }
        }));
        if let Some(event) = stream.next().await {
          handle_trash_event(persistence.clone(), trash_controller.clone(), event).await
        }
      }
    });
  }
}

#[tracing::instrument(level = "trace", skip(persistence, trash_controller))]
async fn handle_trash_event(
  persistence: Arc<FolderPersistence>,
  trash_controller: Arc<TrashController>,
  event: TrashEvent,
) {
  match event {
    TrashEvent::NewTrash(identifiers, ret) | TrashEvent::Putback(identifiers, ret) => {
      let result = persistence
        .begin_transaction(|transaction| {
          for identifier in identifiers.items {
            let app = transaction.read_app(&identifier.id)?;
            notify_apps_changed(&app.workspace_id, trash_controller.clone(), &transaction)?;
          }
          Ok(())
        })
        .await;
      let _ = ret.send(result).await;
    },
    TrashEvent::Delete(identifiers, ret) => {
      let result = persistence
        .begin_transaction(|transaction| {
          let mut notify_ids = HashSet::new();
          for identifier in identifiers.items {
            let app = transaction.read_app(&identifier.id)?;
            let _ = transaction.delete_app(&identifier.id)?;
            notify_ids.insert(app.workspace_id);
          }

          for notify_id in notify_ids {
            notify_apps_changed(&notify_id, trash_controller.clone(), &transaction)?;
          }
          Ok(())
        })
        .await;
      let _ = ret.send(result).await;
    },
  }
}

#[tracing::instrument(
  level = "debug",
  skip(workspace_id, trash_controller, transaction),
  err
)]
fn notify_apps_changed<'a>(
  workspace_id: &str,
  trash_controller: Arc<TrashController>,
  transaction: &'a (dyn FolderPersistenceTransaction + 'a),
) -> FlowyResult<()> {
  let items = read_workspace_apps(workspace_id, trash_controller, transaction)?
    .into_iter()
    .map(|app_rev| app_rev.into())
    .collect();
  let repeated_app = RepeatedAppPB { items };
  send_notification(workspace_id, FolderNotification::DidUpdateWorkspaceApps)
    .payload(repeated_app)
    .send();
  Ok(())
}

pub fn read_workspace_apps<'a>(
  workspace_id: &str,
  trash_controller: Arc<TrashController>,
  transaction: &'a (dyn FolderPersistenceTransaction + 'a),
) -> Result<Vec<AppRevision>, FlowyError> {
  let mut app_revs = transaction.read_workspace_apps(workspace_id)?;
  let trash_ids = trash_controller.read_trash_ids(transaction)?;
  app_revs.retain(|app| !trash_ids.contains(&app.id));
  Ok(app_revs)
}