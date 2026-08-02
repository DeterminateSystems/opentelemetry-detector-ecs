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

    /// Turns a bare `cluster-name` into a full ARN, using partition/region/account
    /// borrowed from an already-qualified sibling ARN (task or container).
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
}

impl ResourceDetector for EcsResourceDetector {
    fn detect(&self) -> Resource {
        let v4 = std::env::var("ECS_CONTAINER_METADATA_URI_V4").ok();
        let has_v3 = std::env::var("ECS_CONTAINER_METADATA_URI").is_ok();
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
        if let Some(cid) = Self::container_id() {
            attrs.push(KeyValue::new(sc::CONTAINER_ID, cid));
        }

        let Some(uri) = v4 else {
            return Self::detected_resource(attrs);
        };

        let client = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
        {
            Ok(c) => c,
            Err(_e) => {
                return Self::detected_resource(attrs);
            }
        };

        let task: Option<TaskMetadataV4> = client
            .get(format!("{uri}/task"))
            .send()
            .ok()
            .and_then(|r| r.json().ok());

        if let Some(task) = task {
            let Ok(task_ref) = NaiveArn::parse(&task.task_arn) else {
                return Self::detected_resource(attrs);
            };

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

            let cluster_arn = Self::qualify(&task.cluster, "cluster", &task_ref);

            attrs.push(KeyValue::new("aws.ecs.cluster.arn", cluster_arn));
            attrs.push(KeyValue::new(
                "aws.ecs.launchtype",
                task.launch_type.to_lowercase(),
            ));
            attrs.push(KeyValue::new("aws.ecs.task.arn", task.task_arn.clone()));
            attrs.push(KeyValue::new("aws.ecs.task.family", task.family));
            attrs.push(KeyValue::new("aws.ecs.task.revision", task.revision));

            let container: Option<ContainerMetadataV4> =
                client.get(&uri).send().ok().and_then(|r| r.json().ok());

            if let Some(container) = container {
                let container_arn = Self::qualify(&container.container_arn, "container", &task_ref);

                if container.log_driver == "awslogs" {
                    if let Some(opts) = &container.log_options {
                        if !opts.group.is_empty() && !opts.stream.is_empty() {
                            let container_ref = NaiveArn::parse(&container_arn).ok();
                            let partition = container_ref
                                .as_ref()
                                .map_or(task_ref.partition, |c| c.partition);
                            let account = container_ref
                                .as_ref()
                                .and_then(|c| c.account_id)
                                .or(task_ref.account_id)
                                .unwrap_or_default();
                            let region = if !opts.region.is_empty() {
                                opts.region.as_str()
                            } else {
                                container_ref
                                    .as_ref()
                                    .and_then(|c| c.region)
                                    .or(task_ref.region)
                                    .unwrap_or_default()
                            };

                            attrs.push(KeyValue::new("aws.log.group.names", opts.group.clone()));
                            attrs.push(KeyValue::new(
                                "aws.log.group.arns",
                                format!(
                                    "arn:{partition}:logs:{region}:{account}:log-group:{}:*",
                                    opts.group
                                ),
                            ));
                            attrs.push(KeyValue::new("aws.log.stream.names", opts.stream.clone()));
                            attrs.push(KeyValue::new(
                                "aws.log.stream.arns",
                                format!(
                            "arn:{partition}:logs:{region}:{account}:log-group:{}:log-stream:{}",
                            opts.group, opts.stream
                        ),
                            ));
                        }
                    }
                }

                attrs.push(KeyValue::new(sc::CLOUD_RESOURCE_ID, container_arn.clone()));
                attrs.push(KeyValue::new("aws.ecs.container.arn", container_arn));
            }
        }

        Self::detected_resource(attrs)
    }
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
