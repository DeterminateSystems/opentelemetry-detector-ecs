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
//! The same description is a structure of its own, [`EcsMetadata`], for a
//! program that wants the task in fields rather than in attributes, or wants it
//! as JSON:
//!
//! ```
//! use opentelemetry_detector_ecs::EcsMetadata;
//!
//! if let Some(metadata) = EcsMetadata::detect() {
//!     println!("{}", serde_json::to_string(metadata).expect("it serializes"));
//! }
//! ```
//!
//! On ECS Anywhere it also reports the Systems Manager managed instance the
//! task runs on, which costs three API calls and the permissions to make them.
//! See [`EcsResourceDetector`] for what those are. The `anywhere` cargo
//! feature, on by default, carries that lookup; turning it off drops the
//! lookup and the AWS SDK dependencies behind it.
//!
//! Detection blocks for up to two seconds while it queries the metadata
//! endpoint, and five more on ECS Anywhere. It reports whatever it has gathered
//! so far if the endpoint or the APIs answer slowly, partially, or not at all.
//! It does so once: the first detection keeps its answer in memory and every
//! detection after it, from whatever thread, costs no more than a clone.
//!
//! [conventions]: https://opentelemetry.io/docs/specs/semconv/resource/cloud-provider/aws/ecs/
//
// Ported from the Go detector in opentelemetry-go-contrib, which is also
// licensed under the Apache License, Version 2.0:
// https://github.com/open-telemetry/opentelemetry-go-contrib/blob/4610324d288f2b56faf237d67b85678f8e6de387/detectors/aws/ecs/ecs.go

#![deny(missing_docs)]

use std::sync::OnceLock;

use opentelemetry_sdk::resource::{Resource, ResourceDetector};

#[cfg(feature = "anywhere")]
mod anywhere;
pub mod metadata;

pub use crate::metadata::EcsMetadata;

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
        CONTAINER_NAME, HOST_ID,
    };

    /// The prefix the detector puts in front of a managed instance tag.
    ///
    /// A task on ECS Anywhere runs on a host the ECS agent registered as a
    /// Systems Manager managed instance. The detector reports every tag on that
    /// instance, naming a tag `Env` as `aws.ecs.container_instance.tag.Env`.
    /// The semantic conventions name no such attribute, so the key is this
    /// crate's own.
    pub const AWS_ECS_CONTAINER_INSTANCE_TAG_PREFIX: &str = "aws.ecs.container_instance.tag.";
}

/// The resource the first detection built, kept for every detection after it.
static RESOURCE: OnceLock<Resource> = OnceLock::new();

/// Describes the Amazon ECS task the current process belongs to.
///
/// The detector recognizes ECS by the `ECS_CONTAINER_METADATA_URI_V4` and
/// `ECS_CONTAINER_METADATA_URI` environment variables. Given the v4 endpoint it
/// reports the full set of attributes; given only v3 it reports the container
/// name and ID; given neither it reports an empty [`Resource`].
///
/// A task whose launch type is `EXTERNAL` runs on ECS Anywhere, and the
/// detector goes on to name the Systems Manager managed instance underneath it.
/// That takes the credentials the environment supplies and three permissions on
/// the task role:
///
/// - `ecs:DescribeTasks`
/// - `ecs:DescribeContainerInstances`
/// - `ssm:ListTagsForResource`
///
/// Each one the role lacks costs the attributes behind it and leaves a notice
/// on standard error. Detection succeeds regardless. The lookup exists under
/// the `anywhere` cargo feature, which is on by default.
///
/// The detector reads all of this once and answers from memory afterwards, so a
/// program can register it with as many providers as it has. [`EcsMetadata`]
/// holds the same description in fields, and says more about the cache.
///
/// See the [crate documentation](crate) for an example.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EcsResourceDetector;

impl EcsResourceDetector {
    /// Builds a detector.
    pub fn new() -> Self {
        Self
    }
}

impl ResourceDetector for EcsResourceDetector {
    fn detect(&self) -> Resource {
        // A `Resource` is a handle to shared attributes, so the clone the
        // caller gets is a pointer and a count.
        RESOURCE
            .get_or_init(|| {
                EcsMetadata::detect()
                    .map(EcsMetadata::resource)
                    .unwrap_or_else(|| Resource::builder_empty().build())
            })
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn detects_once_and_answers_from_memory() {
        let first = EcsResourceDetector.detect();
        assert!(RESOURCE.get().is_some(), "the first call filled the cache");

        assert_eq!(EcsResourceDetector.detect(), first);
    }
}
