use crate::Client;
use client_api_entity::billing_dto::WorkspaceUsageAndLimit;
use reqwest::Method;
use serde_json::json;
use shared_entity::{
  dto::billing_dto::{RecurringInterval, SubscriptionPlan, WorkspaceSubscriptionStatus},
  response::{AppResponse, AppResponseError},
};

lazy_static::lazy_static! {
  static ref BASE_BILLING_URL: Option<String> = match std::env::var("APPFLOWY_CLOUD_BASE_BILLING_URL") {
    Ok(url) => Some(url),
    Err(err) => {
      tracing::warn!("std::env::var(APPFLOWY_CLOUD_BASE_BILLING_URL): {}", err);
      None
    },
  };
}

impl Client {
  pub fn base_billing_url(&self) -> &str {
    BASE_BILLING_URL.as_deref().unwrap_or(&self.base_url)
  }

  pub async fn customer_id(&self) -> Result<String, AppResponseError> {
    let url = format!("{}/billing/api/v1/customer-id", self.base_billing_url());
    let resp = self
      .http_client_with_auth(Method::GET, &url)
      .await?
      .send()
      .await?;

    AppResponse::<String>::from_response(resp)
      .await?
      .into_data()
  }

  pub async fn create_subscription(
    &self,
    workspace_id: &str,
    recurring_interval: RecurringInterval,
    workspace_subscription_plan: SubscriptionPlan,
    success_url: &str,
  ) -> Result<String, AppResponseError> {
    let url = format!(
      "{}/billing/api/v1/subscription-link",
      self.base_billing_url()
    );
    let resp = self
      .http_client_with_auth(Method::GET, &url)
      .await?
      .query(&[
        ("workspace_id", workspace_id),
        ("recurring_interval", recurring_interval.as_str()),
        (
          "workspace_subscription_plan",
          workspace_subscription_plan.as_ref(),
        ),
        ("success_url", success_url),
      ])
      .send()
      .await?;

    AppResponse::<String>::from_response(resp)
      .await?
      .into_data()
  }

  pub async fn cancel_subscription(
    &self,
    workspace_id: &str,
    plan: &SubscriptionPlan,
  ) -> Result<(), AppResponseError> {
    let url = format!(
      "{}/billing/api/v1/cancel-subscription",
      self.base_billing_url()
    );
    let resp = self
      .http_client_with_auth(Method::POST, &url)
      .await?
      .json(&json!({
          "workspace_id": workspace_id,
          "plan": plan.as_ref(),
      }))
      .send()
      .await?;
    AppResponse::<()>::from_response(resp).await?.into_error()
  }

  pub async fn list_subscription(
    &self,
  ) -> Result<Vec<WorkspaceSubscriptionStatus>, AppResponseError> {
    let url = format!(
      "{}/billing/api/v1/subscription-status",
      self.base_billing_url(),
    );
    let resp = self
      .http_client_with_auth(Method::GET, &url)
      .await?
      .send()
      .await?;

    AppResponse::<Vec<WorkspaceSubscriptionStatus>>::from_response(resp)
      .await?
      .into_data()
  }

  pub async fn get_portal_session_link(&self) -> Result<String, AppResponseError> {
    let url = format!(
      "{}/billing/api/v1/portal-session-link",
      self.base_billing_url()
    );
    let portal_url = self
      .http_client_with_auth(Method::GET, &url)
      .await?
      .send()
      .await?
      .error_for_status()?
      .json::<AppResponse<String>>()
      .await?
      .into_data()?;
    Ok(portal_url)
  }

  pub async fn get_workspace_usage_and_limit(
    &self,
    workspace_id: &str,
  ) -> Result<WorkspaceUsageAndLimit, AppResponseError> {
    let url = format!(
      "{}/api/workspace/{}/usage-and-limit",
      self.base_url, workspace_id
    );
    self
      .http_client_with_auth(Method::GET, &url)
      .await?
      .send()
      .await?
      .error_for_status()?
      .json::<AppResponse<WorkspaceUsageAndLimit>>()
      .await?
      .into_data()
  }
}
