# `opentelemetry-detector-ecs`

[![crates.io](https://img.shields.io/crates/v/opentelemetry-detector-ecs.svg)](https://crates.io/crates/opentelemetry-detector-ecs)
[![docs.rs](https://docs.rs/opentelemetry-detector-ecs/badge.svg)](https://docs.rs/opentelemetry-detector-ecs)

An [OpenTelemetry](https://opentelemetry.io/) resource detector for [Amazon ECS](https://aws.amazon.com/ecs/).

The detector reads the ECS task metadata endpoint and reports the cloud, container, task, and log attributes named by the [semantic conventions for ECS][conventions].
Anywhere else it reports nothing, so a program that also runs outside ECS can register it unconditionally.

## Usage

```rust
use opentelemetry_detector_ecs::EcsResourceDetector;
use opentelemetry_sdk::Resource;

let resource = Resource::builder()
    .with_detector(Box::new(EcsResourceDetector))
    .build();
```

Detection blocks for up to two seconds while it queries the metadata endpoint, and it reports whatever it has gathered so far if the endpoint answers slowly, partially, or not at all.

## Attributes

Every key below is a public constant in the crate's `attributes` module, re-exported from [`opentelemetry-semantic-conventions`](https://docs.rs/opentelemetry-semantic-conventions).

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

The last four rows need the v4 metadata endpoint.
A task on the v3 endpoint gets the container name and ID alone.

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
