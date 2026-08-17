//! What the detector learns about a task, and how it learns it.
//!
//! [`EcsMetadata`] holds the answer in a shape a program can read, match on,
//! and serialize, rather than the flat list of key-value pairs OpenTelemetry
//! wants. [`EcsMetadata::attributes`] turns it into that list, and the detector
//! is little more than the two put together.
//!
//! The gathering happens once per process. See [`EcsMetadata::detect`].

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use arn::naive::NaiveArn;
use opentelemetry::KeyValue;
use opentelemetry_sdk::resource::Resource;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::attributes as attr;

/// The environment variable ECS sets to the task metadata endpoint, version 4.
const V4_URI_VAR: &str = "ECS_CONTAINER_METADATA_URI_V4";

/// The environment variable ECS sets to the task metadata endpoint, version 3.
const V3_URI_VAR: &str = "ECS_CONTAINER_METADATA_URI";

/// How long to wait on the metadata endpoint, which answers from the local
/// host and so should answer quickly.
const METADATA_TIMEOUT: Duration = Duration::from_secs(2);

/// What the first detection found, kept for every detection after it.
static DETECTED: OnceLock<Option<EcsMetadata>> = OnceLock::new();

/// The Amazon ECS task the current process belongs to.
///
/// Every field is what one of the sources named in [`detect`](Self::detect)
/// reported, so a task that reports less carries fewer of them. The type
/// serializes and deserializes, which is the convenient way to hand the
/// description to something that is not OpenTelemetry:
///
/// ```
/// use opentelemetry_detector_ecs::EcsMetadata;
///
/// if let Some(metadata) = EcsMetadata::detect() {
///     println!("{}", serde_json::to_string_pretty(metadata).expect("it serializes"));
/// }
/// ```
///
/// A field the task does not have is absent from the JSON rather than null.
///
/// [`attributes`](Self::attributes) turns the same description into the
/// resource attributes the semantic conventions name.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EcsMetadata {
    /// The container the process runs in.
    pub container: Container,

    /// Where in AWS the task runs.
    pub cloud: Cloud,

    /// The task itself, which only the v4 metadata endpoint describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<Task>,

    /// Where the container sends its output, if it sends it to CloudWatch Logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs: Option<Logs>,

    /// The hardware under a task on ECS Anywhere, which no other launch type
    /// has and the `anywhere` cargo feature carries the lookup for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_instance: Option<ManagedInstance>,
}

/// The container the process runs in.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Container {
    /// The container's name, which ECS puts in `$HOSTNAME`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// The 64-character Docker container ID, from the local cgroup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// The container ARN, which only the v4 metadata endpoint reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arn: Option<String>,
}

/// Where in AWS the task runs.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Cloud {
    /// The region, from the task ARN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// The account the task belongs to, from the task ARN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,

    /// The availability zone, which a task outside one does not have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability_zone: Option<String>,
}

/// The task the container belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Task {
    /// The task ARN.
    pub arn: String,

    /// The ARN of the cluster the task runs in, qualified by the partition,
    /// region, and account of the task ARN if the endpoint gave a bare name.
    pub cluster_arn: String,

    /// The name of the task definition family.
    pub family: String,

    /// The revision of the task definition.
    pub revision: String,

    /// The launch type, as ECS spells it: `EC2`, `FARGATE`, or `EXTERNAL`.
    /// The semantic conventions want it in lowercase, and
    /// [`EcsMetadata::attributes`] reports it that way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_type: Option<String>,
}

impl Task {
    /// Whether the task runs on ECS Anywhere, on hardware of the customer's own.
    ///
    /// ECS spells the launch type in capitals and the semantic conventions
    /// spell it in lowercase, so the comparison ignores the difference.
    pub fn is_external(&self) -> bool {
        self.launch_type
            .as_deref()
            .is_some_and(|launch_type| launch_type.eq_ignore_ascii_case("EXTERNAL"))
    }
}

/// Where the container's `awslogs` log driver sends its output.
///
/// A container on any other log driver, or on an `awslogs` driver that names
/// no group or stream, has none of this.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Logs {
    /// The log group.
    pub group_name: String,

    /// The ARN of the log group.
    pub group_arn: String,

    /// The log stream.
    pub stream_name: String,

    /// The ARN of the log stream.
    pub stream_arn: String,
}

/// The Systems Manager managed instance a task on ECS Anywhere runs on.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ManagedInstance {
    /// The `mi-` ID Systems Manager registered the hardware under.
    pub id: String,

    /// Every tag on the instance, whatever its operator gave it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
}

impl EcsMetadata {
    /// Describes the task the current process belongs to, or reports `None`
    /// off ECS.
    ///
    /// The first call reads the environment, the cgroup, and the task metadata
    /// endpoint, and on ECS Anywhere asks the ECS and Systems Manager APIs
    /// besides. It blocks for up to two seconds on the endpoint and five more
    /// on the APIs, and reports whatever it gathered if they answer slowly,
    /// partially, or not at all.
    ///
    /// Every call after that answers from memory, including calls from other
    /// threads while the first one is still out. A program can therefore ask
    /// as often as it likes, and two callers always read the same description.
    ///
    /// The description outlives the environment it came from, so a variable
    /// set or a tag changed afterwards goes unnoticed.
    /// [`detect_uncached`](Self::detect_uncached) reads again.
    pub fn detect() -> Option<&'static Self> {
        DETECTED.get_or_init(Self::detect_uncached).as_ref()
    }

    /// Describes the task without consulting or filling the cache, at the cost
    /// [`detect`](Self::detect) pays only once.
    ///
    /// A long-running program that expects the answer to change, as the tags
    /// on a managed instance can, reads it again this way.
    pub fn detect_uncached() -> Option<Self> {
        let v4 = std::env::var(V4_URI_VAR).ok();
        let has_v3 = std::env::var(V3_URI_VAR).is_ok();
        if v4.is_none() && !has_v3 {
            return None;
        }

        let mut metadata = EcsMetadata {
            container: Container {
                name: std::env::var("HOSTNAME")
                    .or_else(|_| hostname_fallback())
                    .ok(),
                id: container_id(),
                arn: None,
            },
            ..Default::default()
        };

        // The v3 endpoint carries none of the fields below, so a v3-only task
        // gets the container name and ID and nothing more.
        let Some(uri) = v4 else {
            return Some(metadata);
        };

        let Ok(client) = reqwest::blocking::Client::builder()
            .timeout(METADATA_TIMEOUT)
            .build()
        else {
            return Some(metadata);
        };

        let task: Option<TaskMetadataV4> = client
            .get(format!("{uri}/task"))
            .send()
            .ok()
            .and_then(|response| response.json().ok());

        // Every remaining field is qualified by the task ARN, so an unparsable
        // one ends the detection.
        let Some(task) = task else {
            return Some(metadata);
        };
        let Ok(task_ref) = NaiveArn::parse(&task.task_arn) else {
            return Some(metadata);
        };

        metadata.cloud = cloud(&task, &task_ref);
        metadata.task = Some(described_task(&task, &task_ref));

        // ECS Anywhere runs the task on hardware the metadata endpoint says
        // nothing about, so the managed instance under it takes three API calls.
        #[cfg(feature = "anywhere")]
        if metadata
            .task
            .as_ref()
            .is_some_and(|described| described.is_external())
        {
            metadata.managed_instance =
                crate::anywhere::describe(task_ref.region, &task.cluster, &task.task_arn);
        }

        let container: Option<ContainerMetadataV4> = client
            .get(&uri)
            .send()
            .ok()
            .and_then(|response| response.json().ok());

        if let Some(container) = container {
            let arn = qualify(&container.container_arn, "container", &task_ref);
            metadata.logs = logs(&container, &arn, &task_ref);
            metadata.container.arn = Some(arn);
        }

        Some(metadata)
    }

    /// Names the task in the resource attributes of the semantic conventions.
    ///
    /// The keys are the constants in [`attributes`](crate::attributes), and
    /// `cloud.provider` and `cloud.platform` are there whatever the task
    /// reported, since ECS is where the description came from.
    pub fn attributes(&self) -> Vec<KeyValue> {
        let mut attrs = vec![
            KeyValue::new(attr::CLOUD_PROVIDER, "aws"),
            KeyValue::new(attr::CLOUD_PLATFORM, "aws_ecs"),
        ];

        if let Some(name) = &self.container.name {
            attrs.push(KeyValue::new(attr::CONTAINER_NAME, name.clone()));
        }
        if let Some(id) = &self.container.id {
            attrs.push(KeyValue::new(attr::CONTAINER_ID, id.clone()));
        }

        if let Some(region) = &self.cloud.region {
            attrs.push(KeyValue::new(attr::CLOUD_REGION, region.clone()));
        }
        if let Some(account) = &self.cloud.account_id {
            attrs.push(KeyValue::new(attr::CLOUD_ACCOUNT_ID, account.clone()));
        }
        if let Some(zone) = &self.cloud.availability_zone {
            attrs.push(KeyValue::new(attr::CLOUD_AVAILABILITY_ZONE, zone.clone()));
        }

        if let Some(task) = &self.task {
            attrs.push(KeyValue::new(
                attr::AWS_ECS_CLUSTER_ARN,
                task.cluster_arn.clone(),
            ));
            if let Some(launch_type) = &task.launch_type {
                attrs.push(KeyValue::new(
                    attr::AWS_ECS_LAUNCHTYPE,
                    launch_type.to_lowercase(),
                ));
            }
            attrs.push(KeyValue::new(attr::AWS_ECS_TASK_ARN, task.arn.clone()));
            attrs.push(KeyValue::new(
                attr::AWS_ECS_TASK_FAMILY,
                task.family.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::AWS_ECS_TASK_REVISION,
                task.revision.clone(),
            ));
        }

        if let Some(instance) = &self.managed_instance {
            attrs.push(KeyValue::new(attr::HOST_ID, instance.id.clone()));
            attrs.extend(instance.tags.iter().map(|(key, value)| {
                KeyValue::new(
                    format!("{}{key}", attr::AWS_ECS_CONTAINER_INSTANCE_TAG_PREFIX),
                    value.clone(),
                )
            }));
        }

        if let Some(logs) = &self.logs {
            attrs.push(KeyValue::new(
                attr::AWS_LOG_GROUP_NAMES,
                logs.group_name.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::AWS_LOG_GROUP_ARNS,
                logs.group_arn.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::AWS_LOG_STREAM_NAMES,
                logs.stream_name.clone(),
            ));
            attrs.push(KeyValue::new(
                attr::AWS_LOG_STREAM_ARNS,
                logs.stream_arn.clone(),
            ));
        }

        if let Some(arn) = &self.container.arn {
            attrs.push(KeyValue::new(attr::CLOUD_RESOURCE_ID, arn.clone()));
            attrs.push(KeyValue::new(attr::AWS_ECS_CONTAINER_ARN, arn.clone()));
        }

        attrs
    }

    /// Puts [`attributes`](Self::attributes) in a [`Resource`].
    ///
    /// The resource carries those attributes alone, so merging it into another
    /// leaves everything the other one has, the default service name included.
    pub fn resource(&self) -> Resource {
        Resource::builder_empty()
            .with_attributes(self.attributes())
            .build()
    }
}

#[derive(Deserialize, Debug)]
struct TaskMetadataV4 {
    #[serde(rename = "Cluster")]
    cluster: String,
    #[serde(rename = "TaskARN")]
    task_arn: String,
    #[serde(rename = "Family")]
    family: String,
    #[serde(rename = "Revision")]
    revision: String,
    #[serde(rename = "AvailabilityZone", default)]
    availability_zone: String,
    #[serde(rename = "LaunchType", default)]
    launch_type: String,
}

#[derive(Deserialize, Debug)]
struct ContainerMetadataV4 {
    #[serde(rename = "ContainerARN")]
    container_arn: String,
    #[serde(rename = "LogDriver", default)]
    log_driver: String,
    #[serde(rename = "LogOptions", default)]
    log_options: Option<LogOptions>,
}

#[derive(Deserialize, Default, Debug)]
struct LogOptions {
    #[serde(rename = "awslogs-group", default)]
    group: String,
    #[serde(rename = "awslogs-stream", default)]
    stream: String,
    #[serde(rename = "awslogs-region", default)]
    region: String,
}

/// Reads the region and account out of the task ARN, and the zone out of the
/// task metadata.
fn cloud(task: &TaskMetadataV4, task_ref: &NaiveArn) -> Cloud {
    Cloud {
        region: task_ref.region.map(ToString::to_string),
        account_id: task_ref.account_id.map(ToString::to_string),
        availability_zone: non_empty(&task.availability_zone),
    }
}

/// Maps task metadata onto the task it describes.
fn described_task(task: &TaskMetadataV4, task_ref: &NaiveArn) -> Task {
    Task {
        arn: task.task_arn.clone(),
        cluster_arn: qualify(&task.cluster, "cluster", task_ref),
        family: task.family.clone(),
        revision: task.revision.clone(),
        launch_type: non_empty(&task.launch_type),
    }
}

/// Maps the container's log configuration onto the group and stream it names,
/// falling back to the container and then the task ARN for whatever the driver
/// leaves unset.
fn logs(container: &ContainerMetadataV4, container_arn: &str, task_ref: &NaiveArn) -> Option<Logs> {
    if container.log_driver != "awslogs" {
        return None;
    }

    let options = container.log_options.as_ref()?;
    if options.group.is_empty() || options.stream.is_empty() {
        return None;
    }

    let container_ref = NaiveArn::parse(container_arn).ok();
    let container_ref = container_ref.as_ref();

    let partition = container_ref.map_or(task_ref.partition, |c| c.partition);
    let account = container_ref
        .and_then(|c| c.account_id)
        .or(task_ref.account_id)
        .unwrap_or_default();
    let region = if options.region.is_empty() {
        container_ref
            .and_then(|c| c.region)
            .or(task_ref.region)
            .unwrap_or_default()
    } else {
        options.region.as_str()
    };

    let group = &options.group;
    let stream = &options.stream;

    Some(Logs {
        group_name: group.clone(),
        group_arn: format!("arn:{partition}:logs:{region}:{account}:log-group:{group}:*"),
        stream_name: stream.clone(),
        stream_arn: format!(
            "arn:{partition}:logs:{region}:{account}:log-group:{group}:log-stream:{stream}"
        ),
    })
}

/// Turns a bare `cluster-name` into a full ARN, using the partition, region,
/// and account of an already-qualified sibling ARN.
fn qualify(name: &str, resource_type: &str, template: &NaiveArn) -> String {
    if name.starts_with("arn:") {
        return name.to_string();
    }
    format!(
        "arn:{}:ecs:{}:{}:{resource_type}/{name}",
        template.partition,
        template.region.unwrap_or_default(),
        template.account_id.unwrap_or_default(),
    )
}

/// Keeps a value the endpoint reported, and drops the empty string it reports
/// for a field the task does not have.
fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn container_id() -> Option<String> {
    container_id_from_cgroup(&std::fs::read_to_string("/proc/self/cgroup").ok()?)
}

/// Pulls the 64-character Docker container ID out of a cgroup file, if one of
/// its lines names an ECS container.
fn container_id_from_cgroup(cgroup: &str) -> Option<String> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();

    let pattern = PATTERN
        .get_or_init(|| Regex::new(r"/ecs/[^/]+/([a-f0-9]{64})$").expect("the pattern is valid"));

    cgroup
        .lines()
        .find_map(|line| pattern.captures(line).map(|c| c[1].to_string()))
}

fn hostname_fallback() -> Result<String, std::io::Error> {
    Ok(std::fs::read_to_string("/proc/sys/kernel/hostname")?
        .trim()
        .to_string())
}

#[cfg(test)]
mod tests {
    use opentelemetry::{Key, Value};

    use super::*;

    /// The examples AWS publishes for the task metadata endpoint, version 4.
    const TASK_JSON: &str = include_str!("../tests/fixtures/task.json");
    const CONTAINER_JSON: &str = include_str!("../tests/fixtures/container.json");

    const TASK_ARN: &str =
        "arn:aws:ecs:us-west-2:111122223333:task/default/158d1c8083dd49d6b527399fd6414f5c";
    const CONTAINER_ARN: &str =
        "arn:aws:ecs:us-west-2:111122223333:container/acfcddf8-14b5-4d2a-9c1c-4b5e0ee2b8b4";

    fn task_metadata() -> TaskMetadataV4 {
        serde_json::from_str(TASK_JSON).expect("the task fixture parses")
    }

    fn container_metadata() -> ContainerMetadataV4 {
        serde_json::from_str(CONTAINER_JSON).expect("the container fixture parses")
    }

    fn task_ref(arn: &str) -> NaiveArn<'_> {
        NaiveArn::parse(arn).expect("the ARN parses")
    }

    /// The description the fixtures add up to, as detection would assemble it.
    fn detected() -> EcsMetadata {
        let metadata = task_metadata();
        let task_ref = task_ref(&metadata.task_arn);
        let container = container_metadata();
        let container_arn = qualify(&container.container_arn, "container", &task_ref);

        EcsMetadata {
            container: Container {
                name: Some("ip-10-0-0-1.us-west-2.compute.internal".to_string()),
                id: Some(
                    "43481a6ce4842eec8fe72fc28500c6b52edcc0917f105b83379f88cac1ff3946".to_string(),
                ),
                arn: Some(container_arn.clone()),
            },
            cloud: cloud(&metadata, &task_ref),
            task: Some(described_task(&metadata, &task_ref)),
            logs: logs(&container, &container_arn, &task_ref),
            managed_instance: None,
        }
    }

    fn attribute<'a>(attrs: &'a [KeyValue], key: &str) -> Option<&'a Value> {
        attrs
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| &kv.value)
    }

    fn assert_attribute(attrs: &[KeyValue], key: &str, expected: &str) {
        assert_eq!(
            attribute(attrs, key).map(ToString::to_string).as_deref(),
            Some(expected),
            "attribute {key}"
        );
    }

    #[test]
    fn the_task_metadata_describes_the_task() {
        let metadata = detected();
        let task = metadata.task.expect("the fixture describes a task");

        assert_eq!(task.arn, TASK_ARN);
        assert_eq!(task.family, "curltest");
        assert_eq!(task.revision, "26");
        assert_eq!(metadata.cloud.region.as_deref(), Some("us-west-2"));
        assert_eq!(metadata.cloud.account_id.as_deref(), Some("111122223333"));
        assert_eq!(
            metadata.cloud.availability_zone.as_deref(),
            Some("us-west-2d")
        );
    }

    #[test]
    fn the_cluster_name_grows_into_an_arn() {
        let task = detected().task.expect("the fixture describes a task");

        assert_eq!(
            task.cluster_arn,
            "arn:aws:ecs:us-west-2:111122223333:cluster/default"
        );
    }

    #[test]
    fn a_full_cluster_arn_stays_as_it_is() {
        let arn = "arn:aws:ecs:us-east-1:444455556666:cluster/other";

        assert_eq!(qualify(arn, "cluster", &task_ref(TASK_ARN)), arn);
    }

    #[test]
    fn the_launch_type_keeps_the_spelling_ecs_uses() {
        let task = detected().task.expect("the fixture describes a task");

        assert_eq!(task.launch_type.as_deref(), Some("EC2"));
        assert!(!task.is_external());
    }

    #[test]
    fn only_the_external_launch_type_is_ecs_anywhere() {
        let external = |launch_type: Option<&str>| {
            Task {
                arn: TASK_ARN.to_string(),
                cluster_arn: String::new(),
                family: String::new(),
                revision: String::new(),
                launch_type: launch_type.map(ToString::to_string),
            }
            .is_external()
        };

        assert!(external(Some("EXTERNAL")));
        assert!(external(Some("external")));

        assert!(!external(Some("EC2")));
        assert!(!external(Some("FARGATE")));
        assert!(!external(None));
    }

    #[test]
    fn the_container_metadata_describes_the_logs() {
        let logs = detected().logs.expect("the fixture logs to CloudWatch");

        assert_eq!(logs.group_name, "/ecs/metadata");
        assert_eq!(
            logs.group_arn,
            "arn:aws:logs:us-west-2:111122223333:log-group:/ecs/metadata:*"
        );
        assert_eq!(
            logs.stream_name,
            "ecs/curl/8f03e41243824aea923aca126495f665"
        );
        assert_eq!(
            logs.stream_arn,
            "arn:aws:logs:us-west-2:111122223333:log-group:/ecs/metadata:log-stream:ecs/curl/8f03e41243824aea923aca126495f665"
        );
    }

    #[test]
    fn another_log_driver_describes_no_logs() {
        let mut container = container_metadata();
        container.log_driver = "json-file".to_string();

        assert_eq!(logs(&container, CONTAINER_ARN, &task_ref(TASK_ARN)), None);
    }

    #[test]
    fn logs_need_both_a_group_and_a_stream() {
        let mut container = container_metadata();
        container.log_options = Some(LogOptions {
            group: "/ecs/metadata".to_string(),
            ..Default::default()
        });

        assert_eq!(logs(&container, CONTAINER_ARN, &task_ref(TASK_ARN)), None);
    }

    #[test]
    fn logs_fall_back_to_the_container_region() {
        let mut container = container_metadata();
        container.log_options = Some(LogOptions {
            group: "/ecs/metadata".to_string(),
            stream: "ecs/curl/abc".to_string(),
            region: String::new(),
        });

        let container_arn = "arn:aws:ecs:eu-central-1:111122223333:container/abc";
        let logs = logs(&container, container_arn, &task_ref(TASK_ARN))
            .expect("the driver names a group and a stream");

        assert_eq!(
            logs.group_arn,
            "arn:aws:logs:eu-central-1:111122223333:log-group:/ecs/metadata:*"
        );
    }

    #[test]
    fn the_attributes_name_everything_detected() {
        let mut metadata = detected();
        metadata.managed_instance = Some(ManagedInstance {
            id: "mi-0f7e1c9d3b5a8e2c4".to_string(),
            tags: BTreeMap::from([("Env".to_string(), "production".to_string())]),
        });
        let attrs = metadata.attributes();

        assert_attribute(&attrs, attr::CLOUD_PROVIDER, "aws");
        assert_attribute(&attrs, attr::CLOUD_PLATFORM, "aws_ecs");
        assert_attribute(
            &attrs,
            attr::CONTAINER_NAME,
            "ip-10-0-0-1.us-west-2.compute.internal",
        );
        assert_attribute(
            &attrs,
            attr::CONTAINER_ID,
            "43481a6ce4842eec8fe72fc28500c6b52edcc0917f105b83379f88cac1ff3946",
        );
        assert_attribute(&attrs, attr::CLOUD_REGION, "us-west-2");
        assert_attribute(&attrs, attr::CLOUD_ACCOUNT_ID, "111122223333");
        assert_attribute(&attrs, attr::CLOUD_AVAILABILITY_ZONE, "us-west-2d");
        assert_attribute(
            &attrs,
            attr::AWS_ECS_CLUSTER_ARN,
            "arn:aws:ecs:us-west-2:111122223333:cluster/default",
        );
        assert_attribute(&attrs, attr::AWS_ECS_TASK_ARN, TASK_ARN);
        assert_attribute(&attrs, attr::AWS_ECS_TASK_FAMILY, "curltest");
        assert_attribute(&attrs, attr::AWS_ECS_TASK_REVISION, "26");
        assert_attribute(&attrs, attr::CLOUD_RESOURCE_ID, CONTAINER_ARN);
        assert_attribute(&attrs, attr::AWS_ECS_CONTAINER_ARN, CONTAINER_ARN);
        assert_attribute(&attrs, attr::AWS_LOG_GROUP_NAMES, "/ecs/metadata");
        assert_attribute(
            &attrs,
            attr::AWS_LOG_GROUP_ARNS,
            "arn:aws:logs:us-west-2:111122223333:log-group:/ecs/metadata:*",
        );
        assert_attribute(
            &attrs,
            attr::AWS_LOG_STREAM_NAMES,
            "ecs/curl/8f03e41243824aea923aca126495f665",
        );
        assert_attribute(
            &attrs,
            attr::AWS_LOG_STREAM_ARNS,
            "arn:aws:logs:us-west-2:111122223333:log-group:/ecs/metadata:log-stream:ecs/curl/8f03e41243824aea923aca126495f665",
        );
        assert_attribute(&attrs, attr::HOST_ID, "mi-0f7e1c9d3b5a8e2c4");
        assert_attribute(&attrs, "aws.ecs.container_instance.tag.Env", "production");
    }

    #[test]
    fn the_attributes_lowercase_the_launch_type() {
        let attrs = detected().attributes();

        assert_attribute(&attrs, attr::AWS_ECS_LAUNCHTYPE, "ec2");
    }

    #[test]
    fn nothing_detected_names_ecs_and_nothing_else() {
        let attrs = EcsMetadata::default().attributes();

        assert_eq!(attrs.len(), 2);
        assert_attribute(&attrs, attr::CLOUD_PROVIDER, "aws");
        assert_attribute(&attrs, attr::CLOUD_PLATFORM, "aws_ecs");
    }

    #[test]
    fn the_resource_does_not_include_the_default_service_name() {
        let resource = EcsMetadata::default().resource();

        assert_eq!(
            resource.get(&Key::new(attr::CLOUD_PROVIDER)),
            Some("aws".into())
        );
        assert_eq!(resource.get(&Key::new("service.name")), None);
    }

    #[test]
    fn the_description_serializes_to_json() {
        let metadata = EcsMetadata {
            container: Container {
                name: Some("ip-10-0-0-1".to_string()),
                id: None,
                arn: None,
            },
            cloud: Cloud {
                region: Some("us-west-2".to_string()),
                account_id: None,
                availability_zone: None,
            },
            task: None,
            logs: None,
            managed_instance: Some(ManagedInstance {
                id: "mi-0f7e1c9d3b5a8e2c4".to_string(),
                tags: BTreeMap::from([("Env".to_string(), "production".to_string())]),
            }),
        };

        // Whatever the task does not have is absent, not null.
        assert_eq!(
            serde_json::to_string(&metadata).expect("it serializes"),
            r#"{"container":{"name":"ip-10-0-0-1"},"cloud":{"region":"us-west-2"},"managed_instance":{"id":"mi-0f7e1c9d3b5a8e2c4","tags":{"Env":"production"}}}"#
        );
    }

    #[test]
    fn the_description_comes_back_from_json() {
        let metadata = detected();
        let json = serde_json::to_string(&metadata).expect("it serializes");

        assert_eq!(
            serde_json::from_str::<EcsMetadata>(&json).expect("it deserializes"),
            metadata
        );
    }

    #[test]
    fn nothing_is_detected_off_of_ecs() {
        // Both metadata variables are absent under `cargo test`, so detection
        // has nothing to go on.
        assert_eq!(EcsMetadata::detect_uncached(), None);
        assert_eq!(EcsMetadata::detect(), None);
    }

    #[test]
    fn detection_happens_once() {
        assert_eq!(EcsMetadata::detect(), EcsMetadata::detect());
        assert!(DETECTED.get().is_some(), "the first call filled the cache");
    }

    #[test]
    fn container_id_comes_from_the_cgroup() {
        let cgroup = "\
11:devices:/ecs/158d1c8083dd49d6b527399fd6414f5c/43481a6ce4842eec8fe72fc28500c6b52edcc0917f105b83379f88cac1ff3946
10:memory:/ecs/158d1c8083dd49d6b527399fd6414f5c/43481a6ce4842eec8fe72fc28500c6b52edcc0917f105b83379f88cac1ff3946
";

        assert_eq!(
            container_id_from_cgroup(cgroup).as_deref(),
            Some("43481a6ce4842eec8fe72fc28500c6b52edcc0917f105b83379f88cac1ff3946")
        );
    }

    #[test]
    fn container_id_ignores_a_cgroup_from_elsewhere() {
        let cgroup = "\
11:devices:/user.slice
10:memory:/docker/43481a6ce4842eec8fe72fc28500c6b52edcc0917f105b83379f88cac1ff3946
";

        assert_eq!(container_id_from_cgroup(cgroup), None);
    }
}
