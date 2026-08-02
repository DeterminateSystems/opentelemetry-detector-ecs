// Ported from the Go detector in opentelemetry-go-contrib, which is also
// licensed under the Apache License, Version 2.0:
// https://github.com/open-telemetry/opentelemetry-go-contrib/blob/4610324d288f2b56faf237d67b85678f8e6de387/detectors/aws/ecs/ecs.go

use std::sync::OnceLock;
use std::time::Duration;

use arn::naive::NaiveArn;
use opentelemetry::KeyValue;
use opentelemetry_sdk::resource::{Resource, ResourceDetector};
use opentelemetry_semantic_conventions::resource as sc;
use regex::Regex;
use serde::Deserialize;

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

pub struct EcsResourceDetector;

impl EcsResourceDetector {
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
            KeyValue::new(sc::CLOUD_PROVIDER, "aws"),
            KeyValue::new(sc::CLOUD_PLATFORM, "aws_ecs"),
        ];

        if let Ok(name) = std::env::var("HOSTNAME").or_else(|_| hostname_fallback()) {
            attrs.push(KeyValue::new(sc::CONTAINER_NAME, name));
        }
        if let Some(id) = Self::container_id() {
            attrs.push(KeyValue::new(sc::CONTAINER_ID, id));
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
        attrs.push(KeyValue::new(sc::CLOUD_REGION, region.to_string()));
    }
    if let Some(account) = task_ref.account_id {
        attrs.push(KeyValue::new(sc::CLOUD_ACCOUNT_ID, account.to_string()));
    }
    if !task.availability_zone.is_empty() {
        attrs.push(KeyValue::new(
            sc::CLOUD_AVAILABILITY_ZONE,
            task.availability_zone.clone(),
        ));
    }

    attrs.push(KeyValue::new(
        "aws.ecs.cluster.arn",
        qualify(&task.cluster, "cluster", task_ref),
    ));
    attrs.push(KeyValue::new(
        "aws.ecs.launchtype",
        task.launch_type.to_lowercase(),
    ));
    attrs.push(KeyValue::new("aws.ecs.task.arn", task.task_arn.clone()));
    attrs.push(KeyValue::new("aws.ecs.task.family", task.family.clone()));
    attrs.push(KeyValue::new(
        "aws.ecs.task.revision",
        task.revision.clone(),
    ));

    attrs
}

/// Maps container metadata, including its log configuration, onto resource
/// attributes.
fn container_attributes(container: &ContainerMetadataV4, task_ref: &NaiveArn) -> Vec<KeyValue> {
    let mut attrs = Vec::new();

    let container_arn = qualify(&container.container_arn, "container", task_ref);

    if container.log_driver == "awslogs" {
        if let Some(options) = &container.log_options {
            let container_ref = NaiveArn::parse(&container_arn).ok();
            attrs.extend(log_attributes(options, container_ref.as_ref(), task_ref));
        }
    }

    attrs.push(KeyValue::new(sc::CLOUD_RESOURCE_ID, container_arn.clone()));
    attrs.push(KeyValue::new("aws.ecs.container.arn", container_arn));

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
        KeyValue::new("aws.log.group.names", group.clone()),
        KeyValue::new(
            "aws.log.group.arns",
            format!("arn:{partition}:logs:{region}:{account}:log-group:{group}:*"),
        ),
        KeyValue::new("aws.log.stream.names", stream.clone()),
        KeyValue::new(
            "aws.log.stream.arns",
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
    use opentelemetry::Key;

    use super::*;

    #[test]
    fn detected_resource_does_not_include_default_service_name() {
        let resource =
            EcsResourceDetector::detected_resource(vec![KeyValue::new(sc::CLOUD_PROVIDER, "aws")]);

        assert_eq!(
            resource.get(&Key::new(sc::CLOUD_PROVIDER)),
            Some("aws".into())
        );
        assert_eq!(resource.get(&Key::new("service.name")), None);
    }
}
