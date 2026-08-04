# `opentelemetry-detector-ecs`

[![crates.io](https://img.shields.io/crates/v/opentelemetry-detector-ecs.svg)](https://crates.io/crates/opentelemetry-detector-ecs)
[![docs.rs](https://docs.rs/opentelemetry-detector-ecs/badge.svg)](https://docs.rs/opentelemetry-detector-ecs)

An [OpenTelemetry](https://opentelemetry.io/) resource detector for [Amazon ECS](https://aws.amazon.com/ecs/).

The detector reads the ECS task metadata endpoint and reports the cloud, container, task, and log attributes named by the [semantic conventions for ECS][conventions].
On ECS Anywhere it also names the managed instance the task runs on and the tags it carries.
Anywhere else it reports nothing, so a program that also runs outside ECS can register it unconditionally.

## Usage

```rust
use opentelemetry_detector_ecs::EcsResourceDetector;
use opentelemetry_sdk::Resource;

let resource = Resource::builder()
    .with_detector(Box::new(EcsResourceDetector))
    .build();
```

Detection blocks for up to two seconds while it queries the metadata endpoint, and five more on ECS Anywhere.
It reports whatever it has gathered so far if the endpoint or the APIs answer slowly, partially, or not at all.

## Attributes

Every key below is a public constant in the crate's `attributes` module, re-exported from [`opentelemetry-semantic-conventions`](https://docs.rs/opentelemetry-semantic-conventions).
The one exception is `aws.ecs.container_instance.tag.*`, which the semantic conventions do not name; the crate defines that prefix itself.

| Attribute                                                          | Source                                     |
| ------------------------------------------------------------------ | ------------------------------------------ |
| `cloud.provider`, `cloud.platform`                                 | Constant                                   |
| `container.name`                                                   | `$HOSTNAME`, or `/proc/sys/kernel/hostname` |
| `container.id`                                                     | `/proc/self/cgroup`                        |
| `cloud.region`, `cloud.account.id`                                 | The task ARN                               |
| `cloud.availability_zone`                                          | The task metadata                          |
| `aws.ecs.cluster.arn`, `aws.ecs.launchtype`                        | The task metadata                          |
| `aws.ecs.task.arn`, `aws.ecs.task.family`, `aws.ecs.task.revision` | The task metadata                          |
| `cloud.resource_id`, `aws.ecs.container.arn`                       | The container metadata                     |
| `aws.log.group.*`, `aws.log.stream.*`                              | The `awslogs` log driver options           |
| `host.id`                                                          | The ECS APIs                               |
| `aws.ecs.container_instance.tag.*`                                 | The ECS and Systems Manager APIs           |

The rows from `cloud.region` down need the v4 metadata endpoint.
A task on the v3 endpoint gets the container name and ID alone.

The last two rows need ECS Anywhere besides.
`aws.ecs.container_instance.tag.*` names one attribute for each tag on the managed instance, so a tag `Env` arrives as `aws.ecs.container_instance.tag.Env`.

## ECS Anywhere

A task whose launch type is `EXTERNAL` runs on hardware of the customer's own, which the ECS agent registered as a [Systems Manager](https://docs.aws.amazon.com/systems-manager/) managed instance.
The instance carries an `mi-` ID and whatever tags its operator gave it, and neither reaches the task metadata endpoint.
The detector therefore takes the credentials the environment supplies and asks the APIs: `ecs:DescribeTasks` names the container instance holding the task, `ecs:DescribeContainerInstances` turns that into an `mi-` ID, and `ssm:ListTagsForResource` lists the tags on it.

The task role needs those three permissions. [iam/detector-policy.json](./iam/detector-policy.json) grants them and nothing else; [iam/README.md](./iam/README.md) explains how.

Every permission the role lacks costs the attributes behind it and leaves a line on standard error naming what went missing:

```console
opentelemetry-detector-ecs: the task role cannot call ssm:ListTagsForResource
```

Detection succeeds regardless, so a role short of `ssm:ListTagsForResource` still reports `host.id`.
A task on any other launch type skips the three calls altogether.

## Development

```console
$ nix develop
$ just
```

The flake supplies the tools and the [Justfile](./Justfile) decides what to run with them, so `just ci` runs exactly what CI runs: the tests, Clippy, rustdoc, the formatting and spelling checks, and a packaging dry run.
Run `just` alone to list the recipes, or `just fmt` to format the tree in place.

## Releasing

Raise the version in `Cargo.toml`, move the `Unreleased` heading in [CHANGELOG.md](./CHANGELOG.md) down to it, and tag the merged commit `v<version>`.
Pushing the tag publishes the crate to crates.io and opens a GitHub release.
The workflow refuses a tag that disagrees with the manifest.

## License

Apache 2.0. See [LICENSE](./LICENSE).

The detector is a port of the [ECS detector in `opentelemetry-go-contrib`][go-contrib], likewise Apache 2.0.

[conventions]: https://opentelemetry.io/docs/specs/semconv/resource/cloud-provider/aws/ecs/
[go-contrib]: https://github.com/open-telemetry/opentelemetry-go-contrib/tree/main/detectors/aws/ecs
