//! An OpenTelemetry resource detector for Amazon ECS.
//!
//! [`EcsResourceDetector`] reads the ECS task metadata endpoint and reports the
//! cloud, container, task, and log attributes named by the [semantic
//! conventions for ECS][conventions]. Anywhere else it reports nothing, so a
//! program that also runs outside ECS can register it unconditionally:
//!
//! ```
//! use opentelemetry_detector_ecs::EcsResourceDetector;
//! use opentelemetry_sdk::Resource;
//!
//! let resource = Resource::builder()
//!     .with_detector(Box::new(EcsResourceDetector))
//!     .build();
//! ```
//!
//! Every key it reports is a public constant in [`attributes`].
//!
//! Detection blocks for up to two seconds while it queries the metadata
//! endpoint, and it reports whatever it has gathered so far if the endpoint
//! answers slowly, partially, or not at all.
//!
//! [conventions]: https://opentelemetry.io/docs/specs/semconv/resource/cloud-provider/aws/ecs/
//
// Ported from the Go detector in opentelemetry-go-contrib, which is also
// licensed under the Apache License, Version 2.0:
// https://github.com/open-telemetry/opentelemetry-go-contrib/blob/4610324d288f2b56faf237d67b85678f8e6de387/detectors/aws/ecs/ecs.go

#![deny(missing_docs)]

use std::sync::OnceLock;
use std::time::Duration;

use arn::naive::NaiveArn;
use opentelemetry::KeyValue;
use opentelemetry_sdk::resource::{Resource, ResourceDetector};
use regex::Regex;
use serde::Deserialize;

use crate::attributes as attr;

/// Every resource attribute key the detector reports.
///
/// The keys come from [`opentelemetry_semantic_conventions`], which names them
/// all, so a caller can match on what the detector produces without depending
/// on that crate directly. The detector itself reads them from here, so the two
/// lists cannot drift apart.
pub mod attributes {
    pub use opentelemetry_semantic_conventions::resource::{
        AWS_ECS_CLUSTER_ARN, AWS_ECS_CONTAINER_ARN, AWS_ECS_LAUNCHTYPE, AWS_ECS_TASK_ARN,
        AWS_ECS_TASK_FAMILY, AWS_ECS_TASK_REVISION, AWS_LOG_GROUP_ARNS, AWS_LOG_GROUP_NAMES,
        AWS_LOG_STREAM_ARNS, AWS_LOG_STREAM_NAMES, CLOUD_ACCOUNT_ID, CLOUD_AVAILABILITY_ZONE,
        CLOUD_PLATFORM, CLOUD_PROVIDER, CLOUD_REGION, CLOUD_RESOURCE_ID, CONTAINER_ID,
        CONTAINER_NAME,
    };
}

/// The environment variable ECS sets to the task metadata endpoint, version 4.
const V4_URI_VAR: &str = "ECS_CONTAINER_METADATA_URI_V4";

/// The environment variable ECS sets to the task metadata endpoint, version 3.
const V3_URI_VAR: &str = "ECS_CONTAINER_METADATA_URI";

/// How long to wait on the metadata endpoint, which answers from the local
/// host and so should answer quickly.
const METADATA_TIMEOUT: Duration = Duration::from_secs(2);

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

/// Describes the Amazon ECS task the current process belongs to.
///
/// The detector recognizes ECS by the `ECS_CONTAINER_METADATA_URI_V4` and
/// `ECS_CONTAINER_METADATA_URI` environment variables. Given the v4 endpoint it
/// reports the full set of attributes; given only v3 it reports the container
/// name and ID; given neither it reports an empty [`Resource`].
///
/// See the [crate documentation](crate) for an example.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EcsResourceDetector;

impl EcsResourceDetector {
    /// Builds a detector.
    pub fn new() -> Self {
        Self
    }

    fn detected_resource(attrs: Vec<KeyValue>) -> Resource {
        Resource::builder_empty().with_attributes(attrs).build()
    }

    fn container_id() -> Option<String> {
        container_id_from_cgroup(&std::fs::read_to_string("/proc/self/cgroup").ok()?)
    }
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

impl ResourceDetector for EcsResourceDetector {
    fn detect(&self) -> Resource {
        let v4 = std::env::var(V4_URI_VAR).ok();
        let has_v3 = std::env::var(V3_URI_VAR).is_ok();
        if v4.is_none() && !has_v3 {
            return Resource::builder_empty().build();
        }

        let mut attrs = vec![
            KeyValue::new(attr::CLOUD_PROVIDER, "aws"),
            KeyValue::new(attr::CLOUD_PLATFORM, "aws_ecs"),
        ];

        if let Ok(name) = std::env::var("HOSTNAME").or_else(|_| hostname_fallback()) {
            attrs.push(KeyValue::new(attr::CONTAINER_NAME, name));
        }
        if let Some(id) = Self::container_id() {
            attrs.push(KeyValue::new(attr::CONTAINER_ID, id));
        }

        // The v3 endpoint carries none of the attributes below, so a v3-only
        // task gets the container attributes and nothing more.
        let Some(uri) = v4 else {
            return Self::detected_resource(attrs);
        };

        let Ok(client) = reqwest::blocking::Client::builder()
            .timeout(METADATA_TIMEOUT)
            .build()
        else {
            return Self::detected_resource(attrs);
        };

        let task: Option<TaskMetadataV4> = client
            .get(format!("{uri}/task"))
            .send()
            .ok()
            .and_then(|response| response.json().ok());

        // Every remaining attribute is qualified by the task ARN, so an
        // unparsable one ends the detection.
        let Some(task) = task else {
            return Self::detected_resource(attrs);
        };
        let Ok(task_ref) = NaiveArn::parse(&task.task_arn) else {
            return Self::detected_resource(attrs);
        };

        attrs.extend(task_attributes(&task, &task_ref));

        let container: Option<ContainerMetadataV4> = client
            .get(&uri)
            .send()
            .ok()
            .and_then(|response| response.json().ok());

        if let Some(container) = container {
            attrs.extend(container_attributes(&container, &task_ref));
        }

        Self::detected_resource(attrs)
    }
}

/// Maps task metadata onto resource attributes.
fn task_attributes(task: &TaskMetadataV4, task_ref: &NaiveArn) -> Vec<KeyValue> {
    let mut attrs = Vec::new();

    if let Some(region) = task_ref.region {
        attrs.push(KeyValue::new(attr::CLOUD_REGION, region.to_string()));
    }
    if let Some(account) = task_ref.account_id {
        attrs.push(KeyValue::new(attr::CLOUD_ACCOUNT_ID, account.to_string()));
    }
    if !task.availability_zone.is_empty() {
        attrs.push(KeyValue::new(
            attr::CLOUD_AVAILABILITY_ZONE,
            task.availability_zone.clone(),
        ));
    }

    attrs.push(KeyValue::new(
        attr::AWS_ECS_CLUSTER_ARN,
        qualify(&task.cluster, "cluster", task_ref),
    ));
    attrs.push(KeyValue::new(
        attr::AWS_ECS_LAUNCHTYPE,
        task.launch_type.to_lowercase(),
    ));
    attrs.push(KeyValue::new(attr::AWS_ECS_TASK_ARN, task.task_arn.clone()));
    attrs.push(KeyValue::new(
        attr::AWS_ECS_TASK_FAMILY,
        task.family.clone(),
    ));
    attrs.push(KeyValue::new(
        attr::AWS_ECS_TASK_REVISION,
        task.revision.clone(),
    ));

    attrs
}

/// Maps container metadata, including its log configuration, onto resource
/// attributes.
fn container_attributes(container: &ContainerMetadataV4, task_ref: &NaiveArn) -> Vec<KeyValue> {
    let mut attrs = Vec::new();

    let container_arn = qualify(&container.container_arn, "container", task_ref);

    if container.log_driver == "awslogs"
        && let Some(options) = &container.log_options
    {
        let container_ref = NaiveArn::parse(&container_arn).ok();
        attrs.extend(log_attributes(options, container_ref.as_ref(), task_ref));
    }

    attrs.push(KeyValue::new(
        attr::CLOUD_RESOURCE_ID,
        container_arn.clone(),
    ));
    attrs.push(KeyValue::new(attr::AWS_ECS_CONTAINER_ARN, container_arn));

    attrs
}

/// Maps an `awslogs` log driver configuration onto resource attributes,
/// falling back to the container and then the task ARN for whatever the driver
/// leaves unset.
fn log_attributes(
    options: &LogOptions,
    container_ref: Option<&NaiveArn>,
    task_ref: &NaiveArn,
) -> Vec<KeyValue> {
    if options.group.is_empty() || options.stream.is_empty() {
        return Vec::new();
    }

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

    vec![
        KeyValue::new(attr::AWS_LOG_GROUP_NAMES, group.clone()),
        KeyValue::new(
            attr::AWS_LOG_GROUP_ARNS,
            format!("arn:{partition}:logs:{region}:{account}:log-group:{group}:*"),
        ),
        KeyValue::new(attr::AWS_LOG_STREAM_NAMES, stream.clone()),
        KeyValue::new(
            attr::AWS_LOG_STREAM_ARNS,
            format!(
                "arn:{partition}:logs:{region}:{account}:log-group:{group}:log-stream:{stream}"
            ),
        ),
    ]
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

    fn task() -> TaskMetadataV4 {
        serde_json::from_str(TASK_JSON).expect("the task fixture parses")
    }

    fn container() -> ContainerMetadataV4 {
        serde_json::from_str(CONTAINER_JSON).expect("the container fixture parses")
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

    /// The rest of the tests name keys by constant on both sides, so this one
    /// pins the constants to the strings that go over the wire. A semantic
    /// conventions release that renames a key fails here.
    #[test]
    fn the_keys_spell_what_the_conventions_spell() {
        assert_eq!(attributes::CLOUD_PROVIDER, "cloud.provider");
        assert_eq!(attributes::CLOUD_PLATFORM, "cloud.platform");
        assert_eq!(attributes::CLOUD_REGION, "cloud.region");
        assert_eq!(attributes::CLOUD_ACCOUNT_ID, "cloud.account.id");
        assert_eq!(
            attributes::CLOUD_AVAILABILITY_ZONE,
            "cloud.availability_zone"
        );
        assert_eq!(attributes::CLOUD_RESOURCE_ID, "cloud.resource_id");
        assert_eq!(attributes::CONTAINER_NAME, "container.name");
        assert_eq!(attributes::CONTAINER_ID, "container.id");
        assert_eq!(attributes::AWS_ECS_CLUSTER_ARN, "aws.ecs.cluster.arn");
        assert_eq!(attributes::AWS_ECS_CONTAINER_ARN, "aws.ecs.container.arn");
        assert_eq!(attributes::AWS_ECS_LAUNCHTYPE, "aws.ecs.launchtype");
        assert_eq!(attributes::AWS_ECS_TASK_ARN, "aws.ecs.task.arn");
        assert_eq!(attributes::AWS_ECS_TASK_FAMILY, "aws.ecs.task.family");
        assert_eq!(attributes::AWS_ECS_TASK_REVISION, "aws.ecs.task.revision");
        assert_eq!(attributes::AWS_LOG_GROUP_NAMES, "aws.log.group.names");
        assert_eq!(attributes::AWS_LOG_GROUP_ARNS, "aws.log.group.arns");
        assert_eq!(attributes::AWS_LOG_STREAM_NAMES, "aws.log.stream.names");
        assert_eq!(attributes::AWS_LOG_STREAM_ARNS, "aws.log.stream.arns");
    }

    #[test]
    fn detected_resource_does_not_include_default_service_name() {
        let resource = EcsResourceDetector::detected_resource(vec![KeyValue::new(
            attr::CLOUD_PROVIDER,
            "aws",
        )]);

        assert_eq!(
            resource.get(&Key::new(attr::CLOUD_PROVIDER)),
            Some("aws".into())
        );
        assert_eq!(resource.get(&Key::new("service.name")), None);
    }

    #[test]
    fn detects_nothing_off_of_ecs() {
        // Both metadata variables are absent under `cargo test`, so the
        // detector has nothing to go on.
        assert_eq!(
            EcsResourceDetector.detect(),
            Resource::builder_empty().build()
        );
    }

    #[test]
    fn task_attributes_describe_the_task() {
        let task = task();
        let task_ref = NaiveArn::parse(&task.task_arn).expect("the task ARN parses");
        let attrs = task_attributes(&task, &task_ref);

        assert_attribute(&attrs, attr::CLOUD_REGION, "us-west-2");
        assert_attribute(&attrs, attr::CLOUD_ACCOUNT_ID, "111122223333");
        assert_attribute(&attrs, attr::CLOUD_AVAILABILITY_ZONE, "us-west-2d");
        assert_attribute(&attrs, attr::AWS_ECS_TASK_ARN, TASK_ARN);
        assert_attribute(&attrs, attr::AWS_ECS_TASK_FAMILY, "curltest");
        assert_attribute(&attrs, attr::AWS_ECS_TASK_REVISION, "26");
    }

    #[test]
    fn task_attributes_qualify_a_bare_cluster_name() {
        let task = task();
        let task_ref = NaiveArn::parse(&task.task_arn).expect("the task ARN parses");
        let attrs = task_attributes(&task, &task_ref);

        assert_attribute(
            &attrs,
            attr::AWS_ECS_CLUSTER_ARN,
            "arn:aws:ecs:us-west-2:111122223333:cluster/default",
        );
    }

    #[test]
    fn task_attributes_lowercase_the_launch_type() {
        let task = task();
        let task_ref = NaiveArn::parse(&task.task_arn).expect("the task ARN parses");
        let attrs = task_attributes(&task, &task_ref);

        assert_attribute(&attrs, attr::AWS_ECS_LAUNCHTYPE, "ec2");
    }

    #[test]
    fn container_attributes_describe_the_container_and_its_logs() {
        let task = task();
        let task_ref = NaiveArn::parse(&task.task_arn).expect("the task ARN parses");
        let attrs = container_attributes(&container(), &task_ref);

        let container_arn =
            "arn:aws:ecs:us-west-2:111122223333:container/acfcddf8-14b5-4d2a-9c1c-4b5e0ee2b8b4";
        assert_attribute(&attrs, attr::CLOUD_RESOURCE_ID, container_arn);
        assert_attribute(&attrs, attr::AWS_ECS_CONTAINER_ARN, container_arn);

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
    }

    #[test]
    fn container_attributes_skip_the_logs_of_another_driver() {
        let task = task();
        let task_ref = NaiveArn::parse(&task.task_arn).expect("the task ARN parses");

        let mut container = container();
        container.log_driver = "json-file".to_string();
        let attrs = container_attributes(&container, &task_ref);

        assert_eq!(attribute(&attrs, attr::AWS_LOG_GROUP_NAMES), None);
        assert_eq!(attribute(&attrs, attr::AWS_LOG_STREAM_NAMES), None);
    }

    #[test]
    fn log_attributes_fall_back_to_the_container_region() {
        let task = task();
        let task_ref = NaiveArn::parse(&task.task_arn).expect("the task ARN parses");

        let container_arn = "arn:aws:ecs:eu-central-1:111122223333:container/abc";
        let container_ref = NaiveArn::parse(container_arn).expect("the container ARN parses");

        let options = LogOptions {
            group: "/ecs/metadata".to_string(),
            stream: "ecs/curl/abc".to_string(),
            region: String::new(),
        };
        let attrs = log_attributes(&options, Some(&container_ref), &task_ref);

        assert_attribute(
            &attrs,
            attr::AWS_LOG_GROUP_ARNS,
            "arn:aws:logs:eu-central-1:111122223333:log-group:/ecs/metadata:*",
        );
    }

    #[test]
    fn log_attributes_need_both_a_group_and_a_stream() {
        let task = task();
        let task_ref = NaiveArn::parse(&task.task_arn).expect("the task ARN parses");

        let options = LogOptions {
            group: "/ecs/metadata".to_string(),
            ..Default::default()
        };

        assert!(log_attributes(&options, None, &task_ref).is_empty());
    }

    #[test]
    fn qualify_leaves_a_full_arn_alone() {
        let task_ref = NaiveArn::parse(TASK_ARN).expect("the task ARN parses");
        let arn = "arn:aws:ecs:us-east-1:444455556666:cluster/other";

        assert_eq!(qualify(arn, "cluster", &task_ref), arn);
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
