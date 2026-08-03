//! The managed instance behind a task on ECS Anywhere.
//!
//! A task with the `EXTERNAL` launch type runs on a container instance the ECS
//! agent registered with AWS Systems Manager, so the instance carries an `mi-`
//! managed instance ID and whatever tags its operator gave it. Neither reaches
//! the task metadata endpoint, so the detector asks ECS which container
//! instance holds the task, asks ECS for that instance's managed instance ID,
//! and asks Systems Manager for the tags on it.
//!
//! The three calls need credentials and one permission apiece, all three on
//! the task role rather than the execution role:
//!
//! | Permission                       | Attributes                                  |
//! | -------------------------------- | ------------------------------------------- |
//! | `ecs:DescribeTasks`              | None; it names the container instance       |
//! | `ecs:DescribeContainerInstances` | `aws.ecs.container_instance.arn`, `host.id` |
//! | `ssm:ListTagsForResource`        | `aws.ssm.managed_instance.tag.*`            |
//!
//! The detector takes the credentials the environment offers and reports
//! whatever the permissions allow, printing a notice on standard error for
//! each thing it cannot learn. The repository holds a Terraform module that
//! builds the role and the policy, under `examples/iam`.

use std::future::Future;
use std::time::Duration;

use aws_config::retry::RetryConfig;
use aws_config::timeout::TimeoutConfig;
use aws_config::{BehaviorVersion, Region};
use aws_sdk_ecs::Client as EcsClient;
use aws_sdk_ecs::config::ProvideCredentials;
use aws_sdk_ecs::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_ssm::Client as SsmClient;
use aws_sdk_ssm::types::ResourceTypeForTagging;
use opentelemetry::KeyValue;

use crate::attributes as attr;

/// The launch type ECS gives a task on hardware of the customer's own.
const EXTERNAL_LAUNCH_TYPE: &str = "external";

/// The prefix on every Systems Manager managed instance ID.
const MANAGED_INSTANCE_PREFIX: &str = "mi-";

/// The prefix ECS puts on the task group of a task a service owns.
const SERVICE_GROUP_PREFIX: &str = "service:";

/// How long to spend on the whole lookup. It crosses the network to two
/// regional APIs, so it can hang where the local metadata endpoint cannot, and
/// a detector that delays startup owes the program a quick answer rather than
/// a thorough one.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to spend on a single attempt at one call.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);

/// How many times to try each call before giving up.
const ATTEMPTS: u32 = 2;

/// What ECS and Systems Manager know about the container instance a task runs
/// on.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ManagedInstance {
    /// The ARN of the ECS container instance.
    container_instance_arn: String,

    /// The instance ID ECS reports, which on ECS Anywhere is the Systems
    /// Manager managed instance ID and so begins `mi-`.
    id: String,

    /// The tags on the managed instance, ordered by key.
    tags: Vec<(String, String)>,
}

/// What the lookup learned, and what it could not.
#[derive(Debug, Default, PartialEq, Eq)]
struct Report {
    /// The container instance, once ECS names one.
    instance: Option<ManagedInstance>,

    /// Everything the operator should know that the attributes cannot say.
    notices: Vec<String>,
}

impl Report {
    /// Builds a report that learned nothing and says why.
    fn of(notice: String) -> Self {
        Self {
            instance: None,
            notices: vec![notice],
        }
    }
}

/// Where ECS runs a task.
#[derive(Debug, Default, PartialEq, Eq)]
struct Placement {
    /// The container instance holding the task, which a task on Fargate lacks.
    container_instance_arn: Option<String>,

    /// The service owning the task, which a standalone task lacks.
    service: Option<String>,
}

/// Reports whether a launch type is the one ECS Anywhere uses.
pub(crate) fn is_external(launch_type: &str) -> bool {
    launch_type.eq_ignore_ascii_case(EXTERNAL_LAUNCH_TYPE)
}

/// Looks up the managed instance behind a task and maps it onto resource
/// attributes, printing a notice for whatever it cannot learn.
pub(crate) fn detect(cluster: &str, task_arn: &str, region: Option<&str>) -> Vec<KeyValue> {
    let Some(report) = block_on(report(cluster, task_arn, region)) else {
        notice("the managed instance lookup panicked or found no async runtime");
        return Vec::new();
    };

    for line in &report.notices {
        notice(line);
    }

    report
        .instance
        .as_ref()
        .map(instance_attributes)
        .unwrap_or_default()
}

/// Builds clients from the ambient credentials and runs the lookup under a
/// deadline.
async fn report(cluster: &str, task_arn: &str, region: Option<&str>) -> Report {
    let mut config = aws_config::defaults(BehaviorVersion::latest())
        .retry_config(RetryConfig::standard().with_max_attempts(ATTEMPTS))
        .timeout_config(
            TimeoutConfig::builder()
                .operation_attempt_timeout(ATTEMPT_TIMEOUT)
                .build(),
        );

    // The task ARN names the region the cluster lives in, which beats whatever
    // the environment happens to say.
    if let Some(region) = region {
        config = config.region(Region::new(region.to_string()));
    }
    let config = config.load().await;

    // Resolving the credentials up front separates "nobody gave this task a
    // role" from "the role lacks a permission", which need different fixes.
    let Some(credentials) = config.credentials_provider() else {
        return Report::of(
            "no credentials provider is configured, so the managed instance tags went uncollected"
                .to_string(),
        );
    };
    if let Err(error) = credentials.provide_credentials().await {
        return Report::of(format!(
            "no credentials are available, so the managed instance tags went uncollected: {error}"
        ));
    }

    let ecs = EcsClient::new(&config);
    let ssm = SsmClient::new(&config);

    match tokio::time::timeout(LOOKUP_TIMEOUT, lookup(&ecs, &ssm, cluster, task_arn)).await {
        Ok(report) => report,
        Err(_) => Report::of(format!(
            "the managed instance lookup outlasted its {} second deadline",
            LOOKUP_TIMEOUT.as_secs()
        )),
    }
}

/// Asks ECS and Systems Manager about the container instance holding a task.
///
/// Each step gives up on the step it needs, so a lookup that loses the tags
/// still reports the instance.
async fn lookup(ecs: &EcsClient, ssm: &SsmClient, cluster: &str, task_arn: &str) -> Report {
    let placement = match placement(ecs, cluster, task_arn).await {
        Ok(placement) => placement,
        Err(problem) => return Report::of(problem),
    };

    let mut report = Report::default();

    if placement.service.is_none() {
        report.notices.push(format!(
            "no ECS service owns the task {task_arn}, so its metrics name no service"
        ));
    }

    let Some(container_instance_arn) = placement.container_instance_arn else {
        report.notices.push(format!(
            "ECS puts the task {task_arn} on no container instance, so it has no managed instance"
        ));
        return report;
    };

    let id = match managed_instance_id(ecs, cluster, &container_instance_arn).await {
        Ok(id) => id,
        Err(problem) => {
            report.notices.push(problem);
            return report;
        }
    };

    let mut instance = ManagedInstance {
        container_instance_arn,
        id,
        tags: Vec::new(),
    };

    // An EC2 instance ID here means the cluster mixes launch types and this
    // task landed off ECS Anywhere after all. Systems Manager holds no tags
    // under such an ID, so asking would only earn an error.
    if !instance.id.starts_with(MANAGED_INSTANCE_PREFIX) {
        report.notices.push(format!(
            "the instance {} is no Systems Manager managed instance, so it has no tags to collect",
            instance.id
        ));
        report.instance = Some(instance);
        return report;
    }

    match tags(ssm, &instance.id).await {
        Ok(tags) => instance.tags = tags,
        Err(problem) => report.notices.push(problem),
    }

    report.instance = Some(instance);
    report
}

/// Asks ECS where it runs a task.
async fn placement(ecs: &EcsClient, cluster: &str, task_arn: &str) -> Result<Placement, String> {
    let output = ecs
        .describe_tasks()
        .cluster(cluster)
        .tasks(task_arn)
        .send()
        .await
        .map_err(|error| {
            format!(
                "ECS declined to describe the task {task_arn}, so the managed instance tags went uncollected: {}",
                describe(&error)
            )
        })?;

    let Some(task) = output.tasks().first() else {
        let reason = output
            .failures()
            .first()
            .and_then(|failure| failure.reason())
            .unwrap_or("ECS returned no task and no reason");
        return Err(format!(
            "ECS describes no task {task_arn}, so the managed instance tags went uncollected: {reason}"
        ));
    };

    Ok(Placement {
        container_instance_arn: task.container_instance_arn().map(str::to_string),
        service: service_of(task.group()).map(str::to_string),
    })
}

/// Asks ECS for the managed instance ID of a container instance.
async fn managed_instance_id(
    ecs: &EcsClient,
    cluster: &str,
    container_instance_arn: &str,
) -> Result<String, String> {
    let output = ecs
        .describe_container_instances()
        .cluster(cluster)
        .container_instances(container_instance_arn)
        .send()
        .await
        .map_err(|error| {
            format!(
                "ECS declined to describe the container instance {container_instance_arn}, so the managed instance tags went uncollected: {}",
                describe(&error)
            )
        })?;

    let Some(instance) = output.container_instances().first() else {
        let reason = output
            .failures()
            .first()
            .and_then(|failure| failure.reason())
            .unwrap_or("ECS returned no container instance and no reason");
        return Err(format!(
            "ECS describes no container instance {container_instance_arn}, so the managed instance tags went uncollected: {reason}"
        ));
    };

    instance.ec2_instance_id().map(str::to_string).ok_or_else(|| {
        format!(
            "ECS gives the container instance {container_instance_arn} no instance ID, so the managed instance tags went uncollected"
        )
    })
}

/// Asks Systems Manager for the tags on a managed instance, ordered by key.
async fn tags(ssm: &SsmClient, id: &str) -> Result<Vec<(String, String)>, String> {
    let output = ssm
        .list_tags_for_resource()
        .resource_type(ResourceTypeForTagging::ManagedInstance)
        .resource_id(id)
        .send()
        .await
        .map_err(|error| {
            format!(
                "Systems Manager declined to list the tags on the managed instance {id}: {}",
                describe(&error)
            )
        })?;

    let mut tags: Vec<(String, String)> = output
        .tag_list()
        .iter()
        .map(|tag| (tag.key().to_string(), tag.value().to_string()))
        .collect();
    tags.sort();

    Ok(tags)
}

/// Maps a managed instance onto resource attributes.
fn instance_attributes(instance: &ManagedInstance) -> Vec<KeyValue> {
    let mut attrs = vec![
        KeyValue::new(
            attr::AWS_ECS_CONTAINER_INSTANCE_ARN,
            instance.container_instance_arn.clone(),
        ),
        KeyValue::new(attr::HOST_ID, instance.id.clone()),
    ];

    attrs.extend(instance.tags.iter().map(|(key, value)| {
        KeyValue::new(
            format!("{}{key}", attr::SSM_MANAGED_INSTANCE_TAG_PREFIX),
            value.clone(),
        )
    }));

    attrs
}

/// Pulls the service name out of a task group. ECS groups a standalone task
/// under its family instead, and names no group at all for a task old enough.
fn service_of(group: Option<&str>) -> Option<&str> {
    group?.strip_prefix(SERVICE_GROUP_PREFIX)
}

/// Renders an SDK error the way someone reading a log wants it: the service
/// error code and message when the service answered, and the chain of causes
/// when nothing did.
fn describe<E, R>(error: &SdkError<E, R>) -> String
where
    SdkError<E, R>: ProvideErrorMetadata + std::error::Error,
{
    if let Some(code) = error.code() {
        return match error.message() {
            Some(message) => format!("{code}: {message}"),
            None => code.to_string(),
        };
    }

    // Lacking service metadata, the error says only "dispatch failure" or
    // "service error" on its own, and the causes beneath it hold the story.
    let mut description = error.to_string();
    let mut cause = std::error::Error::source(error);
    while let Some(error) = cause {
        description.push_str(": ");
        description.push_str(&error.to_string());
        cause = error.source();
    }

    description
}

/// Prints a notice on standard error.
///
/// Detection runs before a program has configured its telemetry, so standard
/// error is the one channel the detector can count on.
fn notice(message: &str) {
    eprintln!("opentelemetry-detector-ecs: {message}");
}

/// Runs a future to completion on a thread and a runtime of its own, returning
/// [`None`] if either the thread or the future dies.
///
/// The detector is synchronous and a program may call it from inside a Tokio
/// runtime, where blocking on a nested runtime panics, so the future gets a
/// runtime that belongs to nobody else.
fn block_on<F>(future: F) -> Option<F::Output>
where
    F: Future + Send,
    F::Output: Send,
{
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                Some(runtime.block_on(future))
            })
            .join()
            .ok()
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use aws_sdk_ecs::error::ErrorMetadata;
    use aws_sdk_ecs::operation::describe_container_instances::{
        DescribeContainerInstancesError, DescribeContainerInstancesOutput,
    };
    use aws_sdk_ecs::operation::describe_tasks::{DescribeTasksError, DescribeTasksOutput};
    use aws_sdk_ecs::types::error::{AccessDeniedException, ClusterNotFoundException};
    use aws_sdk_ecs::types::{ContainerInstance, Failure, Task};
    use aws_sdk_ssm::operation::list_tags_for_resource::{
        ListTagsForResourceError, ListTagsForResourceOutput,
    };
    use aws_sdk_ssm::types::Tag;
    use aws_sdk_ssm::types::error::InvalidResourceId;
    use aws_smithy_mocks::{
        MockResponseInterceptor, Rule, RuleMode, create_mock_http_client, mock,
    };

    use super::*;

    const CLUSTER: &str = "arn:aws:ecs:us-west-2:111122223333:cluster/anywhere";
    const TASK_ARN: &str =
        "arn:aws:ecs:us-west-2:111122223333:task/anywhere/158d1c8083dd49d6b527399fd6414f5c";
    const CONTAINER_INSTANCE_ARN: &str = "arn:aws:ecs:us-west-2:111122223333:container-instance/anywhere/cf4b1c93b3f14a2fa5f1e2c19a9e8b1d";
    const MANAGED_INSTANCE_ID: &str = "mi-0123456789abcdef0";

    /// Gathers rules into the interceptor that answers on the network's
    /// behalf, matching whichever rule fits rather than insisting on an order.
    fn interceptor(rules: &[&Rule]) -> MockResponseInterceptor {
        rules.iter().fold(
            MockResponseInterceptor::new().rule_mode(RuleMode::MatchAny),
            |interceptor, rule| interceptor.with_rule(rule),
        )
    }

    /// Builds credentials the mock HTTP layer never checks, which the client
    /// nonetheless insists on before it will sign anything.
    fn credentials() -> aws_sdk_ecs::config::Credentials {
        aws_sdk_ecs::config::Credentials::new("akid", "secret", None, None, "tests")
    }

    /// Builds an ECS client that answers from the rules.
    ///
    /// The SDK ships a `mock_client!` macro for this, but it wants the
    /// `test-util` feature, which drags in a decade-old TLS stack that nothing
    /// here uses and `cargo audit` rightly objects to.
    fn ecs_client(rules: &[&Rule]) -> EcsClient {
        EcsClient::from_conf(
            aws_sdk_ecs::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new("us-west-2"))
                .credentials_provider(credentials())
                .http_client(create_mock_http_client())
                .interceptor(interceptor(rules))
                .build(),
        )
    }

    /// Builds a Systems Manager client that answers from the rules.
    fn ssm_client(rules: &[&Rule]) -> SsmClient {
        SsmClient::from_conf(
            aws_sdk_ssm::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new("us-west-2"))
                .credentials_provider(credentials())
                .http_client(create_mock_http_client())
                .interceptor(interceptor(rules))
                .build(),
        )
    }

    /// Builds a `DescribeTasks` rule answering with one task.
    fn describes_task(task: Task) -> Rule {
        mock!(EcsClient::describe_tasks)
            .match_requests(|input| {
                input.cluster() == Some(CLUSTER) && input.tasks() == [TASK_ARN.to_string()]
            })
            .then_output(move || DescribeTasksOutput::builder().tasks(task.clone()).build())
    }

    /// Builds a task ECS placed on the container instance and owned by a
    /// service.
    fn placed_task() -> Task {
        Task::builder()
            .task_arn(TASK_ARN)
            .container_instance_arn(CONTAINER_INSTANCE_ARN)
            .group("service:checkout")
            .launch_type(aws_sdk_ecs::types::LaunchType::External)
            .build()
    }

    /// Builds a `DescribeContainerInstances` rule answering with one instance
    /// of the given ID.
    fn describes_instance(id: &'static str) -> Rule {
        mock!(EcsClient::describe_container_instances)
            .match_requests(|input| {
                input.cluster() == Some(CLUSTER)
                    && input.container_instances() == [CONTAINER_INSTANCE_ARN.to_string()]
            })
            .then_output(move || {
                DescribeContainerInstancesOutput::builder()
                    .container_instances(
                        ContainerInstance::builder()
                            .container_instance_arn(CONTAINER_INSTANCE_ARN)
                            .ec2_instance_id(id)
                            .build(),
                    )
                    .build()
            })
    }

    /// Builds a `ListTagsForResource` rule answering with the given tags.
    fn lists_tags(tags: &'static [(&'static str, &'static str)]) -> Rule {
        mock!(SsmClient::list_tags_for_resource)
            .match_requests(|input| {
                input.resource_id() == Some(MANAGED_INSTANCE_ID)
                    && input.resource_type() == Some(&ResourceTypeForTagging::ManagedInstance)
            })
            .then_output(move || {
                tags.iter()
                    .fold(
                        ListTagsForResourceOutput::builder(),
                        |output, (key, value)| {
                            output.tag_list(Tag::builder().key(*key).value(*value).build().unwrap())
                        },
                    )
                    .build()
            })
    }

    /// Builds the metadata a service attaches to an error on the wire, which
    /// an exception built by hand otherwise lacks.
    fn wire_error(code: &str, message: &str) -> ErrorMetadata {
        ErrorMetadata::builder().code(code).message(message).build()
    }

    /// Builds the exception either ECS call raises against a role short a
    /// permission.
    fn access_denied(action: &str) -> AccessDeniedException {
        let message = format!("User is not authorized to perform: {action}");
        AccessDeniedException::builder()
            .message(&message)
            .meta(wire_error("AccessDeniedException", &message))
            .build()
    }

    /// Builds an SSM client no test expects to call.
    fn unused_ssm() -> SsmClient {
        let never = mock!(SsmClient::list_tags_for_resource).then_error(|| {
            ListTagsForResourceError::InvalidResourceId(InvalidResourceId::builder().build())
        });
        ssm_client(&[&never])
    }

    fn assert_notices(report: &Report, expected: &[&str]) {
        assert_eq!(
            report.notices.len(),
            expected.len(),
            "notices: {:#?}",
            report.notices
        );
        for (notice, fragment) in report.notices.iter().zip(expected) {
            assert!(
                notice.contains(fragment),
                "notice {notice:?} does not mention {fragment:?}"
            );
        }
    }

    #[test]
    fn external_is_the_launch_type_of_ecs_anywhere() {
        assert!(is_external("EXTERNAL"));
        assert!(is_external("external"));
        assert!(!is_external("EC2"));
        assert!(!is_external("FARGATE"));
        assert!(!is_external(""));
    }

    #[test]
    fn a_service_group_names_a_service() {
        assert_eq!(service_of(Some("service:checkout")), Some("checkout"));
    }

    #[test]
    fn a_family_group_names_no_service() {
        assert_eq!(service_of(Some("family:curltest")), None);
        assert_eq!(service_of(None), None);
    }

    #[test]
    fn instance_attributes_name_the_instance_and_prefix_its_tags() {
        let instance = ManagedInstance {
            container_instance_arn: CONTAINER_INSTANCE_ARN.to_string(),
            id: MANAGED_INSTANCE_ID.to_string(),
            tags: vec![
                ("Environment".to_string(), "production".to_string()),
                ("Role".to_string(), "builder".to_string()),
            ],
        };

        let attrs = instance_attributes(&instance);
        let rendered: Vec<(String, String)> = attrs
            .iter()
            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
            .collect();

        assert_eq!(
            rendered,
            vec![
                (
                    attr::AWS_ECS_CONTAINER_INSTANCE_ARN.to_string(),
                    CONTAINER_INSTANCE_ARN.to_string()
                ),
                (attr::HOST_ID.to_string(), MANAGED_INSTANCE_ID.to_string()),
                (
                    "aws.ssm.managed_instance.tag.Environment".to_string(),
                    "production".to_string()
                ),
                (
                    "aws.ssm.managed_instance.tag.Role".to_string(),
                    "builder".to_string()
                ),
            ]
        );
    }

    #[test]
    fn instance_attributes_of_an_untagged_instance_name_the_instance_alone() {
        let instance = ManagedInstance {
            container_instance_arn: CONTAINER_INSTANCE_ARN.to_string(),
            id: MANAGED_INSTANCE_ID.to_string(),
            tags: Vec::new(),
        };

        assert_eq!(instance_attributes(&instance).len(), 2);
    }

    #[tokio::test]
    async fn lookup_reports_the_instance_and_its_tags() {
        let tasks = describes_task(placed_task());
        let instances = describes_instance(MANAGED_INSTANCE_ID);
        let tags = lists_tags(&[("Role", "builder"), ("Environment", "production")]);

        let ecs = ecs_client(&[&tasks, &instances]);
        let ssm = ssm_client(&[&tags]);

        let report = lookup(&ecs, &ssm, CLUSTER, TASK_ARN).await;

        assert_notices(&report, &[]);
        assert_eq!(
            report.instance,
            Some(ManagedInstance {
                container_instance_arn: CONTAINER_INSTANCE_ARN.to_string(),
                id: MANAGED_INSTANCE_ID.to_string(),
                tags: vec![
                    ("Environment".to_string(), "production".to_string()),
                    ("Role".to_string(), "builder".to_string()),
                ],
            })
        );
        assert_eq!(tasks.num_calls(), 1);
        assert_eq!(instances.num_calls(), 1);
        assert_eq!(tags.num_calls(), 1);
    }

    #[tokio::test]
    async fn lookup_notices_a_task_no_service_owns() {
        let standalone = Task::builder()
            .task_arn(TASK_ARN)
            .container_instance_arn(CONTAINER_INSTANCE_ARN)
            .group("family:curltest")
            .build();

        let tasks = describes_task(standalone);
        let instances = describes_instance(MANAGED_INSTANCE_ID);
        let tags = lists_tags(&[("Role", "builder")]);

        let ecs = ecs_client(&[&tasks, &instances]);
        let ssm = ssm_client(&[&tags]);

        let report = lookup(&ecs, &ssm, CLUSTER, TASK_ARN).await;

        assert_notices(&report, &["no ECS service owns the task"]);
        assert_eq!(
            report
                .instance
                .as_ref()
                .map(|instance| instance.id.as_str()),
            Some(MANAGED_INSTANCE_ID)
        );
    }

    #[tokio::test]
    async fn lookup_notices_a_task_ecs_will_not_describe() {
        let denied = mock!(EcsClient::describe_tasks).then_error(|| {
            DescribeTasksError::AccessDeniedException(access_denied("ecs:DescribeTasks"))
        });

        let ecs = ecs_client(&[&denied]);
        let report = lookup(&ecs, &unused_ssm(), CLUSTER, TASK_ARN).await;

        assert_notices(&report, &["ECS declined to describe the task"]);
        assert!(report.notices[0].contains("ecs:DescribeTasks"));
        assert_eq!(report.instance, None);
    }

    #[tokio::test]
    async fn lookup_notices_a_cluster_ecs_does_not_know() {
        let missing = mock!(EcsClient::describe_tasks).then_error(|| {
            DescribeTasksError::ClusterNotFoundException(
                ClusterNotFoundException::builder()
                    .meta(wire_error(
                        "ClusterNotFoundException",
                        "The specified cluster wasn't found.",
                    ))
                    .build(),
            )
        });

        let ecs = ecs_client(&[&missing]);
        let report = lookup(&ecs, &unused_ssm(), CLUSTER, TASK_ARN).await;

        assert_notices(&report, &["ClusterNotFoundException"]);
        assert_eq!(report.instance, None);
    }

    #[tokio::test]
    async fn lookup_notices_a_task_ecs_returns_as_a_failure() {
        let failed = mock!(EcsClient::describe_tasks).then_output(|| {
            DescribeTasksOutput::builder()
                .failures(Failure::builder().arn(TASK_ARN).reason("MISSING").build())
                .build()
        });

        let ecs = ecs_client(&[&failed]);
        let report = lookup(&ecs, &unused_ssm(), CLUSTER, TASK_ARN).await;

        assert_notices(&report, &["MISSING"]);
        assert_eq!(report.instance, None);
    }

    #[tokio::test]
    async fn lookup_notices_a_task_on_no_container_instance() {
        let fargate = Task::builder()
            .task_arn(TASK_ARN)
            .group("service:checkout")
            .build();

        let tasks = describes_task(fargate);
        let ecs = ecs_client(&[&tasks]);
        let report = lookup(&ecs, &unused_ssm(), CLUSTER, TASK_ARN).await;

        assert_notices(&report, &["on no container instance"]);
        assert_eq!(report.instance, None);
    }

    #[tokio::test]
    async fn lookup_notices_a_container_instance_ecs_will_not_describe() {
        let tasks = describes_task(placed_task());
        let denied = mock!(EcsClient::describe_container_instances).then_error(|| {
            DescribeContainerInstancesError::AccessDeniedException(access_denied(
                "ecs:DescribeContainerInstances",
            ))
        });

        let ecs = ecs_client(&[&tasks, &denied]);
        let report = lookup(&ecs, &unused_ssm(), CLUSTER, TASK_ARN).await;

        assert_notices(
            &report,
            &["ECS declined to describe the container instance"],
        );
        assert_eq!(report.instance, None);
    }

    #[tokio::test]
    async fn lookup_notices_a_container_instance_without_an_instance_id() {
        let tasks = describes_task(placed_task());
        let anonymous = mock!(EcsClient::describe_container_instances).then_output(|| {
            DescribeContainerInstancesOutput::builder()
                .container_instances(
                    ContainerInstance::builder()
                        .container_instance_arn(CONTAINER_INSTANCE_ARN)
                        .build(),
                )
                .build()
        });

        let ecs = ecs_client(&[&tasks, &anonymous]);
        let report = lookup(&ecs, &unused_ssm(), CLUSTER, TASK_ARN).await;

        assert_notices(&report, &["no instance ID"]);
        assert_eq!(report.instance, None);
    }

    #[tokio::test]
    async fn lookup_leaves_an_unmanaged_instance_untagged() {
        let tasks = describes_task(placed_task());
        let instances = describes_instance("i-0123456789abcdef0");

        let ecs = ecs_client(&[&tasks, &instances]);
        let ssm = unused_ssm();
        let report = lookup(&ecs, &ssm, CLUSTER, TASK_ARN).await;

        assert_notices(&report, &["no Systems Manager managed instance"]);
        assert_eq!(
            report.instance,
            Some(ManagedInstance {
                container_instance_arn: CONTAINER_INSTANCE_ARN.to_string(),
                id: "i-0123456789abcdef0".to_string(),
                tags: Vec::new(),
            })
        );
    }

    #[tokio::test]
    async fn lookup_reports_the_instance_when_systems_manager_denies_the_tags() {
        let tasks = describes_task(placed_task());
        let instances = describes_instance(MANAGED_INSTANCE_ID);
        let denied = mock!(SsmClient::list_tags_for_resource)
            .then_http_response(|| {
                aws_smithy_runtime_api::client::orchestrator::HttpResponse::new(
                    400.try_into().unwrap(),
                    aws_smithy_types::body::SdkBody::from(
                        r#"{"__type":"AccessDeniedException","message":"User is not authorized to perform: ssm:ListTagsForResource"}"#,
                    ),
                )
            });

        let ecs = ecs_client(&[&tasks, &instances]);
        let ssm = ssm_client(&[&denied]);

        let report = lookup(&ecs, &ssm, CLUSTER, TASK_ARN).await;

        assert_notices(&report, &["Systems Manager declined to list the tags"]);
        assert!(report.notices[0].contains("ssm:ListTagsForResource"));
        assert_eq!(
            report.instance,
            Some(ManagedInstance {
                container_instance_arn: CONTAINER_INSTANCE_ARN.to_string(),
                id: MANAGED_INSTANCE_ID.to_string(),
                tags: Vec::new(),
            })
        );
    }

    #[tokio::test]
    async fn lookup_reports_an_untagged_instance() {
        let tasks = describes_task(placed_task());
        let instances = describes_instance(MANAGED_INSTANCE_ID);
        let tags = lists_tags(&[]);

        let ecs = ecs_client(&[&tasks, &instances]);
        let ssm = ssm_client(&[&tags]);

        let report = lookup(&ecs, &ssm, CLUSTER, TASK_ARN).await;

        assert_notices(&report, &[]);
        assert_eq!(
            report.instance.map(|instance| instance.tags),
            Some(Vec::new())
        );
    }

    #[tokio::test]
    async fn lookup_asks_about_the_cluster_and_task_it_was_given() {
        // Every rule matches on the cluster and the task, so a lookup that
        // asked about anything else would find no rule and fail here.
        let tasks = describes_task(placed_task());
        let instances = describes_instance(MANAGED_INSTANCE_ID);
        let tags = lists_tags(&[("Role", "builder")]);

        let ecs = ecs_client(&[&tasks, &instances]);
        let ssm = ssm_client(&[&tags]);

        let report = lookup(&ecs, &ssm, CLUSTER, TASK_ARN).await;

        assert_notices(&report, &[]);
        assert_eq!(tasks.num_calls(), 1);
        assert_eq!(instances.num_calls(), 1);
        assert_eq!(tags.num_calls(), 1);
    }
}
