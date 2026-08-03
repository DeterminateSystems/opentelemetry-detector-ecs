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
const EXTERNAL_LAUNCH_TYPE: &str = "EXTERNAL";

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

/// Whether a launch type puts the task on ECS Anywhere.
///
/// ECS spells the launch type in capitals and the semantic conventions spell it
/// in lowercase, so the comparison ignores the difference rather than depend on
/// which spelling reaches it.
pub(crate) fn is_external(launch_type: &str) -> bool {
    launch_type.eq_ignore_ascii_case(EXTERNAL_LAUNCH_TYPE)
}

/// Describes the managed instance the task runs on, blocking until it knows or
/// the deadline passes.
///
/// The lookup gets a thread and a runtime of its own, so a caller already
/// inside Tokio can block on it without nesting one runtime in another.
pub(crate) fn attributes(region: Option<&str>, cluster: &str, task_arn: &str) -> Vec<KeyValue> {
    let region = region.map(ToString::to_string);
    let cluster = cluster.to_string();
    let task_arn = task_arn.to_string();

    let lookup = detached(LOOKUP_TIMEOUT, async move {
        lookup(region.as_deref(), &cluster, &task_arn).await
    });

    lookup.print_notices();
    lookup.attributes
}

/// Runs a lookup on a thread and a runtime of its own, and waits for it.
///
/// The thread is what lets a caller already inside Tokio block on the result,
/// since a runtime cannot nest inside another. It also keeps whatever goes
/// wrong on that side of the boundary: a lookup that overruns the deadline or
/// panics outright costs the attributes it would have found and no more.
fn detached<F>(deadline: Duration, lookup: F) -> Lookup
where
    F: Future<Output = Lookup> + Send + 'static,
{
    let worker = std::thread::spawn(move || {
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

        runtime.block_on(async move {
            tokio::time::timeout(deadline, lookup)
                .await
                .unwrap_or_else(|_| {
                    let mut lookup = Lookup::default();
                    lookup.note(format!(
                        "gave up on the managed instance after {} seconds",
                        deadline.as_secs()
                    ));
                    lookup
                })
        })
    });

    worker.join().unwrap_or_else(|_| {
        let mut lookup = Lookup::default();
        lookup.note("the managed instance lookup panicked".to_string());
        lookup
    })
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

#[cfg(test)]
mod tests {
    use aws_sdk_ecs::config::Credentials;
    use aws_sdk_ecs::config::retry::RetryConfig;
    use aws_sdk_ecs::error::ErrorMetadata;
    use aws_sdk_ecs::operation::describe_container_instances::{
        DescribeContainerInstancesError, DescribeContainerInstancesOutput,
    };
    use aws_sdk_ecs::operation::describe_tasks::{DescribeTasksError, DescribeTasksOutput};
    use aws_sdk_ssm::operation::list_tags_for_resource::ListTagsForResourceError;
    use aws_smithy_mocks::{
        MockResponseInterceptor, Rule, RuleMode, create_mock_http_client, mock,
    };

    use super::*;

    const CLUSTER: &str = "default";
    const TASK_ARN: &str =
        "arn:aws:ecs:us-west-2:111122223333:task/default/158d1c8083dd49d6b527399fd6414f5c";
    const CONTAINER_INSTANCE_ARN: &str = "arn:aws:ecs:us-west-2:111122223333:container-instance/default/cd2c1a51e1b34cd1a4a9c4e4b5f8b0d2";
    const INSTANCE_ID: &str = "mi-0f7e1c9d3b5a8e2c4";

    /// Builds a client that answers from the rules rather than the network.
    ///
    /// The `mock_client!` macro would do this, but it wants the SDK's
    /// `test-util` feature, which pulls in a TLS stack a decade out of date.
    macro_rules! client {
        ($sdk:ident, $($rule:expr),+ $(,)?) => {{
            let interceptor = MockResponseInterceptor::new().rule_mode(RuleMode::MatchAny);
            $(let interceptor = interceptor.with_rule(&$rule);)+

            $sdk::Client::from_conf(
                $sdk::Config::builder()
                    .behavior_version(BehaviorVersion::latest())
                    .credentials_provider(Credentials::new("id", "secret", None, None, "tests"))
                    .region(Region::new("us-west-2"))
                    // Without this a denied call is denied three times over,
                    // and the test waits out the backoff in between.
                    .retry_config(RetryConfig::disabled())
                    .http_client(create_mock_http_client())
                    .interceptor(interceptor)
                    .build(),
            )
        }};
    }

    /// The metadata AWS attaches to a call the caller has no permission for.
    fn access_denied() -> ErrorMetadata {
        ErrorMetadata::builder()
            .code(ACCESS_DENIED)
            .message("User is not authorized to perform this action")
            .build()
    }

    /// A `DescribeTasks` that names the container instance holding the task.
    fn describes_the_task() -> Rule {
        mock!(aws_sdk_ecs::Client::describe_tasks)
            .match_requests(|input| {
                input.cluster() == Some(CLUSTER) && input.tasks() == [TASK_ARN.to_string()]
            })
            .then_output(|| {
                DescribeTasksOutput::builder()
                    .tasks(
                        aws_sdk_ecs::types::Task::builder()
                            .task_arn(TASK_ARN)
                            .container_instance_arn(CONTAINER_INSTANCE_ARN)
                            .build(),
                    )
                    .build()
            })
    }

    /// A `DescribeContainerInstances` that names the managed instance under it.
    fn describes_the_container_instance() -> Rule {
        mock!(aws_sdk_ecs::Client::describe_container_instances)
            .match_requests(|input| {
                input.container_instances() == [CONTAINER_INSTANCE_ARN.to_string()]
            })
            .then_output(|| {
                DescribeContainerInstancesOutput::builder()
                    .container_instances(
                        aws_sdk_ecs::types::ContainerInstance::builder()
                            .container_instance_arn(CONTAINER_INSTANCE_ARN)
                            .ec2_instance_id(INSTANCE_ID)
                            .build(),
                    )
                    .build()
            })
    }

    /// A `ListTagsForResource` returning the given tags on the managed instance.
    fn lists_tags(tags: &'static [(&'static str, &'static str)]) -> Rule {
        mock!(aws_sdk_ssm::Client::list_tags_for_resource)
            .match_requests(|input| {
                input.resource_id() == Some(INSTANCE_ID)
                    && input.resource_type() == Some(&ResourceTypeForTagging::ManagedInstance)
            })
            .then_output(move || {
                let list = tags.iter().map(|(key, value)| {
                    aws_sdk_ssm::types::Tag::builder()
                        .key(*key)
                        .value(*value)
                        .build()
                        .expect("the tag has a key and a value")
                });

                aws_sdk_ssm::operation::list_tags_for_resource::ListTagsForResourceOutput::builder()
                    .set_tag_list(Some(list.collect()))
                    .build()
            })
    }

    /// The attributes of a lookup, as key and value strings.
    fn attributes_of(lookup: &Lookup) -> Vec<(String, String)> {
        lookup
            .attributes
            .iter()
            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
            .collect()
    }

    #[test]
    fn only_the_external_launch_type_is_ecs_anywhere() {
        assert!(is_external("EXTERNAL"));
        assert!(is_external("external"));

        assert!(!is_external("EC2"));
        assert!(!is_external("FARGATE"));
        assert!(!is_external(""));
    }

    #[tokio::test]
    async fn describes_the_managed_instance_and_its_tags() {
        let ecs = client!(
            aws_sdk_ecs,
            describes_the_task(),
            describes_the_container_instance()
        );
        let ssm = client!(
            aws_sdk_ssm,
            lists_tags(&[("Env", "production"), ("Rack", "b12")])
        );

        let lookup = managed_instance(&ecs, &ssm, CLUSTER, TASK_ARN).await;

        assert_eq!(
            attributes_of(&lookup),
            [
                (attr::HOST_ID.to_string(), INSTANCE_ID.to_string()),
                (
                    "aws.ecs.container_instance.tag.Env".to_string(),
                    "production".to_string()
                ),
                (
                    "aws.ecs.container_instance.tag.Rack".to_string(),
                    "b12".to_string()
                ),
            ]
        );
        assert_eq!(lookup.notices, Vec::<String>::new());
    }

    #[tokio::test]
    async fn describes_an_untagged_managed_instance() {
        let ecs = client!(
            aws_sdk_ecs,
            describes_the_task(),
            describes_the_container_instance()
        );
        let ssm = client!(aws_sdk_ssm, lists_tags(&[]));

        let lookup = managed_instance(&ecs, &ssm, CLUSTER, TASK_ARN).await;

        assert_eq!(
            attributes_of(&lookup),
            [(attr::HOST_ID.to_string(), INSTANCE_ID.to_string())]
        );
        assert_eq!(lookup.notices, Vec::<String>::new());
    }

    #[tokio::test]
    async fn notes_a_role_that_cannot_describe_tasks() {
        let denied = mock!(aws_sdk_ecs::Client::describe_tasks).then_error(|| {
            DescribeTasksError::AccessDeniedException(
                aws_sdk_ecs::types::error::AccessDeniedException::builder()
                    .meta(access_denied())
                    .build(),
            )
        });

        let ecs = client!(aws_sdk_ecs, denied);
        let ssm = client!(aws_sdk_ssm, lists_tags(&[]));

        let lookup = managed_instance(&ecs, &ssm, CLUSTER, TASK_ARN).await;

        assert_eq!(attributes_of(&lookup), []);
        assert_eq!(
            lookup.notices,
            ["the task role cannot call ecs:DescribeTasks"]
        );
    }

    #[tokio::test]
    async fn notes_a_role_that_cannot_describe_container_instances() {
        let denied = mock!(aws_sdk_ecs::Client::describe_container_instances).then_error(|| {
            DescribeContainerInstancesError::AccessDeniedException(
                aws_sdk_ecs::types::error::AccessDeniedException::builder()
                    .meta(access_denied())
                    .build(),
            )
        });

        let ecs = client!(aws_sdk_ecs, describes_the_task(), denied);
        let ssm = client!(aws_sdk_ssm, lists_tags(&[]));

        let lookup = managed_instance(&ecs, &ssm, CLUSTER, TASK_ARN).await;

        assert_eq!(attributes_of(&lookup), []);
        assert_eq!(
            lookup.notices,
            ["the task role cannot call ecs:DescribeContainerInstances"]
        );
    }

    #[tokio::test]
    async fn notes_a_cluster_ecs_does_not_know() {
        let missing = mock!(aws_sdk_ecs::Client::describe_tasks).then_error(|| {
            DescribeTasksError::ClusterNotFoundException(
                aws_sdk_ecs::types::error::ClusterNotFoundException::builder()
                    .message("Cluster not found.")
                    .build(),
            )
        });

        let ecs = client!(aws_sdk_ecs, missing);
        let ssm = client!(aws_sdk_ssm, lists_tags(&[]));

        let lookup = managed_instance(&ecs, &ssm, CLUSTER, TASK_ARN).await;

        assert_eq!(attributes_of(&lookup), []);

        // Anything but a missing permission says so in the service's own words.
        let [notice] = &lookup.notices[..] else {
            panic!("expected one notice, got {:?}", lookup.notices);
        };
        assert!(
            notice.starts_with("ecs:DescribeTasks failed: "),
            "notice {notice:?}"
        );
        assert!(notice.contains("Cluster not found."), "notice {notice:?}");
    }

    #[tokio::test]
    async fn notes_a_task_ecs_does_not_return() {
        let empty = mock!(aws_sdk_ecs::Client::describe_tasks)
            .then_output(|| DescribeTasksOutput::builder().build());

        let ecs = client!(aws_sdk_ecs, empty);
        let ssm = client!(aws_sdk_ssm, lists_tags(&[]));

        let lookup = managed_instance(&ecs, &ssm, CLUSTER, TASK_ARN).await;

        assert_eq!(attributes_of(&lookup), []);
        assert_eq!(
            lookup.notices,
            [format!("ECS knows no container instance for {TASK_ARN}")]
        );
    }

    #[tokio::test]
    async fn notes_a_task_on_no_container_instance() {
        // A task ECS itself hosts, which the launch type should have ruled out.
        let hosted = mock!(aws_sdk_ecs::Client::describe_tasks).then_output(|| {
            DescribeTasksOutput::builder()
                .tasks(
                    aws_sdk_ecs::types::Task::builder()
                        .task_arn(TASK_ARN)
                        .build(),
                )
                .build()
        });

        let ecs = client!(aws_sdk_ecs, hosted);
        let ssm = client!(aws_sdk_ssm, lists_tags(&[]));

        let lookup = managed_instance(&ecs, &ssm, CLUSTER, TASK_ARN).await;

        assert_eq!(attributes_of(&lookup), []);
        assert_eq!(
            lookup.notices,
            [format!("ECS knows no container instance for {TASK_ARN}")]
        );
    }

    #[tokio::test]
    async fn notes_a_container_instance_with_no_instance_id() {
        let nameless = mock!(aws_sdk_ecs::Client::describe_container_instances).then_output(|| {
            DescribeContainerInstancesOutput::builder()
                .container_instances(
                    aws_sdk_ecs::types::ContainerInstance::builder()
                        .container_instance_arn(CONTAINER_INSTANCE_ARN)
                        .build(),
                )
                .build()
        });

        let ecs = client!(aws_sdk_ecs, describes_the_task(), nameless);
        let ssm = client!(aws_sdk_ssm, lists_tags(&[]));

        let lookup = managed_instance(&ecs, &ssm, CLUSTER, TASK_ARN).await;

        assert_eq!(attributes_of(&lookup), []);
        assert_eq!(
            lookup.notices,
            [format!(
                "ECS knows no instance ID for {CONTAINER_INSTANCE_ARN}"
            )]
        );
    }

    #[tokio::test]
    async fn notes_a_role_that_cannot_list_tags() {
        // Systems Manager models no access denied error for this call, so the
        // code arrives as metadata on an otherwise unhandled one.
        let denied = mock!(aws_sdk_ssm::Client::list_tags_for_resource)
            .then_error(|| ListTagsForResourceError::generic(access_denied()));

        let ecs = client!(
            aws_sdk_ecs,
            describes_the_task(),
            describes_the_container_instance()
        );
        let ssm = client!(aws_sdk_ssm, denied);

        let lookup = managed_instance(&ecs, &ssm, CLUSTER, TASK_ARN).await;

        // The ID survives the tags the lookup could not reach.
        assert_eq!(
            attributes_of(&lookup),
            [(attr::HOST_ID.to_string(), INSTANCE_ID.to_string())]
        );
        assert_eq!(
            lookup.notices,
            ["the task role cannot call ssm:ListTagsForResource"]
        );
    }

    #[tokio::test]
    async fn notes_an_instance_systems_manager_does_not_know() {
        let unknown = mock!(aws_sdk_ssm::Client::list_tags_for_resource).then_error(|| {
            ListTagsForResourceError::InvalidResourceId(
                aws_sdk_ssm::types::error::InvalidResourceId::builder().build(),
            )
        });

        let ecs = client!(
            aws_sdk_ecs,
            describes_the_task(),
            describes_the_container_instance()
        );
        let ssm = client!(aws_sdk_ssm, unknown);

        let lookup = managed_instance(&ecs, &ssm, CLUSTER, TASK_ARN).await;

        assert_eq!(
            attributes_of(&lookup),
            [(attr::HOST_ID.to_string(), INSTANCE_ID.to_string())]
        );

        let [notice] = &lookup.notices[..] else {
            panic!("expected one notice, got {:?}", lookup.notices);
        };
        assert!(
            notice.starts_with("ssm:ListTagsForResource failed: "),
            "notice {notice:?}"
        );
    }

    /// A lookup that found one attribute and had nothing to complain about.
    fn found() -> Lookup {
        Lookup {
            attributes: vec![KeyValue::new(attr::HOST_ID, INSTANCE_ID)],
            notices: Vec::new(),
        }
    }

    #[test]
    fn detached_runs_outside_a_runtime() {
        assert_eq!(detached(LOOKUP_TIMEOUT, async { found() }), found());
    }

    #[tokio::test]
    async fn detached_runs_inside_a_runtime() {
        // A runtime cannot nest inside another, so a caller that already has
        // one would panic here if the lookup did not get a thread of its own.
        assert_eq!(detached(LOOKUP_TIMEOUT, async { found() }), found());
    }

    #[test]
    fn detached_gives_up_at_the_deadline() {
        let lookup = detached(Duration::ZERO, std::future::pending());

        assert_eq!(lookup.attributes, []);
        assert_eq!(
            lookup.notices,
            ["gave up on the managed instance after 0 seconds"]
        );
    }

    #[test]
    fn detached_survives_a_panic() {
        let lookup = detached(LOOKUP_TIMEOUT, async { panic!("the lookup gave out") });

        assert_eq!(lookup.attributes, []);
        assert_eq!(lookup.notices, ["the managed instance lookup panicked"]);
    }
}
