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

It pays that cost once.
The first detection keeps its answer in memory, and every detection after it, from whatever thread, costs no more than a clone.
A program can therefore give the detector to as many providers as it has.
The answer outlives the environment it came from, so a variable set or a tag changed afterwards goes unnoticed.

## The detected task

`EcsMetadata` is the same description in fields rather than in attributes, for a program that wants the task itself:

```rust
use opentelemetry_detector_ecs::EcsMetadata;

if let Some(metadata) = EcsMetadata::detect() {
    println!("{}", serde_json::to_string_pretty(metadata).expect("it serializes"));
    println!("{:?}", metadata.task.as_ref().map(|task| &task.family));
}
```

`EcsMetadata::detect` reports `None` off ECS and reads from the same cache the detector does.
The type serializes and deserializes, and leaves out whatever the task does not have:

```json
{
  "container": {
    "name": "ip-10-0-0-1.us-west-2.compute.internal",
    "id": "43481a6ce4842eec8fe72fc28500c6b52edcc0917f105b83379f88cac1ff3946",
    "arn": "arn:aws:ecs:us-west-2:111122223333:container/acfcddf8-14b5-4d2a-9c1c-4b5e0ee2b8b4"
  },
  "cloud": {
    "region": "us-west-2",
    "account_id": "111122223333",
    "availability_zone": "us-west-2d"
  },
  "task": {
    "arn": "arn:aws:ecs:us-west-2:111122223333:task/default/158d1c8083dd49d6b527399fd6414f5c",
    "cluster_arn": "arn:aws:ecs:us-west-2:111122223333:cluster/default",
    "family": "curltest",
    "revision": "26",
    "launch_type": "EC2"
  },
  "logs": {
    "group_name": "/ecs/metadata",
    "group_arn": "arn:aws:logs:us-west-2:111122223333:log-group:/ecs/metadata:*",
    "stream_name": "ecs/curl/8f03e41243824aea923aca126495f665",
    "stream_arn": "arn:aws:logs:us-west-2:111122223333:log-group:/ecs/metadata:log-stream:ecs/curl/8f03e41243824aea923aca126495f665"
  }
}
```

`EcsMetadata::attributes` turns that into the table below, and `EcsMetadata::resource` into the resource the detector reports.
`EcsMetadata::detect_uncached` reads it all again, at the cost `detect` pays only once.

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
What the lookup found is the `managed_instance` field of `EcsMetadata`, which holds the `mi-` ID and the tags as a map.

The lookup lives under the `anywhere` cargo feature, which is on by default.
Turning it off leaves the metadata-endpoint attributes and drops the AWS SDK dependencies:

```toml
opentelemetry-detector-ecs = { version = "0.3.0", default-features = false }
```

## FIPS

The `fips` feature serves the detector's AWS calls with FIPS-validated crypto:

```toml
opentelemetry-detector-ecs = { version = "0.3.0", features = ["fips"] }
```

The feature puts the SDK clients' TLS stack on [`aws-lc-fips-sys`](https://crates.io/crates/aws-lc-fips-sys), and because Cargo builds one `aws-lc-rs` for the whole binary, every other `aws-lc` caller in the program gets the FIPS module too.
Compiling it builds AWS-LC's FIPS module from source, which takes cmake, Go, and Perl.
The clients exist under the `anywhere` feature, so `fips` without it has no crypto to swap and compiles none.

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
