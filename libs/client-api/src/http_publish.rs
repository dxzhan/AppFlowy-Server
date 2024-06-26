use bytes::Bytes;
use database_entity::dto::UpdatePublishNamespace;
use reqwest::{Body, Method};
use shared_entity::response::{AppResponse, AppResponseError};

use crate::Client;

// Publisher API
impl Client {
  pub async fn set_workspace_publish_namespace(
    &self,
    workspace_id: &str,
    new_namespace: &str,
  ) -> Result<(), AppResponseError> {
    let url = format!(
      "{}/api/workspace/{}/publish-namespace",
      self.base_url, workspace_id
    );

    let resp = self
      .http_client_with_auth(Method::PUT, &url)
      .await?
      .json(&UpdatePublishNamespace {
        new_namespace: new_namespace.to_string(),
      })
      .send()
      .await?;

    AppResponse::<()>::from_response(resp).await?.into_error()
  }

  pub async fn get_workspace_publish_namespace(
    &self,
    workspace_id: &str,
  ) -> Result<String, AppResponseError> {
    let url = format!(
      "{}/api/workspace/{}/publish-namespace",
      self.base_url, workspace_id
    );
    let resp = self
      .http_client_with_auth(Method::GET, &url)
      .await?
      .send()
      .await?;
    AppResponse::<String>::from_response(resp)
      .await?
      .into_data()
  }

  pub async fn publish_collab<T>(
    &self,
    workspace_id: &str,
    doc_name: &str,
    metadata: T,
  ) -> Result<(), AppResponseError>
  where
    T: serde::Serialize,
  {
    let url = format!(
      "{}/api/workspace/{}/publish/{}",
      self.base_url, workspace_id, doc_name
    );

    let resp = self
      .http_client_with_auth(Method::PUT, &url)
      .await?
      .json(&metadata)
      .send()
      .await?;

    AppResponse::<()>::from_response(resp).await?.into_error()
  }

  pub async fn put_published_collab_blob<T>(
    &self,
    workspace_id: &str,
    doc_name: &str,
    data: T,
  ) -> Result<(), AppResponseError>
  where
    T: Into<Body>,
  {
    let url = format!(
      "{}/api/workspace/{}/publish/{}/blob",
      self.base_url, workspace_id, doc_name
    );
    let resp = self
      .http_client_with_auth(Method::PUT, &url)
      .await?
      .body(data)
      .send()
      .await?;
    AppResponse::<()>::from_response(resp).await?.into_error()
  }

  pub async fn delete_published_collab(
    &self,
    workspace_id: &str,
    doc_name: &str,
  ) -> Result<(), AppResponseError> {
    let url = format!(
      "{}/api/workspace/{}/publish/{}",
      self.base_url, workspace_id, doc_name
    );
    let resp = self
      .http_client_with_auth(Method::DELETE, &url)
      .await?
      .send()
      .await?;
    AppResponse::<()>::from_response(resp).await?.into_error()
  }
}

// Guest API (no login required)
impl Client {
  pub async fn get_published_collab<T>(
    &self,
    publish_namespace: &str,
    doc_name: &str,
  ) -> Result<T, AppResponseError>
  where
    T: serde::de::DeserializeOwned,
  {
    let url = format!(
      "{}/api/workspace/published/{}/{}",
      self.base_url, publish_namespace, doc_name
    );

    let resp = self
      .cloud_client
      .get(&url)
      .send()
      .await?
      .error_for_status()?;

    let txt = resp.text().await?;

    if let Ok(app_err) = serde_json::from_str::<AppResponseError>(&txt) {
      return Err(app_err);
    }

    let meta = serde_json::from_str::<T>(&txt)?;
    Ok(meta)
  }

  pub async fn get_published_collab_blob(
    &self,
    publish_namespace: &str,
    doc_name: &str,
  ) -> Result<Bytes, AppResponseError> {
    let url = format!(
      "{}/api/workspace/published/{}/{}/blob",
      self.base_url, publish_namespace, doc_name
    );
    let bytes = self
      .cloud_client
      .get(&url)
      .send()
      .await?
      .error_for_status()?
      .bytes()
      .await?;

    if let Ok(app_err) = serde_json::from_slice::<AppResponseError>(&bytes) {
      return Err(app_err);
    }

    Ok(bytes)
  }
}
