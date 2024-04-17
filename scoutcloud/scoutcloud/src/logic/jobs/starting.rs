use super::global;
use crate::logic::{DeployError, Deployment, GithubClient, Instance};

use fang::{typetag, AsyncQueueable, AsyncRunnable, FangError, Scheduled};

use scoutcloud_entity::sea_orm_active_enums::DeploymentStatusType;
use sea_orm::DatabaseConnection;
use std::time::Duration;

const WORKFLOW_TIMEOUT: Duration = Duration::from_secs(3 * 60);
const WORKFLOW_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const SLEEP_AFTER_POSTGRES: Duration = Duration::from_secs(30);

#[derive(fang::serde::Serialize, fang::serde::Deserialize, Debug)]
#[serde(crate = "fang::serde")]
pub struct StartingTask {
    deployment_id: i32,
}

impl StartingTask {
    pub fn new(deployment_id: i32) -> Self {
        Self { deployment_id }
    }
}

#[typetag::serde]
#[fang::async_trait]
impl AsyncRunnable for StartingTask {
    #[tracing::instrument(skip(_client), level = "info")]
    async fn run(&self, _client: &mut dyn AsyncQueueable) -> Result<(), FangError> {
        let db = global::get_db_connection();
        let github = global::get_github_client();

        let mut deployment = Deployment::get(db.as_ref(), self.deployment_id)
            .await
            .map_err(DeployError::Db)?;
        let instance = deployment
            .get_instance(db.as_ref())
            .await
            .map_err(DeployError::Db)?;

        let allowed_statuses = [DeploymentStatusType::Created, DeploymentStatusType::Stopped];
        if !allowed_statuses.contains(&deployment.model.status) {
            tracing::warn!(
                "cannot start deployment '{}': not in created/stopped state",
                self.deployment_id
            );
            return Ok(());
        };

        if let Err(err) =
            github_deploy_and_wait(db.as_ref(), github.as_ref(), &instance, &mut deployment).await
        {
            tracing::error!("failed to start deployment: {}", err);
            deployment
                .mark_as_error(db.as_ref(), format!("failed to start deployment: {}", err))
                .await
                .map_err(DeployError::Db)?;
        };

        Ok(())
    }

    fn cron(&self) -> Option<Scheduled> {
        None
    }
}

async fn github_deploy_and_wait(
    db: &DatabaseConnection,
    github: &GithubClient,
    instance: &Instance,
    deployment: &mut Deployment,
) -> Result<(), DeployError> {
    let postgres_run = instance.deploy_postgres(github).await?;
    deployment
        .update_status(db, DeploymentStatusType::Pending)
        .await?;
    github
        .wait_for_success_workflow(
            "deploy postgres",
            postgres_run.id,
            WORKFLOW_TIMEOUT,
            WORKFLOW_CHECK_INTERVAL,
        )
        .await?;

    tracing::info!(
        "successfully deployed postgres, waiting for {} seconds",
        SLEEP_AFTER_POSTGRES.as_secs()
    );
    tokio::time::sleep(SLEEP_AFTER_POSTGRES).await;

    let microservices_run = instance.deploy_microservices(github).await?;
    github
        .wait_for_success_workflow(
            "deploy microservices",
            microservices_run.id,
            WORKFLOW_TIMEOUT,
            WORKFLOW_CHECK_INTERVAL,
        )
        .await?;

    deployment.mark_as_running(db).await?;
    Ok(())
}
