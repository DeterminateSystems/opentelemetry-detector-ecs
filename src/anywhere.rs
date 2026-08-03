//! The attributes a task gets from ECS Anywhere.
//!
//! A task with the `EXTERNAL` launch type runs on hardware of the customer's
//! own, which the ECS agent registered as a Systems Manager managed instance.
//! The instance carries an `mi-` ID and whatever tags its operator gave it, and
//! neither reaches the task metadata endpoint, so this module asks the APIs
//! instead: `ecs:DescribeTasks` names the container instance holding the task,
//! `ecs:DescribeContainerInstances` turns that into an `mi-` ID, and
//! `ssm:ListTagsForResource` lists the tags on it.
//!
//! Each call needs a permission the task role may not carry. A lookup that
//! cannot finish reports whatever it reached and leaves a notice on standard
//! error naming the permission, since a detector answers to no logger of its
//! own and a missing attribute is not worth failing a program over.

use std::time::Duration;

use aws_config::BehaviorVersion;
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_ecs::config::Region;
use aws_sdk_ecs::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_ssm::types::ResourceTypeForTagging;
use opentelemetry::KeyValue;

use crate::attributes as attr;

/// The launch type ECS reports for a task on the customer's own hardware.
pub(crate) const EXTERNAL_LAUNCH_TYPE: &str = "EXTERNAL";

/// The error code every AWS service returns for a permission the caller lacks.
const ACCESS_DENIED: &str = "AccessDeniedException";

/// How long the three calls together may take. They cross the network to a
/// regional endpoint, so they want a longer deadline than the local metadata
/// endpoint, but not one a program would notice at startup.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// What a lookup learned, and what it could not.
#[derive(Debug, Default, PartialEq)]
struct Lookup {
    attributes: Vec<KeyValue>,
    notices: Vec<String>,
}

impl Lookup {
    /// Records that the lookup fell short, and how.
    fn note(&mut self, notice: String) {
        self.notices.push(notice);
    }

    /// Prints the notices, one to a line.
    fn print_notices(&self) {
        for notice in &self.notices {
            eprintln!("opentelemetry-detector-ecs: {notice}");
        }
    }
}

/// Describes the managed instance the task runs on, blocking until it knows or
/// the deadline passes.
///
/// The lookup gets a thread and a runtime of its own, so a caller already
/// inside Tokio can block on it without nesting one runtime in another.
pub(crate) fn attributes(region: Option<&str>, cluster: &str, task_arn: &str) -> Vec<KeyValue> {
    let lookup = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();

                let runtime = match runtime {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let mut lookup = Lookup::default();
                        lookup.note(format!("could not start a runtime: {error}"));
                        return lookup;
                    }
                };

                runtime.block_on(async {
                    let deadline =
                        tokio::time::timeout(LOOKUP_TIMEOUT, lookup(region, cluster, task_arn));
                    deadline.await.unwrap_or_else(|_| {
                        let mut lookup = Lookup::default();
                        lookup.note(format!(
                            "gave up on the managed instance after {} seconds",
                            LOOKUP_TIMEOUT.as_secs()
                        ));
                        lookup
                    })
                })
            })
            .join()
            .unwrap_or_default()
    });

    lookup.print_notices();
    lookup.attributes
}

/// Takes the credentials the environment supplies and asks the two services.
async fn lookup(region: Option<&str>, cluster: &str, task_arn: &str) -> Lookup {
    // The environment names the region on a well-configured host, but the task
    // ARN names it too, and the cluster is wherever the ARN says it is.
    let region = RegionProviderChain::default_provider()
        .or_else(region.map(|region| Region::new(region.to_string())));

    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(region)
        .load()
        .await;

    let ecs = aws_sdk_ecs::Client::new(&config);
    let ssm = aws_sdk_ssm::Client::new(&config);

    managed_instance(&ecs, &ssm, cluster, task_arn).await
}

/// Names the managed instance holding the task and lists the tags on it.
async fn managed_instance(
    ecs: &aws_sdk_ecs::Client,
    ssm: &aws_sdk_ssm::Client,
    cluster: &str,
    task_arn: &str,
) -> Lookup {
    let mut lookup = Lookup::default();

    let Some(instance_id) = managed_instance_id(ecs, cluster, task_arn, &mut lookup).await else {
        return lookup;
    };

    lookup
        .attributes
        .push(KeyValue::new(attr::HOST_ID, instance_id.clone()));

    // The ID is worth reporting on its own, so a role short of the Systems
    // Manager permission still gets one attribute out of the lookup.
    let tags = tags(ssm, &instance_id, &mut lookup).await;
    lookup.attributes.extend(tags);

    lookup
}

/// Walks from the task to the container instance holding it, and from there to
/// the Systems Manager ID of the hardware underneath.
async fn managed_instance_id(
    ecs: &aws_sdk_ecs::Client,
    cluster: &str,
    task_arn: &str,
    lookup: &mut Lookup,
) -> Option<String> {
    let tasks = ecs
        .describe_tasks()
        .cluster(cluster)
        .tasks(task_arn)
        .send()
        .await;

    let tasks = match tasks {
        Ok(tasks) => tasks,
        Err(error) => {
            lookup.note(explain("ecs:DescribeTasks", &error));
            return None;
        }
    };

    let Some(container_instance_arn) = tasks
        .tasks()
        .first()
        .and_then(|task| task.container_instance_arn())
    else {
        lookup.note(format!("ECS knows no container instance for {task_arn}"));
        return None;
    };

    let instances = ecs
        .describe_container_instances()
        .cluster(cluster)
        .container_instances(container_instance_arn)
        .send()
        .await;

    let instances = match instances {
        Ok(instances) => instances,
        Err(error) => {
            lookup.note(explain("ecs:DescribeContainerInstances", &error));
            return None;
        }
    };

    let Some(instance_id) = instances
        .container_instances()
        .first()
        .and_then(|instance| instance.ec2_instance_id())
    else {
        lookup.note(format!(
            "ECS knows no instance ID for {container_instance_arn}"
        ));
        return None;
    };

    Some(instance_id.to_string())
}

/// Lists the tags on a managed instance, one attribute to a tag.
async fn tags(ssm: &aws_sdk_ssm::Client, instance_id: &str, lookup: &mut Lookup) -> Vec<KeyValue> {
    let tags = ssm
        .list_tags_for_resource()
        .resource_type(ResourceTypeForTagging::ManagedInstance)
        .resource_id(instance_id)
        .send()
        .await;

    let tags = match tags {
        Ok(tags) => tags,
        Err(error) => {
            lookup.note(explain("ssm:ListTagsForResource", &error));
            return Vec::new();
        }
    };

    tags.tag_list()
        .iter()
        .map(|tag| {
            KeyValue::new(
                format!(
                    "{}{}",
                    attr::AWS_ECS_CONTAINER_INSTANCE_TAG_PREFIX,
                    tag.key()
                ),
                tag.value().to_string(),
            )
        })
        .collect()
}

/// Says what went wrong with a call, singling out the permission the task role
/// lacks from every other way a call can fail.
fn explain<E, R>(action: &str, error: &SdkError<E, R>) -> String
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug,
{
    if error.code() == Some(ACCESS_DENIED) {
        format!("the task role cannot call {action}")
    } else {
        format!("{action} failed: {}", DisplayErrorContext(error))
    }
}
